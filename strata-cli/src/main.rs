use std::borrow::Cow;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use reedline::{
    FileBackedHistory, Prompt, PromptEditMode, PromptHistorySearch, Reedline, Signal,
    ValidationResult, Validator,
};
use tokio_postgres::{Client, Config, NoTls, SimpleQueryMessage};

const DEFAULT_HOST: &str = "localhost";
const DEFAULT_PORT: u16 = 5433;
const DEFAULT_USER: &str = "strata";
const DEFAULT_DATABASE: &str = "strata";
const HISTORY_CAPACITY: usize = 1000;

#[derive(Parser, Debug)]
#[command(version, about = "Interactive client for strata-server")]
struct Cli {
    #[arg(long, default_value = DEFAULT_HOST)]
    host: String,

    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,

    #[arg(long, default_value = DEFAULT_USER)]
    user: String,

    #[arg(long, default_value = DEFAULT_DATABASE)]
    database: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Drop into an interactive SQL shell. Default when no subcommand is given.
    Shell,
    /// Run a single SQL command and exit.
    Query { sql: String },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let mut config = Config::new();
    config
        .host(&cli.host)
        .port(cli.port)
        .user(&cli.user)
        .dbname(&cli.database);

    let (client, connection) = match config.connect(NoTls).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("connection failed: {e}");
            std::process::exit(1);
        }
    };

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {e}");
        }
    });

    match cli.command.unwrap_or(Command::Shell) {
        Command::Shell => run_shell(&client, &cli.host, cli.port).await,
        Command::Query { sql } => run_query(&client, &sql).await,
    }
}

async fn run_query(client: &Client, sql: &str) {
    match client.simple_query(sql).await {
        Ok(messages) => print_messages(&messages),
        Err(e) => {
            eprintln!("query failed: {e}");
            std::process::exit(1);
        }
    }
}

async fn run_shell(client: &Client, host: &str, port: u16) {
    println!("connected to {host}:{port}");
    println!("type SQL terminated by ';'. Ctrl-D to exit.\n");

    let mut editor = build_editor();
    let prompt = StrataPrompt;

    loop {
        let signal = tokio::task::block_in_place(|| editor.read_line(&prompt));
        match signal {
            Ok(Signal::Success(buf)) => {
                let sql = buf.trim().trim_end_matches(';').trim();
                if sql.is_empty() {
                    continue;
                }
                match client.simple_query(sql).await {
                    Ok(messages) => print_messages(&messages),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }
}

fn build_editor() -> Reedline {
    let mut editor = Reedline::create().with_validator(Box::new(SemicolonValidator));
    if let Some(path) = history_path()
        && let Ok(history) = FileBackedHistory::with_file(HISTORY_CAPACITY, path)
    {
        editor = editor.with_history(Box::new(history));
    }
    editor
}

fn history_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".strata_history"))
}

/// Treats input as incomplete until it ends with `;`. Does not yet understand
/// `;` inside string literals or comments — paste valid SQL or be patient.
struct SemicolonValidator;

impl Validator for SemicolonValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if line.trim_end().ends_with(';') {
            ValidationResult::Complete
        } else {
            ValidationResult::Incomplete
        }
    }
}

struct StrataPrompt;

impl Prompt for StrataPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        "strata".into()
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        "".into()
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        "=> ".into()
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        "-> ".into()
    }
    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        "(search) ".into()
    }
}

fn print_messages(messages: &[SimpleQueryMessage]) {
    let mut header_printed = false;
    let mut row_count: u64 = 0;
    for msg in messages {
        match msg {
            SimpleQueryMessage::RowDescription(cols) => {
                let names: Vec<&str> = cols.iter().map(|c| c.name()).collect();
                println!("{}", names.join("\t"));
                header_printed = true;
                row_count = 0;
            }
            SimpleQueryMessage::Row(row) => {
                let values: Vec<&str> = (0..row.len()).map(|i| row.get(i).unwrap_or("")).collect();
                println!("{}", values.join("\t"));
                row_count += 1;
            }
            SimpleQueryMessage::CommandComplete(_) => {
                if header_printed {
                    println!("({row_count} rows)");
                    header_printed = false;
                } else {
                    println!("OK");
                }
            }
            _ => {}
        }
    }
}
