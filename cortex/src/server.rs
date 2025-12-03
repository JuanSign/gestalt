use crate::pb::cortex::cortex_service_server::CortexService;
use crate::pb::cortex::{ClientMessage, FileChunk, FileRequest, ServerResponse, TransferStatus};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::path::Path;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{info, warn};

pub struct ServerState {
    pub multi_bar: MultiProgress,
}

pub struct MyCortexService {
    pub state: Arc<ServerState>,
    pub storage_dir: String,
}

#[tonic::async_trait]
impl CortexService for MyCortexService {
    type DownloadFileStream = ReceiverStream<Result<FileChunk, Status>>;

    async fn upload_file(
        &self,
        request: Request<Streaming<FileChunk>>,
    ) -> Result<Response<TransferStatus>, Status> {
        let mut stream = request.into_inner();
        let storage_dir = self.storage_dir.clone();
        let mb = self.state.multi_bar.clone();

        let pb = mb.add(ProgressBar::new(0));
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        pb.set_message("Receiving Upload...");

        let mut file: Option<BufWriter<File>> = None;
        let mut file_name = String::new();

        while let Some(chunk_result) = stream.message().await? {
            let chunk = chunk_result;

            if file.is_none() {
                file_name = chunk.file_name.clone();
                let path = Path::new(&storage_dir).join(&file_name);

                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                let f = File::create(path).await?;
                file = Some(BufWriter::new(f));

                if chunk.total_size > 0 {
                    pb.set_length(chunk.total_size);
                }
                pb.set_message(format!("Receiving: {}", file_name));
            }

            if let Some(ref mut f) = file {
                f.write_all(&chunk.content).await?;
                pb.inc(chunk.content.len() as u64);
            }
        }

        if let Some(mut f) = file {
            f.flush().await?;
        }

        pb.finish_with_message(format!("Saved: {}", file_name));

        Ok(Response::new(TransferStatus {
            message: format!("Upload of {} complete", file_name),
            success: true,
        }))
    }

    async fn download_file(
        &self,
        request: Request<FileRequest>,
    ) -> Result<Response<Self::DownloadFileStream>, Status> {
        let req = request.into_inner();
        let path = Path::new(&self.storage_dir).join(&req.file_name);

        if !path.exists() {
            return Err(Status::not_found("File not found on server"));
        }

        let file = File::open(&path).await?;
        let metadata = file.metadata().await?;
        let total_size = metadata.len();

        let mb = self.state.multi_bar.clone();
        let pb = mb.add(ProgressBar::new(total_size));
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{msg} [{bar:40.green/white}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        pb.set_message(format!("Sending: {}", req.file_name));

        let (tx, rx) = mpsc::channel(128);

        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(file);

            let mut buffer = vec![0u8; 1024 * 64];

            let mut first = true;

            loop {
                let n = match reader.read(&mut buffer).await {
                    Ok(n) if n == 0 => break,
                    Ok(n) => n,
                    Err(e) => {
                        warn!("Error reading file: {}", e);
                        break;
                    }
                };

                let chunk = FileChunk {
                    file_name: req.file_name.clone(),
                    content: buffer[..n].to_vec(),
                    total_size: if first { total_size } else { 0 },
                };
                first = false;

                if tx.send(Ok(chunk)).await.is_err() {
                    warn!("Client disconnected during download");
                    break;
                }

                pb.inc(n as u64);
            }
            pb.finish_with_message(format!("Sent: {}", req.file_name));
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn send_message(
        &self,
        request: Request<ClientMessage>,
    ) -> Result<Response<ServerResponse>, Status> {
        let msg = request.into_inner();
        self.state.multi_bar.suspend(|| {
            info!("Message from {}: {}", msg.client_id, msg.text);
        });

        Ok(Response::new(ServerResponse {
            text: "Message received".into(),
        }))
    }
}
