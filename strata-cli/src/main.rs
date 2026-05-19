use clap::Parser;
use tokio_postgres::{Config, NoTls};

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

    match config.connect(NoTls).await {
        Ok(_) => println!("connected to {}:{}", cli.host, cli.port),
        Err(e) => {
            eprintln!("connection failed: {e}");
            std::process::exit(1);
        }
    }
}
