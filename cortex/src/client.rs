use crate::pb::cortex::cortex_service_client::CortexServiceClient;
use crate::pb::cortex::{
    upload_part_request, ClientMessage, CompleteRequest, ConnectRequest, FileRequest, PartMetadata,
    UploadPartRequest,
};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::Semaphore;
use tonic::transport::Channel;
use tonic::Request;

const PART_SIZE: u64 = 50 * 1024 * 1024;
const CHUNK_SIZE: usize = 64 * 1024;
const MAX_CONCURRENT_UPLOADS: usize = 5;

pub async fn run_client(address: String) -> anyhow::Result<()> {
    let client_id = uuid::Uuid::new_v4().to_string();
    println!("Connecting to {} as ID: {}", address, client_id);

    let channel = Channel::from_shared(address.clone())?.connect().await?;
    let client = CortexServiceClient::new(channel);

    let sub_client = client.clone();
    let sub_id = client_id.clone();
    tokio::spawn(async move {
        if let Err(e) = subscribe_task(sub_client, sub_id).await {
            eprintln!("Subscription error: {}", e);
        }
    });

    let multi_progress = Arc::new(MultiProgress::new());

    println!("Connected! Commands: upload <path>, download <name>, msg <text>, exit");

    loop {
        print!("cortex> ");
        io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let parts: Vec<&str> = input.trim().split_whitespace().collect();

        if parts.is_empty() {
            continue;
        }

        match parts[0] {
            "upload" => {
                if parts.len() < 2 {
                    println!("Usage: upload <path>");
                    continue;
                }
                let fname = parts[1].to_string();
                let c = client.clone();
                let mp = multi_progress.clone();

                tokio::spawn(async move {
                    if let Err(e) = multipart_upload(c, fname, mp).await {
                        eprintln!("\nUpload Error: {}", e);
                    }
                });
            }
            "download" => {
                if parts.len() < 2 {
                    println!("Usage: download <name>");
                    continue;
                }
                let fname = parts[1].to_string();
                let c = client.clone();
                let mp = multi_progress.clone();

                tokio::spawn(async move {
                    if let Err(e) = download_task(c, fname, mp).await {
                        eprintln!("\nDownload Error: {}", e);
                    }
                });
            }
            "msg" => {
                if parts.len() < 2 {
                    println!("Usage: msg <text>");
                    continue;
                }
                let text = parts[1..].join(" ");
                let mut c = client.clone();
                let id = client_id.clone();

                tokio::spawn(async move {
                    match c
                        .send_message(Request::new(ClientMessage {
                            client_id: id,
                            text,
                        }))
                        .await
                    {
                        Ok(resp) => {
                            print!("\r\x1b[2K");
                            println!("Server ACK: {}", resp.into_inner().text);
                            print!("cortex> ");
                            io::stdout().flush().unwrap();
                        }
                        Err(e) => eprintln!("Message error: {}", e),
                    }
                });
            }
            "exit" => break,
            _ => println!("Unknown command"),
        }
    }
    Ok(())
}

async fn subscribe_task(
    mut client: CortexServiceClient<Channel>,
    id: String,
) -> anyhow::Result<()> {
    let req = Request::new(ConnectRequest { client_id: id });
    let mut stream = client.subscribe_messages(req).await?.into_inner();

    while let Some(msg) = stream.message().await? {
        print!("\r\x1b[2K");
        println!("\n[SERVER]: {}", msg.text);
        print!("cortex> ");
        io::stdout().flush()?;
    }
    Ok(())
}

async fn multipart_upload(
    mut client: CortexServiceClient<Channel>,
    path_str: String,
    mp: Arc<MultiProgress>,
) -> anyhow::Result<()> {
    let path = Path::new(&path_str);
    if !path.is_file() {
        return Err(anyhow::anyhow!("File not found"));
    }

    let file_name = path.file_name().unwrap().to_str().unwrap().to_string();
    let file = File::open(path).await?;
    let total_size = file.metadata().await?.len();

    let main_pb = mp.add(ProgressBar::new(total_size));
    main_pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [TOTAL] {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")?
        .progress_chars("=>-"));
    main_pb.set_message(file_name.clone());

    let init_resp = client
        .initiate_upload(Request::new(FileRequest {
            file_name: file_name.clone(),
            total_size,
        }))
        .await?
        .into_inner();

    let upload_id = init_resp.upload_id;
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS));
    let path_arc = Arc::new(path_str);
    let mut tasks = vec![];
    let mut current_offset = 0;
    let mut part_num = 1;

    while current_offset < total_size {
        let length = std::cmp::min(PART_SIZE, total_size - current_offset);
        let offset = current_offset;

        let sem_clone = semaphore.clone();
        let client_clone = client.clone();
        let path_clone = path_arc.clone();
        let id_clone = upload_id.clone();

        let main_pb_clone = main_pb.clone();
        let mp_clone = mp.clone();
        let fname_clone = file_name.clone();

        let task = tokio::spawn(async move {
            let _permit = sem_clone.acquire().await.unwrap();
            upload_part(
                client_clone,
                path_clone,
                id_clone,
                offset,
                length,
                part_num,
                fname_clone,
                main_pb_clone,
                mp_clone,
            )
            .await
        });

        tasks.push(task);
        current_offset += length;
        part_num += 1;
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

    main_pb.finish_with_message("Upload Complete");
    Ok(())
}

async fn upload_part(
    mut client: CortexServiceClient<Channel>,
    path: Arc<String>,
    upload_id: String,
    offset: u64,
    length: u64,
    part_num: usize,
    filename: String,
    main_pb: ProgressBar,
    mp: Arc<MultiProgress>,
) -> anyhow::Result<()> {
    let mut file = File::open(path.as_str()).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    let part_pb = mp.add(ProgressBar::new(length));
    part_pb.set_style(
        ProgressStyle::default_bar()
            .template("  ↳ {spinner:.yellow} {msg} [{bar:20.yellow/white}] {bytes}/{total_bytes}")?
            .progress_chars("#>-"),
    );
    part_pb.set_message(format!("Part #{} of {}", part_num, filename));

    let part_pb_stream = part_pb.clone();

    let stream = async_stream::stream! {
        yield UploadPartRequest {
            data: Some(upload_part_request::Data::Metadata(PartMetadata {
                upload_id: upload_id.clone(),
                offset,
                content_length: length,
            }))
        };

        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut sent = 0;

        while sent < length {
            let to_read = std::cmp::min(CHUNK_SIZE as u64, length - sent) as usize;
            let n = file.read(&mut buffer[0..to_read]).await.unwrap();
            if n == 0 { break; }

            part_pb_stream.inc(n as u64);
            main_pb.inc(n as u64);

            yield UploadPartRequest {
                data: Some(upload_part_request::Data::Content(buffer[0..n].to_vec()))
            };
            sent += n as u64;
        }
    };

    client.upload_part(Request::new(stream)).await?;

    part_pb.finish_and_clear();
    Ok(())
}

async fn download_task(
    mut client: CortexServiceClient<Channel>,
    fname: String,
    mp: Arc<MultiProgress>,
) -> anyhow::Result<()> {
    let req = Request::new(FileRequest {
        file_name: fname.clone(),
        total_size: 0,
    });
    let mut stream = client.download_file(req).await?.into_inner();

    let download_dir = "cortex_download";
    tokio::fs::create_dir_all(download_dir).await?;
    let path = Path::new(download_dir).join(&fname);
    let f = File::create(path).await?;
    let mut writer = BufWriter::new(f);

    let pb = mp.add(ProgressBar::new(0));
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [DOWN] {msg} [{bar:40.green/white}] {bytes} ({bytes_per_sec})",
            )?
            .progress_chars("#>-"),
    );

    let mut chunks_received = 0;
    pb.set_message(format!("{} (Init)", fname));

    while let Some(chunk) = stream.message().await? {
        writer.write_all(&chunk.content).await?;
        pb.inc(chunk.content.len() as u64);

        chunks_received += 1;
        if chunks_received % 10 == 0 {
            pb.set_message(format!("{} - Chunk {}", fname, chunks_received));
        }
    }
    writer.flush().await?;
    pb.finish_with_message("Download Done");
    Ok(())
}
