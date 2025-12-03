mod client;
mod pb;
mod server;

use clap::{Parser, Subcommand};
use indicatif::MultiProgress;
use std::sync::Arc;
use tonic::transport::Server;
use tracing_subscriber::FmtSubscriber;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
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
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let cli = Cli::parse();

    match cli.command {
        Commands::Server { addr, storage } => {
            println!("Starting Cortex Server on {}", addr);
            std::fs::create_dir_all(&storage)?;

            let addr = addr.parse()?;

            let state = Arc::new(server::ServerState {
                multi_bar: MultiProgress::new(),
            });

            let service = server::MyCortexService {
                state,
                storage_dir: storage,
            };

            Server::builder()
                .add_service(pb::cortex::cortex_service_server::CortexServiceServer::new(
                    service,
                ))
                .serve(addr)
                .await?;
        }
        Commands::Client { connect } => {
            client::run_client(connect).await?;
        }
    }

    Ok(())
}
