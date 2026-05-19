use clap::Parser;
use tokio_postgres::{Config, NoTls, SimpleQueryMessage};

const DEFAULT_HOST: &str = "localhost";
const DEFAULT_PORT: u16 = 5433;
const DEFAULT_USER: &str = "strata";
const DEFAULT_DATABASE: &str = "strata";

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

    /// Run a single SQL command and exit (analogous to `psql -c`).
    #[arg(short = 'c', long = "command")]
    command: Option<String>,
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

    let Some(sql) = cli.command else {
        println!("connected to {}:{}", cli.host, cli.port);
        return;
    };

    match client.simple_query(&sql).await {
        Ok(messages) => print_messages(&messages),
        Err(e) => {
            eprintln!("query failed: {e}");
            std::process::exit(1);
        }
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
