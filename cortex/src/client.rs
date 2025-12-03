use crate::pb::cortex::cortex_service_client::CortexServiceClient;
use crate::pb::cortex::{
    upload_part_request, ClientMessage, CompleteRequest, ConnectRequest, DownloadPartRequest,
    FileRequest, PartMetadata, UploadPartRequest,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Frame, Terminal,
};
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};
use tonic::transport::Channel;
use tonic::Request;

const PART_SIZE: u64 = 5 * 1024 * 1024;
const CHUNK_SIZE: usize = 32 * 1024;
const MAX_CONCURRENT_TASKS: usize = 5;
const UI_UPDATE_THRESHOLD_BYTES: u64 = 256 * 1024;

#[derive(Debug)]
pub enum AppEvent {
    Input(char),
    Backspace,
    SubmitCommand,
    Log(String),
    TransferStart {
        id: String,
        filename: String,
        total_size: u64,
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

struct App {
    input: String,
    messages: Vec<String>,
    transfers: HashMap<String, TransferState>,
}

impl App {
    fn new() -> Self {
        Self {
            input: String::new(),
            messages: vec![
                "[SYSTEM] Cortex Client Ready. Type 'upload <file>' or 'download <file>'".into(),
            ],
            transfers: HashMap::new(),
        }
    }
}

pub async fn run_client(address: String) -> anyhow::Result<()> {
    let client_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let channel = Channel::from_shared(address.clone())?.connect().await?;
    let client = CortexServiceClient::new(channel);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::channel(500);

    let tx_input = tx.clone();
    std::thread::spawn(move || {
        let tick_rate = Duration::from_millis(100);
        loop {
            if event::poll(tick_rate).unwrap() {
                if let Event::Key(key) = event::read().unwrap() {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char(c) => {
                                let _ = tx_input.blocking_send(AppEvent::Input(c));
                            }
                            KeyCode::Backspace => {
                                let _ = tx_input.blocking_send(AppEvent::Backspace);
                            }
                            KeyCode::Enter => {
                                let _ = tx_input.blocking_send(AppEvent::SubmitCommand);
                            }
                            KeyCode::Esc => {
                                let _ = tx_input.blocking_send(AppEvent::Exit);
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            } else {
                let _ = tx_input.blocking_send(AppEvent::Tick);
            }
        }
    });

    let mut sub_client = client.clone();
    let sub_id = client_id.clone();
    let tx_sub = tx.clone();
    tokio::spawn(async move {
        let req = Request::new(ConnectRequest { client_id: sub_id });
        if let Ok(resp) = sub_client.subscribe_messages(req).await {
            let mut stream = resp.into_inner();
            while let Some(msg) = stream.message().await.unwrap_or(None) {
                let _ = tx_sub
                    .send(AppEvent::Log(format!("[{}]: {}", msg.sender, msg.text)))
                    .await;
            }
        }
    });

    let mut app = App::new();
    let res = run_app_loop(
        &mut terminal,
        &mut app,
        &mut rx,
        client,
        client_id,
        tx.clone(),
    )
    .await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = res {
        println!("Error: {}", e);
    }

    Ok(())
}

async fn run_app_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: &mut mpsc::Receiver<AppEvent>,
    client: CortexServiceClient<Channel>,
    client_id: String,
    tx: mpsc::Sender<AppEvent>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app, &client_id))?;

        let mut processed = 0;
        loop {
            if let Ok(event) = rx.try_recv() {
                if handle_event(app, event, &client, &client_id, &tx) {
                    return Ok(());
                }
                processed += 1;
                if processed > 50 {
                    break;
                }
            } else {
                break;
            }
        }

        if processed == 0 {
            if let Some(event) = rx.recv().await {
                if handle_event(app, event, &client, &client_id, &tx) {
                    return Ok(());
                }
            } else {
                break;
            }
        }
    }
    Ok(())
}

fn handle_event(
    app: &mut App,
    event: AppEvent,
    client: &CortexServiceClient<Channel>,
    client_id: &String,
    tx: &mpsc::Sender<AppEvent>,
) -> bool {
    match event {
        AppEvent::Exit => return true,
        AppEvent::Input(c) => app.input.push(c),
        AppEvent::Backspace => {
            app.input.pop();
        }
        AppEvent::Tick => {
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
        AppEvent::Log(msg) => app.messages.push(msg),
        AppEvent::TransferStart {
            id,
            filename,
            total_size,
            mode,
        } => {
            app.transfers.insert(
                id,
                TransferState {
                    filename,
                    total_size,
                    current_bytes: 0,
                    mode,
                    parts: HashMap::new(),
                    last_bytes: 0,
                    last_tick: Instant::now(),
                    speed_bps: 0.0,
                },
            );
        }
        AppEvent::PartStart {
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
        AppEvent::PartProgress {
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
        AppEvent::PartComplete { file_id, part_id } => {
            if let Some(t) = app.transfers.get_mut(&file_id) {
                t.parts.remove(&part_id);
            }
        }
        AppEvent::TransferComplete { id } => {
            if let Some(t) = app.transfers.remove(&id) {
                app.messages
                    .push(format!("[SYSTEM] {} of {} finished.", t.mode, t.filename));
            }
        }
        AppEvent::SubmitCommand => {
            let cmd = app.input.drain(..).collect::<String>();
            handle_command(cmd, client.clone(), client_id.clone(), tx.clone());
        }
    }
    false
}

fn handle_command(
    cmd: String,
    client: CortexServiceClient<Channel>,
    client_id: String,
    tx: mpsc::Sender<AppEvent>,
) {
    let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    match parts[0] {
        "upload" => {
            if parts.len() < 2 {
                return;
            }
            let fname = parts[1].to_string();
            tokio::spawn(async move {
                if let Err(e) = multipart_upload(client, fname, tx.clone()).await {
                    let _ = tx
                        .send(AppEvent::Log(format!("[ERROR] Upload: {}", e)))
                        .await;
                }
            });
        }
        "download" => {
            if parts.len() < 2 {
                return;
            }
            let fname = parts[1].to_string();
            tokio::spawn(async move {
                if let Err(e) = multipart_download(client, fname, tx.clone()).await {
                    let _ = tx
                        .send(AppEvent::Log(format!("[ERROR] Download: {}", e)))
                        .await;
                }
            });
        }
        "msg" => {
            let text = parts[1..].join(" ");
            let mut c = client.clone();
            let id = client_id.clone();
            let tx_log = tx.clone();
            tokio::spawn(async move {
                match c
                    .send_message(Request::new(ClientMessage {
                        client_id: id,
                        text,
                    }))
                    .await
                {
                    Ok(_) => {
                        let _ = tx_log.send(AppEvent::Log("Message sent.".into())).await;
                    }
                    Err(e) => {
                        let _ = tx_log
                            .send(AppEvent::Log(format!("Msg Failed: {}", e)))
                            .await;
                    }
                }
            });
        }
        "exit" => {
            std::process::exit(0);
        }
        _ => {}
    }
}

fn ui(f: &mut Frame, app: &App, client_id: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(f.area());

    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    let block_transfers = Block::default()
        .borders(Borders::ALL)
        .title(" Concurrent IO ");
    let transfer_area = block_transfers.inner(body_chunks[0]);
    f.render_widget(block_transfers, body_chunks[0]);

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
            items.push(RenderItem {
                label: format!(
                    "{} {} {:.0}% | {:.1} MB/s",
                    if t.mode == "UPLOAD" { "▲" } else { "▼" },
                    t.filename,
                    pct * 100.0,
                    t.speed_bps / 1_048_576.0
                ),
                ratio: pct,
                color: if t.mode == "UPLOAD" {
                    Color::Green
                } else {
                    Color::Cyan
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
                    label: format!(
                        "  └ Part @{:.1}MB {:.0}%",
                        p.offset as f64 / 1_048_576.0,
                        p_pct * 100.0
                    ),
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
                .gauge_style(Style::default().fg(item.color).bg(Color::Black))
                .ratio(item.ratio)
                .label(item.label.clone());
            f.render_widget(g, rects[i]);
        }
    }

    let messages: Vec<ListItem> = app
        .messages
        .iter()
        .rev()
        .take(body_chunks[1].height as usize)
        .map(|m| ListItem::new(Line::from(Span::raw(m))))
        .collect();

    let logs_list = List::new(messages)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Message Log "),
        )
        .style(Style::default().fg(Color::White));
    f.render_widget(logs_list, body_chunks[1]);

    let input = Paragraph::new(format!("> {}", app.input))
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Command (ID: {}) ", client_id)),
        );
    f.render_widget(input, chunks[1]);
}

async fn multipart_upload(
    mut client: CortexServiceClient<Channel>,
    path_str: String,
    tx: mpsc::Sender<AppEvent>,
) -> anyhow::Result<()> {
    let path = Path::new(&path_str);
    if !path.is_file() {
        return Err(anyhow::anyhow!("File not found"));
    }

    let file_name = path.file_name().unwrap().to_str().unwrap().to_string();
    let file = File::open(path).await?;
    let total_size = file.metadata().await?.len();
    let file_id = uuid::Uuid::new_v4().to_string();

    tx.send(AppEvent::TransferStart {
        id: file_id.clone(),
        filename: file_name.clone(),
        total_size,
        mode: "UPLOAD".into(),
    })
    .await?;

    let init_resp = client
        .initiate_upload(Request::new(FileRequest {
            file_name: file_name.clone(),
            total_size,
        }))
        .await?
        .into_inner();
    let upload_id = init_resp.upload_id;

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_TASKS));
    let path_arc = Arc::new(path_str);
    let mut tasks = vec![];
    let mut current_offset = 0;

    while current_offset < total_size {
        let length = std::cmp::min(PART_SIZE, total_size - current_offset);
        let offset = current_offset;

        let sem = semaphore.clone();
        let c = client.clone();
        let p = path_arc.clone();
        let u = upload_id.clone();
        let t_ui = tx.clone();
        let f_id = file_id.clone();

        let task = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let part_id = uuid::Uuid::new_v4().to_string();

            let _ = t_ui
                .send(AppEvent::PartStart {
                    file_id: f_id.clone(),
                    part_id: part_id.clone(),
                    offset,
                    total: length,
                })
                .await;

            let res = upload_part(
                c,
                p,
                u,
                offset,
                length,
                t_ui.clone(),
                f_id.clone(),
                part_id.clone(),
            )
            .await;

            let _ = t_ui
                .send(AppEvent::PartComplete {
                    file_id: f_id,
                    part_id,
                })
                .await;
            res
        });

        tasks.push(task);
        current_offset += length;
    }

    for task in tasks {
        task.await??;
    }

    client
        .complete_upload(Request::new(CompleteRequest {
            upload_id,
            final_filename: file_name,
        }))
        .await?;

    tx.send(AppEvent::TransferComplete { id: file_id }).await?;
    Ok(())
}

async fn upload_part(
    mut client: CortexServiceClient<Channel>,
    path: Arc<String>,
    upload_id: String,
    offset: u64,
    length: u64,
    tx: mpsc::Sender<AppEvent>,
    file_id: String,
    part_id: String,
) -> anyhow::Result<()> {
    let mut file = File::open(path.as_str()).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    let stream = async_stream::stream! {
        yield UploadPartRequest {
            data: Some(upload_part_request::Data::Metadata(PartMetadata {
                upload_id: upload_id.clone(), offset, content_length: length,
            }))
        };

        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut sent = 0;
        let mut bytes_since_ui = 0;

        while sent < length {
            let to_read = std::cmp::min(CHUNK_SIZE as u64, length - sent) as usize;
            let n = file.read(&mut buffer[0..to_read]).await.unwrap();
            if n == 0 { break; }

            bytes_since_ui += n as u64;
            if bytes_since_ui >= UI_UPDATE_THRESHOLD_BYTES {
                 let _ = tx.try_send(AppEvent::PartProgress {
                    file_id: file_id.clone(), part_id: part_id.clone(), added: bytes_since_ui
                });
                bytes_since_ui = 0;
            }

            yield UploadPartRequest { data: Some(upload_part_request::Data::Content(buffer[0..n].to_vec())) };
            sent += n as u64;
        }
        if bytes_since_ui > 0 {
             let _ = tx.try_send(AppEvent::PartProgress {
                file_id: file_id.clone(), part_id: part_id.clone(), added: bytes_since_ui
            });
        }
    };

    client.upload_part(Request::new(stream)).await?;
    Ok(())
}

async fn multipart_download(
    client: CortexServiceClient<Channel>,
    fname: String,
    tx: mpsc::Sender<AppEvent>,
) -> anyhow::Result<()> {
    let mut meta_client = client.clone();
    let meta_req = Request::new(FileRequest {
        file_name: fname.clone(),
        total_size: 0,
    });
    let meta = meta_client.get_file_metadata(meta_req).await?.into_inner();
    let total_size = meta.total_size;

    let file_id = uuid::Uuid::new_v4().to_string();
    tx.send(AppEvent::TransferStart {
        id: file_id.clone(),
        filename: fname.clone(),
        total_size,
        mode: "DOWNLOAD".into(),
    })
    .await?;

    let download_dir = "cortex_download";
    tokio::fs::create_dir_all(download_dir).await?;
    let path_buf = Path::new(download_dir).join(&fname);
    let local_path = path_buf.to_str().unwrap().to_string();

    let f = File::create(&local_path).await?;
    f.set_len(total_size).await?;
    drop(f);

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_TASKS));
    let path_arc = Arc::new(local_path);
    let mut tasks = vec![];
    let mut current_offset = 0;

    while current_offset < total_size {
        let length = std::cmp::min(PART_SIZE, total_size - current_offset);
        let offset = current_offset;

        let sem = semaphore.clone();
        let mut c = client.clone();
        let p = path_arc.clone();
        let t_ui = tx.clone();
        let f_id = file_id.clone();
        let f_name = fname.clone();

        let task = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let part_id = uuid::Uuid::new_v4().to_string();

            let _ = t_ui
                .send(AppEvent::PartStart {
                    file_id: f_id.clone(),
                    part_id: part_id.clone(),
                    offset,
                    total: length,
                })
                .await;

            let req = Request::new(DownloadPartRequest {
                file_name: f_name,
                offset,
                length,
            });

            let mut file = OpenOptions::new()
                .write(true)
                .open(p.as_str())
                .await
                .unwrap();
            file.seek(std::io::SeekFrom::Start(offset)).await.unwrap();

            if let Ok(response) = c.download_part(req).await {
                let mut stream = response.into_inner();
                let mut bytes_since_ui = 0;

                while let Some(chunk) = stream.message().await.unwrap() {
                    file.write_all(&chunk.content).await.unwrap();
                    let len = chunk.content.len() as u64;
                    bytes_since_ui += len;

                    if bytes_since_ui >= UI_UPDATE_THRESHOLD_BYTES {
                        let _ = t_ui.try_send(AppEvent::PartProgress {
                            file_id: f_id.clone(),
                            part_id: part_id.clone(),
                            added: bytes_since_ui,
                        });
                        bytes_since_ui = 0;
                    }
                }
                if bytes_since_ui > 0 {
                    let _ = t_ui.try_send(AppEvent::PartProgress {
                        file_id: f_id.clone(),
                        part_id: part_id.clone(),
                        added: bytes_since_ui,
                    });
                }
            }

            let _ = t_ui
                .send(AppEvent::PartComplete {
                    file_id: f_id,
                    part_id,
                })
                .await;
        });

        tasks.push(task);
        current_offset += length;
    }

    for task in tasks {
        task.await?;
    }

    tx.send(AppEvent::TransferComplete { id: file_id }).await?;
    Ok(())
}
