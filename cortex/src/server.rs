use crate::pb::cortex::cortex_service_server::{CortexService, CortexServiceServer};
use crate::pb::cortex::{
    upload_part_request, ClientMessage, CompleteRequest, ConnectRequest, DownloadPartRequest,
    FileChunk, FileMetadata, FileRequest, ServerMessage, ServerResponse, TransferStatus, UploadId,
    UploadPartRequest,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Terminal,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};

const UI_TICK_RATE: Duration = Duration::from_millis(100);
const UI_UPDATE_THRESHOLD_BYTES: u64 = 512 * 1024;

#[derive(Debug)]
pub enum ServerUiEvent {
    Log(String),
    ClientConnected(String),
    TransferStart {
        id: String,
        filename: String,
        total: u64,
        mode: String,
    },
    TransferComplete {
        id: String,
    },
    PartStart {
        file_id: String,
        part_id: String,
        offset: u64,
        total: u64,
    },
    PartProgress {
        file_id: String,
        part_id: String,
        added: u64,
    },
    PartComplete {
        file_id: String,
        part_id: String,
    },
    Tick,
    Input(char),
    Backspace,
    Submit,
    Exit,
}

struct PartState {
    offset: u64,
    total: u64,
    current: u64,
}

struct TransferState {
    filename: String,
    total_size: u64,
    current_bytes: u64,
    mode: String,
    parts: HashMap<String, PartState>,
    last_bytes: u64,
    last_tick: Instant,
    speed_bps: f64,
}

struct ServerApp {
    input: String,
    logs: Vec<String>,
    transfers: HashMap<String, TransferState>,
    clients: Vec<String>,
}

type ClientMap = Arc<Mutex<HashMap<String, mpsc::Sender<Result<ServerMessage, Status>>>>>;

#[derive(Debug)]
pub struct ServerState {
    pub storage_dir: String,
    pub clients: ClientMap,
    pub ui_tx: mpsc::Sender<ServerUiEvent>,
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
        Ok(Path::new(&self.state.storage_dir).join(clean_name.as_ref()))
    }

    fn emit(&self, event: ServerUiEvent) {
        let _ = self.state.ui_tx.try_send(event);
    }
}

#[tonic::async_trait]
impl CortexService for MyCortexService {
    type DownloadFileStream = ReceiverStream<Result<FileChunk, Status>>;
    type DownloadPartStream = ReceiverStream<Result<FileChunk, Status>>;
    type SubscribeMessagesStream = ReceiverStream<Result<ServerMessage, Status>>;

    async fn get_file_metadata(
        &self,
        request: Request<FileRequest>,
    ) -> Result<Response<FileMetadata>, Status> {
        let req = request.into_inner();
        let path = self.get_safe_path(&req.file_name)?;

        if !path.exists() {
            return Err(Status::not_found("File not found"));
        }

        let file = File::open(&path).await?;
        let size = file.metadata().await?.len();

        Ok(Response::new(FileMetadata {
            file_name: req.file_name,
            total_size: size,
        }))
    }

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

        self.emit(ServerUiEvent::TransferStart {
            id: upload_id.clone(),
            filename: req.file_name,
            total: req.total_size,
            mode: "RECV".into(),
        });

        Ok(Response::new(UploadId { upload_id }))
    }

    async fn upload_part(
        &self,
        request: Request<Streaming<UploadPartRequest>>,
    ) -> Result<Response<TransferStatus>, Status> {
        let mut stream = request.into_inner();
        let part_stream_id = uuid::Uuid::new_v4().to_string();

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

        self.emit(ServerUiEvent::PartStart {
            file_id: metadata.upload_id.clone(),
            part_id: part_stream_id.clone(),
            offset: metadata.offset,
            total: metadata.content_length,
        });

        let mut file = OpenOptions::new().write(true).open(&path).await?;
        file.seek(std::io::SeekFrom::Start(metadata.offset)).await?;

        let mut bytes_since_update = 0;

        while let Some(msg) = stream.message().await? {
            if let Some(upload_part_request::Data::Content(bytes)) = msg.data {
                file.write_all(&bytes).await?;

                let len = bytes.len() as u64;
                bytes_since_update += len;

                if bytes_since_update >= UI_UPDATE_THRESHOLD_BYTES {
                    self.emit(ServerUiEvent::PartProgress {
                        file_id: metadata.upload_id.clone(),
                        part_id: part_stream_id.clone(),
                        added: bytes_since_update,
                    });
                    bytes_since_update = 0;
                }
            }
        }

        if bytes_since_update > 0 {
            self.emit(ServerUiEvent::PartProgress {
                file_id: metadata.upload_id.clone(),
                part_id: part_stream_id.clone(),
                added: bytes_since_update,
            });
        }

        self.emit(ServerUiEvent::PartComplete {
            file_id: metadata.upload_id.clone(),
            part_id: part_stream_id,
        });

        Ok(Response::new(TransferStatus {
            message: "OK".into(),
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

        self.emit(ServerUiEvent::TransferComplete { id: req.upload_id });
        self.emit(ServerUiEvent::Log(format!(
            "File finalized: {}",
            req.final_filename
        )));

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

        if !path.exists() {
            return Err(Status::not_found("File not found"));
        }

        let download_id = uuid::Uuid::new_v4().to_string();
        let file = File::open(&path).await?;
        let total_size = file.metadata().await?.len();

        self.emit(ServerUiEvent::TransferStart {
            id: download_id.clone(),
            filename: req.file_name,
            total: total_size,
            mode: "SEND".into(),
        });

        let (tx, rx) = mpsc::channel(128);
        let ui_tx = self.state.ui_tx.clone();
        let part_id = "main_stream".to_string();

        tokio::spawn(async move {
            let _ = ui_tx.try_send(ServerUiEvent::PartStart {
                file_id: download_id.clone(),
                part_id: part_id.clone(),
                offset: 0,
                total: total_size,
            });

            let mut reader = BufReader::with_capacity(64 * 1024, file);
            let mut buffer = vec![0u8; 32 * 1024];
            let mut bytes_since_update = 0;

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

                        bytes_since_update += n as u64;
                        if bytes_since_update >= UI_UPDATE_THRESHOLD_BYTES {
                            let _ = ui_tx.try_send(ServerUiEvent::PartProgress {
                                file_id: download_id.clone(),
                                part_id: part_id.clone(),
                                added: bytes_since_update,
                            });
                            bytes_since_update = 0;
                        }
                    }
                    Err(_) => break,
                }
            }
            if bytes_since_update > 0 {
                let _ = ui_tx.try_send(ServerUiEvent::PartProgress {
                    file_id: download_id.clone(),
                    part_id: part_id.clone(),
                    added: bytes_since_update,
                });
            }

            let _ = ui_tx.try_send(ServerUiEvent::PartComplete {
                file_id: download_id.clone(),
                part_id,
            });
            let _ = ui_tx.try_send(ServerUiEvent::TransferComplete { id: download_id });
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn download_part(
        &self,
        request: Request<DownloadPartRequest>,
    ) -> Result<Response<Self::DownloadPartStream>, Status> {
        let req = request.into_inner();
        let path = self.get_safe_path(&req.file_name)?;

        if !path.exists() {
            return Err(Status::not_found("File not found"));
        }

        let mut file = File::open(&path).await?;
        file.seek(std::io::SeekFrom::Start(req.offset)).await?;

        let limited_reader = file.take(req.length);
        let mut reader = BufReader::with_capacity(64 * 1024, limited_reader);

        let (tx, rx) = mpsc::channel(128);

        tokio::spawn(async move {
            let mut buffer = vec![0u8; 32 * 1024];
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
        self.emit(ServerUiEvent::Log(format!(
            "MSG from {}: {}",
            msg.client_id, msg.text
        )));
        let clients = self.state.clients.lock().unwrap();
        for (_id, tx) in clients.iter() {
            let _ = tx.try_send(Ok(ServerMessage {
                sender: msg.client_id.clone(),
                text: msg.text.clone(),
            }));
        }
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
        self.emit(ServerUiEvent::ClientConnected(req.client_id.clone()));
        self.state.clients.lock().unwrap().insert(req.client_id, tx);
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub async fn run_server(addr: String, storage: String) -> anyhow::Result<()> {
    std::fs::create_dir_all(&storage)?;

    let (ui_tx, mut ui_rx) = mpsc::channel(500);
    let clients = Arc::new(Mutex::new(HashMap::new()));

    let state = Arc::new(ServerState {
        storage_dir: storage,
        clients: clients.clone(),
        ui_tx: ui_tx.clone(),
    });

    let service = MyCortexService { state };
    let addr_obj = addr.parse()?;

    tokio::spawn(async move {
        Server::builder()
            .add_service(CortexServiceServer::new(service))
            .serve(addr_obj)
            .await
            .unwrap();
    });

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tx_input = ui_tx.clone();
    std::thread::spawn(move || loop {
        if event::poll(UI_TICK_RATE).unwrap() {
            if let Event::Key(key) = event::read().unwrap() {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char(c) => {
                            let _ = tx_input.blocking_send(ServerUiEvent::Input(c));
                        }
                        KeyCode::Backspace => {
                            let _ = tx_input.blocking_send(ServerUiEvent::Backspace);
                        }
                        KeyCode::Enter => {
                            let _ = tx_input.blocking_send(ServerUiEvent::Submit);
                        }
                        KeyCode::Esc => {
                            let _ = tx_input.blocking_send(ServerUiEvent::Exit);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        } else {
            let _ = tx_input.blocking_send(ServerUiEvent::Tick);
        }
    });

    let mut app = ServerApp {
        input: String::new(),
        logs: vec!["Server listening...".into()],
        transfers: HashMap::new(),
        clients: Vec::new(),
    };

    loop {
        terminal.draw(|f| server_ui(f, &app))?;

        let mut processed = 0;
        while processed < 50 {
            if let Ok(event) = ui_rx.try_recv() {
                process_event(&mut app, event, &clients);
                processed += 1;
            } else {
                break;
            }
        }

        if processed == 0 {
            if let Some(event) = ui_rx.recv().await {
                if matches!(event, ServerUiEvent::Exit) {
                    break;
                }
                process_event(&mut app, event, &clients);
            } else {
                break;
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn process_event(app: &mut ServerApp, event: ServerUiEvent, clients: &ClientMap) {
    match event {
        ServerUiEvent::Exit => {}
        ServerUiEvent::Log(m) => app.logs.push(m),
        ServerUiEvent::ClientConnected(id) => {
            app.clients.push(id.clone());
            app.logs.push(format!("New Client: {}", id));
        }
        ServerUiEvent::TransferStart {
            id,
            filename,
            total,
            mode,
        } => {
            app.transfers.insert(
                id,
                TransferState {
                    filename,
                    total_size: total,
                    current_bytes: 0,
                    mode,
                    parts: HashMap::new(),
                    last_bytes: 0,
                    last_tick: Instant::now(),
                    speed_bps: 0.0,
                },
            );
        }
        ServerUiEvent::PartStart {
            file_id,
            part_id,
            offset,
            total,
        } => {
            if let Some(t) = app.transfers.get_mut(&file_id) {
                t.parts.insert(
                    part_id,
                    PartState {
                        offset,
                        total,
                        current: 0,
                    },
                );
            }
        }
        ServerUiEvent::PartProgress {
            file_id,
            part_id,
            added,
        } => {
            if let Some(t) = app.transfers.get_mut(&file_id) {
                t.current_bytes += added;
                if let Some(p) = t.parts.get_mut(&part_id) {
                    p.current += added;
                }
            }
        }
        ServerUiEvent::PartComplete { file_id, part_id } => {
            if let Some(t) = app.transfers.get_mut(&file_id) {
                t.parts.remove(&part_id);
            }
        }
        ServerUiEvent::TransferComplete { id } => {
            app.transfers.remove(&id);
        }
        ServerUiEvent::Input(c) => app.input.push(c),
        ServerUiEvent::Backspace => {
            app.input.pop();
        }
        ServerUiEvent::Submit => {
            let cmd = app.input.drain(..).collect::<String>();
            let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
            if !parts.is_empty() && parts[0] == "bc" {
                let text = parts[1..].join(" ");
                let map = clients.lock().unwrap();
                for tx in map.values() {
                    let _ = tx.try_send(Ok(ServerMessage {
                        sender: "SERVER".into(),
                        text: text.clone(),
                    }));
                }
                app.logs.push(format!("Broadcast: {}", text));
            }
        }
        ServerUiEvent::Tick => {
            for t in app.transfers.values_mut() {
                let now = Instant::now();
                let elapsed = now.duration_since(t.last_tick).as_secs_f64();
                if elapsed > 0.5 {
                    let bytes_diff = t.current_bytes.saturating_sub(t.last_bytes);
                    t.speed_bps = bytes_diff as f64 / elapsed;
                    t.last_bytes = t.current_bytes;
                    t.last_tick = now;
                }
            }
        }
    }
}

fn server_ui(f: &mut ratatui::Frame, app: &ServerApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(f.area());

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(50),
            Constraint::Percentage(30),
        ])
        .split(chunks[0]);

    let clients: Vec<ListItem> = app
        .clients
        .iter()
        .map(|c| ListItem::new(Line::from(c.as_str())))
        .collect();
    f.render_widget(
        List::new(clients).block(Block::default().borders(Borders::ALL).title("Clients")),
        top[0],
    );

    let transfer_block = Block::default()
        .borders(Borders::ALL)
        .title(" Concurrent IO Operations ");
    let transfer_area = transfer_block.inner(top[1]);
    f.render_widget(transfer_block, top[1]);

    let available_height = transfer_area.height as usize;
    if available_height > 0 {
        let mut layout_constraints = Vec::new();
        struct RenderItem {
            label: String,
            ratio: f64,
            color: Color,
        }
        let mut items = Vec::new();

        for t in app.transfers.values() {
            let pct = if t.total_size > 0 {
                (t.current_bytes as f64 / t.total_size as f64).min(1.0)
            } else {
                0.0
            };
            let speed_mb = t.speed_bps / 1_048_576.0;

            items.push(RenderItem {
                label: format!(
                    "{} {} {:.0}% {:.2}MB/s",
                    if t.mode == "RECV" { "▼" } else { "▲" },
                    t.filename,
                    pct * 100.0,
                    speed_mb
                ),
                ratio: pct,
                color: if t.mode == "RECV" {
                    Color::Cyan
                } else {
                    Color::Green
                },
            });

            let mut parts: Vec<_> = t.parts.iter().collect();
            parts.sort_by_key(|(_, p)| p.offset);

            for (_, p) in parts {
                let p_pct = if p.total > 0 {
                    (p.current as f64 / p.total as f64).min(1.0)
                } else {
                    0.0
                };
                items.push(RenderItem {
                    label: format!("  └ Part {:.0}%", p_pct * 100.0),
                    ratio: p_pct,
                    color: Color::DarkGray,
                });
            }
        }

        for _ in items.iter().take(available_height) {
            layout_constraints.push(Constraint::Length(1));
        }

        layout_constraints.push(Constraint::Min(0));

        let rects = Layout::default()
            .direction(Direction::Vertical)
            .constraints(layout_constraints)
            .split(transfer_area);

        for (i, item) in items.iter().take(available_height).enumerate() {
            let g = Gauge::default()
                .gauge_style(Style::default().fg(item.color).bg(Color::Reset))
                .ratio(item.ratio)
                .label(item.label.clone());
            f.render_widget(g, rects[i]);
        }
    }

    let logs: Vec<ListItem> = app
        .logs
        .iter()
        .rev()
        .take(30)
        .map(|l| ListItem::new(Line::from(l.as_str())))
        .collect();
    f.render_widget(
        List::new(logs).block(Block::default().borders(Borders::ALL).title("Logs")),
        top[2],
    );

    f.render_widget(
        Paragraph::new(app.input.as_str())
            .block(Block::default().borders(Borders::ALL).title("Cmd")),
        chunks[1],
    );
}
