use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

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

fn main() {
    let cli = Cli::parse();
    println!("{cli:?}");
}
