use std::fmt::Debug;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use futures::Sink;
use pgwire::api::auth::StartupHandler;
use pgwire::api::auth::noop::NoopStartupHandler;
use pgwire::api::query::SimpleQueryHandler;
use pgwire::api::results::{DataRowEncoder, FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use pgwire::tokio::process_socket;
use strata_db::query::Planner;
use strata_db::query::executor::{ExecuteResult, Executor};
use strata_db::query::physical_plan::PlanNode;
use strata_db::query::volcano::Volcano;
use strata_db::{Db, QueryError, Tuple, Value};
use tokio::net::TcpListener;
use tracing::{Instrument, error, info, info_span};
use tracing_subscriber::EnvFilter;

const DEFAULT_LISTEN: &str = "127.0.0.1:5433";
const DEFAULT_DATA_DIR: &str = "./strata-data";

#[derive(Parser, Debug)]
#[command(version, about = "strata database server")]
struct Cli {
    #[arg(long, default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,

    #[arg(long, default_value = DEFAULT_DATA_DIR)]
    data_dir: PathBuf,
}

/// One per parsed statement in a simple-query batch.
enum StatementResult {
    Rows { columns: usize, rows: Vec<Tuple> },
    Affected(u64),
}

struct Backend {
    db: Arc<Db>,
}

impl Backend {
    fn open(data_dir: &Path) -> Result<Self, QueryError> {
        let db = Db::open(data_dir)?;
        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait]
impl NoopStartupHandler for Backend {
    async fn post_startup<C>(
        &self,
        _client: &mut C,
        _message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        info!("startup complete");
        Ok(())
    }
}

#[async_trait]
impl SimpleQueryHandler for Backend {
    async fn do_query<C>(&self, _client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + ClientPortalStore + Unpin + Send + Sync,
        C::PortalStore: PortalStore,
    {
        info!(query = %query, "received simple query");

        let db = self.db.clone();
        let sql = query.to_string();
        // The engine lock is sync; do plan + execute on a blocking task
        // so the tokio worker thread isn't pinned while we hold it.
        let join = tokio::task::spawn_blocking(move || run_query(&db, &sql)).await;

        let results = match join {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(to_pgwire_error(&e)),
            Err(e) => return Err(PgWireError::ApiError(Box::new(e))),
        };

        let mut responses = Vec::with_capacity(results.len());
        for sr in results {
            match sr {
                StatementResult::Rows { columns, rows } => {
                    let schema = build_schema(columns);
                    let data_rows: Vec<DataRow> = rows
                        .iter()
                        .map(|t| encode_row(&schema, t))
                        .collect::<PgWireResult<_>>()?;
                    responses.push(Response::Query(QueryResponse::new(
                        schema,
                        futures::stream::iter(data_rows.into_iter().map(Ok)),
                    )));
                }
                StatementResult::Affected(n) => {
                    responses.push(Response::Execution(Tag::new("OK").with_rows(n as usize)));
                }
            }
        }
        Ok(responses)
    }
}

fn run_query(db: &Db, sql: &str) -> Result<Vec<StatementResult>, QueryError> {
    let mut ctx = db.query_context();
    let planner = Planner::builder()
        .build()
        .expect("default planner builder fills every pass slot");
    let pq = planner.plan(sql, &ctx)?;
    let mut out = Vec::with_capacity(pq.physical.len());
    for plan in pq.physical {
        let columns = output_columns(&plan.root);
        match Volcano.execute(plan, &mut ctx)? {
            ExecuteResult::Rows(stream) => {
                let rows = stream.collect::<Result<Vec<_>, _>>()?;
                out.push(StatementResult::Rows { columns, rows });
            }
            ExecuteResult::Affected(n) => out.push(StatementResult::Affected(n)),
        }
    }
    Ok(out)
}

/// The binder wraps every SELECT in a `Project`, so column count is the
/// projection arity. Write plans (Insert/Delete) don't return rows.
fn output_columns(root: &PlanNode) -> usize {
    if let PlanNode::Project { expressions, .. } = root {
        expressions.len()
    } else {
        0
    }
}

fn build_schema(columns: usize) -> Arc<Vec<FieldInfo>> {
    Arc::new(
        (0..columns)
            .map(|_| FieldInfo::new("?column?".into(), None, None, Type::TEXT, FieldFormat::Text))
            .collect(),
    )
}

fn encode_row(schema: &Arc<Vec<FieldInfo>>, tuple: &Tuple) -> PgWireResult<DataRow> {
    let mut encoder = DataRowEncoder::new(schema.clone());
    for value in &tuple.values {
        let text = value_to_text(value);
        encoder.encode_field(&text)?;
    }
    Ok(encoder.take_row())
}

fn value_to_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(if *b { "t".into() } else { "f".into() }),
        Value::Int16(i) => Some(i.to_string()),
        Value::Int32(i) => Some(i.to_string()),
        Value::Int64(i) => Some(i.to_string()),
        Value::Text(s) => Some(s.clone()),
        Value::Bytes(b) => Some(format!("\\x{}", hex_encode(b))),
        Value::Json(j) => Some(j.to_string()),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn to_pgwire_error(e: &QueryError) -> PgWireError {
    // SQLSTATE codes per https://www.postgresql.org/docs/current/errcodes-appendix.html.
    let code = match e {
        QueryError::Parse(_) => "42601",       // syntax_error
        QueryError::Catalog(_) => "42P01",     // undefined_table (closest fit today)
        QueryError::Unsupported(_) => "0A000", // feature_not_supported
        QueryError::Storage(_) => "58000",     // system_error
        QueryError::Codec(_) | QueryError::Internal(_) => "XX000", // internal_error
    };
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".into(),
        code.into(),
        e.to_string(),
    )))
}

/// Bundle of handlers `pgwire` asks for on each new connection. Holds
/// the shared [`Backend`] and hands out clones of the same `Arc` — there
/// is no per-connection construction.
struct Server {
    backend: Arc<Backend>,
}

impl PgWireServerHandlers for Server {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.backend.clone()
    }

    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        self.backend.clone()
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let listener = TcpListener::bind(cli.listen).await?;
    info!(addr = %cli.listen, data_dir = %cli.data_dir.display(), "strata-server listening");

    let backend = Backend::open(&cli.data_dir).map_err(|e| {
        std::io::Error::other(format!("opening db at {}: {e}", cli.data_dir.display()))
    })?;
    let server = Arc::new(Server {
        backend: Arc::new(backend),
    });

    loop {
        let (socket, peer) = listener.accept().await?;
        let server = server.clone();
        let span = info_span!("connection", peer = %peer);
        tokio::spawn(
            async move {
                info!("connection received");
                if let Err(e) = process_socket(socket, None, server).await {
                    error!(error = %e, "connection error");
                }
                info!("connection closed");
            }
            .instrument(span),
        );
    }
}
