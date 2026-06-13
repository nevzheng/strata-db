use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use sqllogictest::{AsyncDB, DBOutput, DefaultColumnType, Runner};
use tempfile::TempDir;
use thiserror::Error;
use tokio_postgres::{Client, Config, NoTls, SimpleQueryMessage};
use walkdir::WalkDir;

const SLT_EXTENSION: &str = "slt";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Per-query timeout. Defends against a server that accepts the message but
/// never replies — without it, the harness would hang forever.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const STRATA_USER: &str = "strata";
const STRATA_DB: &str = "strata";

/// Run sqllogictest `.slt` files against a strata-server spawned for the run.
#[derive(Parser, Debug)]
#[command(version, about = "Run .slt spec tests against strata")]
struct Cli {
    /// File or directory containing `.slt` files. Directories are walked recursively.
    path: PathBuf,
}

#[derive(Debug, Error)]
enum StrataError {
    #[error("postgres: {0}")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("query exceeded {0:?}")]
    Timeout(Duration),
}

/// Client side of one connection to a running strata-server.
struct StrataDb {
    client: Client,
}

impl StrataDb {
    async fn connect(addr: SocketAddr) -> Result<Self, StrataError> {
        let mut config = Config::new();
        config
            .host(addr.ip().to_string())
            .port(addr.port())
            .user(STRATA_USER)
            .dbname(STRATA_DB);
        let (client, connection) = config.connect(NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("strata connection dropped: {e}");
            }
        });
        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl AsyncDB for StrataDb {
    type Error = StrataError;
    type ColumnType = DefaultColumnType;

    async fn run(&mut self, sql: &str) -> Result<DBOutput<Self::ColumnType>, Self::Error> {
        let messages = tokio::time::timeout(QUERY_TIMEOUT, self.client.simple_query(sql))
            .await
            .map_err(|_| StrataError::Timeout(QUERY_TIMEOUT))??;

        let mut types: Vec<DefaultColumnType> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut affected: u64 = 0;
        let mut saw_row_description = false;

        for msg in messages {
            match msg {
                SimpleQueryMessage::RowDescription(cols) => {
                    saw_row_description = true;
                    // Simple-query results come back as text; we don't have
                    // pg type oids surfaced, so `Any` is the honest answer.
                    types = cols.iter().map(|_| DefaultColumnType::Any).collect();
                }
                SimpleQueryMessage::Row(row) => {
                    let values: Vec<String> = (0..row.len())
                        .map(|i| row.get(i).unwrap_or("").to_string())
                        .collect();
                    rows.push(values);
                }
                SimpleQueryMessage::CommandComplete(n) => {
                    affected += n;
                }
                _ => {}
            }
        }

        if saw_row_description {
            Ok(DBOutput::Rows { types, rows })
        } else {
            Ok(DBOutput::StatementComplete(affected))
        }
    }

    async fn shutdown(&mut self) {}

    fn engine_name(&self) -> &str {
        "strata"
    }

    async fn sleep(dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// strata-server child process plus the tempdir it writes into. Dropping this
/// kills the child (tokio's `kill_on_drop`) and cleans up the data dir.
struct TestServer {
    _child: tokio::process::Child,
    addr: SocketAddr,
    _data_dir: TempDir,
}

impl TestServer {
    async fn spawn() -> Result<Self> {
        let data_dir = TempDir::new().context("creating tempdir for strata data")?;
        let port = pick_free_port()?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let bin = locate_strata_server()?;

        let child = tokio::process::Command::new(&bin)
            .args(["--listen", &addr.to_string()])
            .arg("--data-dir")
            .arg(data_dir.path())
            .env("RUST_LOG", "error")
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", bin.display()))?;

        wait_for_ready(addr)
            .await
            .with_context(|| format!("strata-server at {addr} never became ready"))?;

        Ok(Self {
            _child: child,
            addr,
            _data_dir: data_dir,
        })
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("binding ephemeral port")?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

/// Locate the `strata-server` binary. Honors `STRATA_SERVER_BIN`, falls back
/// to a sibling of the current executable (the cargo `target/<profile>/` dir).
fn locate_strata_server() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("STRATA_SERVER_BIN") {
        let p = PathBuf::from(p);
        if !p.exists() {
            anyhow::bail!(
                "STRATA_SERVER_BIN points at non-existent path: {}",
                p.display()
            );
        }
        return Ok(p);
    }
    let current = std::env::current_exe().context("reading current_exe")?;
    let dir = current
        .parent()
        .context("current exe has no parent directory")?;
    let candidate = dir.join("strata-server");
    if candidate.exists() {
        return Ok(candidate);
    }
    anyhow::bail!(
        "strata-server binary not found at {}. \
         Run `cargo build -p strata-server` first, or set STRATA_SERVER_BIN.",
        candidate.display()
    )
}

/// Poll the address with a real pgwire handshake until one completes or the
/// timeout expires. TCP-connect alone would prove the listener is bound, not
/// that the server can answer.
async fn wait_for_ready(addr: SocketAddr) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut last_err: Option<tokio_postgres::Error> = None;
    while Instant::now() < deadline {
        let mut config = Config::new();
        config
            .host(addr.ip().to_string())
            .port(addr.port())
            .user(STRATA_USER)
            .dbname(STRATA_DB);
        match config.connect(NoTls).await {
            Ok((_client, connection)) => {
                tokio::spawn(connection);
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
            }
        }
    }
    match last_err {
        Some(e) => anyhow::bail!("timeout: {e}"),
        None => anyhow::bail!("timeout (no handshake attempt produced an error)"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli.path).await {
        Ok(count) => {
            println!("ran {count} spec file(s) successfully");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(path: &Path) -> Result<usize> {
    if !path.exists() {
        anyhow::bail!("path does not exist: {}", path.display());
    }
    let files = collect_slt_files(path);
    if files.is_empty() {
        anyhow::bail!("no .slt files found under {}", path.display());
    }

    let server = TestServer::spawn().await?;
    let addr = server.addr();
    println!("spawned strata-server at {addr}");

    for file in &files {
        println!("running {}", file.display());
        let mut runner = Runner::new(move || async move { StrataDb::connect(addr).await });
        runner
            .run_file_async(file)
            .await
            .with_context(|| format!("failed running {}", file.display()))?;
    }

    drop(server);
    Ok(files.len())
}

fn collect_slt_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return if path.extension().and_then(|e| e.to_str()) == Some(SLT_EXTENSION) {
            vec![path.to_path_buf()]
        } else {
            vec![]
        };
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(SLT_EXTENSION))
        .collect()
}
