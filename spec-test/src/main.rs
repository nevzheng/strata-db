use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use sqllogictest::{AsyncDB, DBOutput, DefaultColumnType, Runner};
use thiserror::Error;
use walkdir::WalkDir;

const SLT_EXTENSION: &str = "slt";

/// Run sqllogictest `.slt` files against a strata database.
///
/// Today the harness is wired to a stub DB that accepts every statement and
/// returns no rows. A `tokio-postgres`-backed implementation that spawns
/// `strata-server` lands next.
#[derive(Parser, Debug)]
#[command(version, about = "Run .slt spec tests against strata")]
struct Cli {
    /// File or directory containing `.slt` files. Directories are walked recursively.
    path: PathBuf,
}

#[derive(Debug, Error)]
#[error("spec-test stub db: {0}")]
struct StubError(String);

/// Stand-in for a real DB connection. Accepts every statement, returns no rows.
struct StubDb;

#[async_trait::async_trait]
impl AsyncDB for StubDb {
    type Error = StubError;
    type ColumnType = DefaultColumnType;

    async fn run(
        &mut self,
        _sql: &str,
    ) -> std::result::Result<DBOutput<Self::ColumnType>, Self::Error> {
        Ok(DBOutput::StatementComplete(0))
    }

    async fn shutdown(&mut self) {}

    fn engine_name(&self) -> &str {
        "strata-stub"
    }

    async fn sleep(dur: Duration) {
        tokio::time::sleep(dur).await;
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
    for file in &files {
        println!("running {}", file.display());
        let mut runner = Runner::new(|| async { Ok::<_, StubError>(StubDb) });
        runner
            .run_file_async(file)
            .await
            .with_context(|| format!("failed running {}", file.display()))?;
    }
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
