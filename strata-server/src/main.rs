use std::fmt::Debug;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use futures::Sink;
use pgwire::api::auth::StartupHandler;
use pgwire::api::auth::noop::NoopStartupHandler;
use pgwire::api::query::SimpleQueryHandler;
use pgwire::api::results::{Response, Tag};
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, PgWireServerHandlers};
use pgwire::error::{PgWireError, PgWireResult};
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use pgwire::tokio::process_socket;
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

/// pgwire handler stub: completes the startup handshake and acknowledges every
/// query with an empty `OK` tag. Replace `do_query` with real parse/plan/execute
/// against `strata-db` when that layer lands.
struct Processor;

#[async_trait]
impl NoopStartupHandler for Processor {
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
impl SimpleQueryHandler for Processor {
    async fn do_query<C>(&self, _client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + ClientPortalStore + Unpin + Send + Sync,
        C::PortalStore: PortalStore,
    {
        info!(query = %query, "received query");
        Ok(vec![Response::Execution(Tag::new("OK"))])
    }
}

struct HandlerFactory {
    processor: Arc<Processor>,
}

impl PgWireServerHandlers for HandlerFactory {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.processor.clone()
    }

    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        self.processor.clone()
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

    let factory = Arc::new(HandlerFactory {
        processor: Arc::new(Processor),
    });

    loop {
        let (socket, peer) = listener.accept().await?;
        let factory = factory.clone();
        let span = info_span!("conn", peer = %peer);
        tokio::spawn(
            async move {
                info!("accepted");
                if let Err(e) = process_socket(socket, None, factory).await {
                    error!(error = %e, "connection error");
                }
            }
            .instrument(span),
        );
    }
}
