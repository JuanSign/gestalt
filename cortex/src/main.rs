mod client;
mod pb;
mod server;

use clap::{Parser, Subcommand};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Server {
        #[arg(short, long, default_value = "0.0.0.0:50051")]
        addr: String,
        #[arg(short, long, default_value = "./cortex_storage")]
        storage: String,
    },
    Client {
        #[arg(short, long, default_value = "http://127.0.0.1:50051")]
        connect: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::ERROR)
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("failed to set subscriber");

    let cli = Cli::parse();

    match cli.command {
        Commands::Server { addr, storage } => {
            server::run_server(addr, storage).await?;
        }
        Commands::Client { connect } => {
            client::run_client(connect).await?;
        }
    }

    Ok(())
}
