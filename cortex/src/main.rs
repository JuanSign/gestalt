mod client;
mod pb;
mod server;

use crate::pb::cortex::ServerMessage;
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tonic::transport::Server;
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
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("failed to set subscriber");

    let cli = Cli::parse();

    match cli.command {
        Commands::Server { addr, storage } => {
            println!("Starting Cortex Server on {}", addr);
            std::fs::create_dir_all(&storage)?;

            let clients = Arc::new(Mutex::new(HashMap::new()));
            let state = Arc::new(server::ServerState {
                storage_dir: storage,
                clients: clients.clone(),
            });

            let service = server::MyCortexService { state };
            let addr_obj = addr.parse()?;

            tokio::spawn(async move {
                if let Err(e) = Server::builder()
                    .add_service(pb::cortex::cortex_service_server::CortexServiceServer::new(
                        service,
                    ))
                    .serve(addr_obj)
                    .await
                {
                    eprintln!("Server error: {}", e);
                }
            });

            println!("Server ready. Type 'send <client_id> <message>' or 'exit'");
            loop {
                print!("server> ");
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let parts: Vec<&str> = input.trim().split_whitespace().collect();
                if parts.is_empty() {
                    continue;
                }

                match parts[0] {
                    "send" => {
                        if parts.len() < 3 {
                            println!("Usage: send <client_id> <message>");
                            continue;
                        }
                        let id = parts[1];
                        let msg = parts[2..].join(" ");

                        let map = clients.lock().unwrap();
                        if let Some(tx) = map.get(id) {
                            let _ = tx.try_send(Ok(ServerMessage { text: msg }));
                            println!("Message sent to {}", id);
                        } else {
                            println!("Client {} not connected", id);
                        }
                    }
                    "exit" => break,
                    _ => println!("Unknown command"),
                }
            }
        }
        Commands::Client { connect } => {
            client::run_client(connect).await?;
        }
    }

    Ok(())
}
