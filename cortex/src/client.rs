use crate::pb::cortex::cortex_service_client::CortexServiceClient;
use crate::pb::cortex::{ClientMessage, FileChunk, FileRequest};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tonic::transport::Channel;
use tonic::Request;

const CHUNK_SIZE: usize = 64 * 1024;

struct ClientState {
    client_id: String,
    multi_bar: MultiProgress,
}

pub async fn run_client(address: String) -> anyhow::Result<()> {
    let client_id = uuid::Uuid::new_v4().to_string();
    println!(
        "Connecting to Cortex Server at {} as ID: {}",
        address, client_id
    );

    let channel = Channel::from_shared(address.clone())?.connect().await?;

    let grpc_client = CortexServiceClient::new(channel);

    let state = Arc::new(ClientState {
        client_id: client_id.clone(),
        multi_bar: MultiProgress::new(),
    });

    println!("Connected! Commands: 'upload <path>', 'download <name>', 'msg <text>', 'exit'");

    loop {
        print!("cortex> ");
        io::stdout().flush()?;

        let mut input = String::new();
        let stdin = std::io::stdin();
        stdin.read_line(&mut input)?;
        let input = input.trim();
        let parts: Vec<&str> = input.split_whitespace().collect();

        if parts.is_empty() {
            continue;
        }

        match parts[0] {
            "upload" => {
                if parts.len() < 2 {
                    println!("Usage: upload <path>");
                    continue;
                }
                let path = parts[1].to_string();
                let client_clone = grpc_client.clone();
                let state_clone = state.clone();

                tokio::spawn(async move {
                    if let Err(e) = upload_task(client_clone, state_clone, path).await {
                        eprintln!("Upload failed: {}", e);
                    }
                });
            }
            "download" => {
                if parts.len() < 2 {
                    println!("Usage: download <filename>");
                    continue;
                }
                let fname = parts[1].to_string();
                let client_clone = grpc_client.clone();
                let state_clone = state.clone();

                tokio::spawn(async move {
                    if let Err(e) = download_task(client_clone, state_clone, fname).await {
                        eprintln!("Download failed: {}", e);
                    }
                });
            }
            "msg" => {
                let msg_text = parts[1..].join(" ");
                let mut client_clone = grpc_client.clone();
                let id = state.client_id.clone();

                tokio::spawn(async move {
                    let req = Request::new(ClientMessage {
                        client_id: id,
                        text: msg_text,
                    });
                    if let Ok(_) = client_clone.send_message(req).await {}
                });
            }
            "exit" => break,
            _ => println!("Unknown command."),
        }
    }

    Ok(())
}

async fn upload_task(
    mut client: CortexServiceClient<Channel>,
    state: Arc<ClientState>,
    path_str: String,
) -> anyhow::Result<()> {
    let path = Path::new(&path_str);

    if !path.is_file() {
        return Err(anyhow::anyhow!("Path is not a file or does not exist"));
    }

    let file_name = path.file_name().unwrap().to_str().unwrap().to_string();
    let file = File::open(path).await?;
    let metadata = file.metadata().await?;
    let total_size = metadata.len();

    let pb = state.multi_bar.add(ProgressBar::new(total_size));
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} Uploading {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})")?
        .progress_chars("#>-"));
    pb.set_message(file_name.clone());

    let stream = async_stream::stream! {
        let mut reader = BufReader::new(file);
        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut first = true;

        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    pb.inc(n as u64);
                    yield FileChunk {
                        file_name: file_name.clone(),
                        content: buffer[..n].to_vec(),
                        total_size: if first { total_size } else { 0 },
                    };
                    first = false;
                },
                Err(e) => {
                    state.multi_bar.suspend(|| eprintln!("Error reading file: {}", e));
                    break;
                }
            }
        }
        pb.finish_with_message("Done");
    };

    client.upload_file(Request::new(stream)).await?;
    Ok(())
}

async fn download_task(
    mut client: CortexServiceClient<Channel>,
    state: Arc<ClientState>,
    file_name: String,
) -> anyhow::Result<()> {
    let req = Request::new(FileRequest {
        file_name: file_name.clone(),
    });

    let mut stream = client.download_file(req).await?.into_inner();
    let pb = state.multi_bar.add(ProgressBar::new(0));
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} Downloading {msg} [{bar:40.green/white}] {bytes}/{total_bytes} ({bytes_per_sec})")?
        .progress_chars("#>-"));
    pb.set_message(file_name.clone());

    let mut file: Option<BufWriter<File>> = None;
    let download_dir = "cortex_download";

    while let Some(chunk) = stream.message().await? {
        if file.is_none() {
            if chunk.total_size > 0 {
                pb.set_length(chunk.total_size);
            }

            tokio::fs::create_dir_all(download_dir).await?;

            let path = Path::new(download_dir).join(&chunk.file_name);

            let f = File::create(path).await?;
            file = Some(BufWriter::new(f));
        }

        if let Some(ref mut f) = file {
            f.write_all(&chunk.content).await?;
            pb.inc(chunk.content.len() as u64);
        }
    }

    if let Some(mut f) = file {
        f.flush().await?;
    }

    pb.finish_with_message(format!("Saved to {}\\{}", download_dir, file_name));
    Ok(())
}
