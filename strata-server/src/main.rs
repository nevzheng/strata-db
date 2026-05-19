use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use tokio::net::TcpListener;

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

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let listener = TcpListener::bind(cli.listen).await?;
    println!(
        "strata-server listening on {} (data: {})",
        cli.listen,
        cli.data_dir.display()
    );

    loop {
        let (_socket, peer) = listener.accept().await?;
        println!("accepted connection from {peer} (no pgwire handler yet)");
    }
}
