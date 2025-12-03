use crate::pb::cortex::cortex_service_server::CortexService;
use crate::pb::cortex::{
    upload_part_request, ClientMessage, CompleteRequest, ConnectRequest, FileChunk, FileRequest,
    ServerMessage, ServerResponse, TransferStatus, UploadId, UploadPartRequest,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::info;

type ClientMap = Arc<Mutex<HashMap<String, mpsc::Sender<Result<ServerMessage, Status>>>>>;

#[derive(Debug)]
pub struct ServerState {
    pub storage_dir: String,
    pub clients: ClientMap,
}

pub struct MyCortexService {
    pub state: Arc<ServerState>,
}

impl MyCortexService {
    fn get_safe_path(&self, filename: &str) -> Result<PathBuf, Status> {
        let clean_name = Path::new(filename)
            .file_name()
            .ok_or_else(|| Status::invalid_argument("Invalid filename"))?
            .to_string_lossy();

        if clean_name.is_empty() || clean_name.contains("..") {
            return Err(Status::invalid_argument("Invalid filename security check"));
        }
        Ok(Path::new(&self.state.storage_dir).join(clean_name.as_ref()))
    }
}

#[tonic::async_trait]
impl CortexService for MyCortexService {
    type DownloadFileStream = ReceiverStream<Result<FileChunk, Status>>;
    type SubscribeMessagesStream = ReceiverStream<Result<ServerMessage, Status>>;

    async fn initiate_upload(
        &self,
        request: Request<FileRequest>,
    ) -> Result<Response<UploadId>, Status> {
        let req = request.into_inner();
        let upload_id = uuid::Uuid::new_v4().to_string();
        let path = self.get_safe_path(&upload_id)?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let file = File::create(&path).await?;
        file.set_len(req.total_size).await?;
        info!("Initiated upload: {}", upload_id);

        Ok(Response::new(UploadId { upload_id }))
    }

    async fn upload_part(
        &self,
        request: Request<Streaming<UploadPartRequest>>,
    ) -> Result<Response<TransferStatus>, Status> {
        let mut stream = request.into_inner();

        let metadata = match stream.message().await? {
            Some(msg) => match msg.data {
                Some(upload_part_request::Data::Metadata(m)) => m,
                _ => return Err(Status::invalid_argument("Metadata missing")),
            },
            None => return Err(Status::invalid_argument("Empty stream")),
        };

        let path = self.get_safe_path(&metadata.upload_id)?;
        if !path.exists() {
            return Err(Status::not_found("Upload ID not found"));
        }

        let mut file = OpenOptions::new().write(true).open(&path).await?;
        file.seek(std::io::SeekFrom::Start(metadata.offset)).await?;

        let mut bytes_written = 0;
        while let Some(msg) = stream.message().await? {
            if let Some(upload_part_request::Data::Content(bytes)) = msg.data {
                file.write_all(&bytes).await?;
                bytes_written += bytes.len();
            }
        }

        Ok(Response::new(TransferStatus {
            message: format!("Written {}", bytes_written),
            success: true,
        }))
    }

    async fn complete_upload(
        &self,
        request: Request<CompleteRequest>,
    ) -> Result<Response<TransferStatus>, Status> {
        let req = request.into_inner();
        let temp_path = self.get_safe_path(&req.upload_id)?;
        let final_path = self.get_safe_path(&req.final_filename)?;

        if !temp_path.exists() {
            return Err(Status::not_found("Upload ID not found"));
        }
        tokio::fs::rename(temp_path, &final_path).await?;
        info!("Upload completed: {:?}", final_path);

        Ok(Response::new(TransferStatus {
            message: "Success".into(),
            success: true,
        }))
    }

    async fn download_file(
        &self,
        request: Request<FileRequest>,
    ) -> Result<Response<Self::DownloadFileStream>, Status> {
        let req = request.into_inner();
        let path = self.get_safe_path(&req.file_name)?;

        let file = File::open(&path).await?;
        let (tx, rx) = mpsc::channel(128);

        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(file);
            let mut buffer = vec![0u8; 64 * 1024];

            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx
                            .send(Ok(FileChunk {
                                content: buffer[..n].to_vec(),
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn send_message(
        &self,
        request: Request<ClientMessage>,
    ) -> Result<Response<ServerResponse>, Status> {
        let msg = request.into_inner();
        info!("Message from {}: {}", msg.client_id, msg.text);
        Ok(Response::new(ServerResponse {
            text: "Received".into(),
        }))
    }

    async fn subscribe_messages(
        &self,
        request: Request<ConnectRequest>,
    ) -> Result<Response<Self::SubscribeMessagesStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = mpsc::channel(100);

        info!("Client subscribed: {}", req.client_id);
        self.state.clients.lock().unwrap().insert(req.client_id, tx);

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
