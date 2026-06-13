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

/// A query to run on the engine thread, with a channel to send the result back.
struct Request {
    sql: String,
    reply: tokio::sync::oneshot::Sender<Result<Vec<StatementResult>, QueryError>>,
}

/// Handle to the storage engine, which lives on its own dedicated thread.
///
/// The engine (and the `Db` that owns it) is single-threaded — `Rc`/`RefCell`
/// all the way down — so it must never move between threads. Instead of sharing
/// it, we pin it to one thread and talk to it over a channel: only the `sql`
/// string and the result rows (both `Send`) cross the boundary. The engine is
/// not, and should not be, `Send`.
struct Backend {
    engine: tokio::sync::mpsc::UnboundedSender<Request>,
}

impl Backend {
    fn open(data_dir: &Path) -> Result<Self, QueryError> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), QueryError>>();
        let data_dir = data_dir.to_path_buf();

        std::thread::Builder::new()
            .name("strata-engine".into())
            .spawn(move || {
                // Open the Db *here* so it is created and used entirely on this
                // thread, never crossing a thread boundary.
                let db = match Db::open(&data_dir) {
                    Ok(db) => {
                        let _ = ready_tx.send(Ok(()));
                        db
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                // Serve queries one at a time until every sender is dropped.
                while let Some(req) = rx.blocking_recv() {
                    let result = run_query(&db, &req.sql);
                    let _ = req.reply.send(result);
                }
            })
            .expect("spawn engine thread");

        // Surface a failure to open the Db as the open() error.
        ready_rx.recv().expect("engine thread reports readiness")?;
        Ok(Self { engine: tx })
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

        // Hand the query to the engine thread and await its result. The engine
        // never leaves its thread; only the sql and the rows cross the channel.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.engine
            .send(Request {
                sql: query.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| PgWireError::ApiError("engine thread has stopped".into()))?;

        let results = match reply_rx.await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(to_pgwire_error(&e)),
            Err(_) => return Err(PgWireError::ApiError("engine dropped the query".into())),
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
    match root {
        PlanNode::Project { expressions, .. } => expressions.len(),
        // LIMIT / OFFSET wrap the projection; look through them.
        PlanNode::Limit { input, .. } | PlanNode::Offset { input, .. } => output_columns(input),
        _ => 0,
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
        Value::Date(d) => Some(strata_db::storage::temporal::format_date(*d)),
        Value::Timestamp(t) => Some(strata_db::storage::temporal::format_timestamptz(*t)),
        // Render each at its native width so the shortest round-tripping
        // form is preserved (widening an f32 to f64 would lengthen it).
        Value::Float32(f) => Some(float_text(
            f.is_finite(),
            f.is_nan(),
            *f >= 0.0,
            f.to_string(),
        )),
        Value::Float64(f) => Some(float_text(
            f.is_finite(),
            f.is_nan(),
            *f >= 0.0,
            f.to_string(),
        )),
        Value::Numeric(d) => Some(d.to_string()),
    }
}

/// Render a float in Postgres style: finite values use the shortest
/// round-tripping form (`finite_repr`), non-finite use `Infinity` /
/// `-Infinity` / `NaN`.
fn float_text(finite: bool, is_nan: bool, nonneg: bool, finite_repr: String) -> String {
    if finite {
        finite_repr
    } else if is_nan {
        "NaN".into()
    } else if nonneg {
        "Infinity".into()
    } else {
        "-Infinity".into()
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
        QueryError::Type(_) => "42804",        // datatype_mismatch
        QueryError::Storage(e) if e.is_exhausted() => "53000", // insufficient_resources
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
