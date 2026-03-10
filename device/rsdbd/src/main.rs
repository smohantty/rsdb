use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::{fs::OpenOptions as StdOpenOptions, io};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use rsdb_proto::{
    CapabilitySet, ControlRequest, ControlResponse, DEFAULT_STREAM_CHUNK_SIZE, DiscoveryRequest,
    DiscoveryResponse, ErrorCode, FrameKind, MAX_DISCOVERY_PAYLOAD_LEN, PROTOCOL_VERSION,
    ProtocolError, StreamChannel, decode_discovery_message, decode_json, decode_stream_frame,
    encode_discovery_message, read_frame, write_json_frame, write_stream_frame,
};
use tokio::fs::{File, OpenOptions, metadata};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, trace};

#[derive(Debug, Parser)]
#[command(name = "rsdbd")]
#[command(about = "Root daemon for RSDB remote control")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:27101")]
    listen: String,
    #[arg(long)]
    server_id: Option<String>,
    #[arg(long, default_value_t = 60)]
    exec_timeout_secs: u64,
}

#[derive(Debug, Clone)]
struct ServerState {
    server_id: Arc<str>,
    device_name: Arc<str>,
    exec_timeout: Duration,
    tcp_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;
    let args = Args::parse();
    let listener = TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("failed to bind listener on {}", args.listen))?;
    let listen_addr = listener
        .local_addr()
        .context("failed to read bound tcp listener address")?;
    let server_id = args.server_id.unwrap_or_else(default_server_id);
    let device_name = default_device_name(&server_id);
    let state = ServerState {
        server_id: Arc::from(server_id),
        device_name: Arc::from(device_name),
        exec_timeout: Duration::from_secs(args.exec_timeout_secs),
        tcp_port: listen_addr.port(),
    };
    let discovery_socket = UdpSocket::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind discovery socket on {listen_addr}"))?;
    debug!(
        listen = %listen_addr,
        server_id = %state.server_id,
        discovery = %listen_addr,
        "rsdbd listening"
    );
    let discovery_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = handle_discovery(discovery_socket, discovery_state).await {
            debug!(error = %err, "discovery socket stopped");
        }
    });

    loop {
        let (stream, peer) = listener.accept().await.context("accept failed")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, state).await {
                debug!(peer = %peer, error = %err, "connection closed with error");
            }
        });
    }
}

fn init_tracing() -> Result<()> {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "off".into());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact();
    if let Some(log_file) = std::env::var("RSDB_LOG_FILE")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        let file = StdOpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
            .with_context(|| format!("failed to open log file: {log_file}"))?;
        builder
            .with_writer(move || {
                file.try_clone()
                    .unwrap_or_else(|err| panic!("failed to clone log file handle: {err}"))
            })
            .init();
    } else {
        builder.with_writer(io::stderr).init();
    }
    Ok(())
}

fn default_server_id() -> String {
    std::env::var("RSDB_SERVER_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "rsdbd".to_string())
}

fn default_device_name(server_id: &str) -> String {
    std::env::var("RSDB_DEVICE_NAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| server_id.to_string())
}

async fn handle_discovery(socket: UdpSocket, state: ServerState) -> Result<()> {
    let mut buffer = vec![0_u8; MAX_DISCOVERY_PAYLOAD_LEN + rsdb_proto::DISCOVERY_MAGIC.len()];
    loop {
        let (len, peer) = socket
            .recv_from(&mut buffer)
            .await
            .context("discovery recv failed")?;
        let request: DiscoveryRequest = match decode_discovery_message(&buffer[..len]) {
            Ok(request) => request,
            Err(ProtocolError::InvalidDiscoveryMagic) => continue,
            Err(err) => {
                trace!(peer = %peer, error = %err, "invalid discovery request");
                continue;
            }
        };

        match request {
            DiscoveryRequest::Probe { nonce } => {
                let response = DiscoveryResponse {
                    nonce,
                    server_id: state.server_id.to_string(),
                    device_name: state.device_name.to_string(),
                    tcp_port: state.tcp_port,
                    protocol_version: PROTOCOL_VERSION,
                    features: capabilities().features,
                };
                let payload = encode_discovery_message(&response)?;
                socket
                    .send_to(&payload, peer)
                    .await
                    .with_context(|| format!("failed to send discovery response to {peer}"))?;
            }
        }
    }
}

async fn handle_connection(mut stream: TcpStream, state: ServerState) -> Result<()> {
    loop {
        let frame = match read_frame(&mut stream).await {
            Ok(frame) => frame,
            Err(ProtocolError::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };

        let request_id = frame.header.request_id;
        let request: ControlRequest = decode_json(&frame, FrameKind::Request)?;
        handle_request(&mut stream, request_id, request, &state).await?;
    }
}

async fn handle_request(
    stream: &mut TcpStream,
    request_id: u32,
    request: ControlRequest,
    state: &ServerState,
) -> Result<()> {
    match request {
        ControlRequest::Ping => {
            let response = ControlResponse::Pong {
                server_id: state.server_id.to_string(),
                protocol_version: PROTOCOL_VERSION,
            };
            write_json_frame(stream, FrameKind::Response, request_id, &response).await?;
        }
        ControlRequest::GetCapabilities => {
            let response = ControlResponse::Capabilities {
                server_id: state.server_id.to_string(),
                capability: capabilities(),
            };
            write_json_frame(stream, FrameKind::Response, request_id, &response).await?;
        }
        ControlRequest::Exec { command, args } => {
            let response = match execute_command(&command, &args, state).await {
                Ok(result) => result,
                Err(err) => ControlResponse::Error {
                    code: ErrorCode::ExecFailed,
                    message: err.to_string(),
                },
            };
            write_json_frame(stream, FrameKind::Response, request_id, &response).await?;
        }
        ControlRequest::Push { path, mode } => {
            if let Err(err) = handle_push(stream, request_id, &path, mode).await {
                send_error(stream, request_id, ErrorCode::FileTransferFailed, err).await?;
            }
        }
        ControlRequest::Pull { path } => {
            let code = if matches!(metadata(&path).await, Err(err) if err.kind() == std::io::ErrorKind::NotFound)
            {
                ErrorCode::NotFound
            } else {
                ErrorCode::FileTransferFailed
            };
            if let Err(err) = handle_pull(stream, request_id, &path).await {
                send_error(stream, request_id, code, err).await?;
            }
        }
        ControlRequest::Shell { command, args } => {
            if let Err(err) = handle_shell(stream, request_id, command, args).await {
                send_error(stream, request_id, ErrorCode::ExecFailed, err).await?;
            }
        }
    }
    Ok(())
}

fn capabilities() -> CapabilitySet {
    CapabilitySet {
        protocol_version: PROTOCOL_VERSION,
        transports: vec!["tcp".to_string(), "udp-discovery".to_string()],
        security: vec!["none".to_string()],
        features: vec![
            "discover.udp".to_string(),
            "ping".to_string(),
            "capability".to_string(),
            "shell.exec".to_string(),
            "shell.interactive".to_string(),
            "fs.push".to_string(),
            "fs.pull".to_string(),
        ],
    }
}

async fn execute_command(
    command: &str,
    args: &[String],
    state: &ServerState,
) -> Result<ControlResponse> {
    if command.trim().is_empty() {
        return Err(anyhow!("command must not be empty"));
    }

    let mut child = Command::new(command);
    child.args(args);
    child.stdout(Stdio::piped());
    child.stderr(Stdio::piped());

    let output = timeout(state.exec_timeout, child.output())
        .await
        .context("command timed out")?
        .with_context(|| format!("failed to execute command: {command}"))?;

    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(ControlResponse::ExecResult {
        status,
        stdout,
        stderr,
    })
}

async fn handle_push(stream: &mut TcpStream, request_id: u32, path: &str, mode: u32) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(normalize_mode(mode));

    let mut file = options
        .open(path)
        .await
        .with_context(|| format!("failed to open remote path for write: {path}"))?;
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::PushReady,
    )
    .await?;

    let mut bytes_written = 0_u64;
    loop {
        let frame = read_frame(stream).await?;
        let chunk = decode_stream_frame(&frame)?;
        expect_stream_chunk(&chunk, request_id, StreamChannel::File)?;

        if !chunk.payload.is_empty() {
            file.write_all(&chunk.payload).await?;
            bytes_written += chunk.payload.len() as u64;
        }

        if chunk.eof {
            break;
        }
    }

    file.flush().await?;
    #[cfg(unix)]
    if mode != 0 {
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(normalize_mode(mode)))
            .await
            .with_context(|| format!("failed to set permissions on {path}"))?;
    }

    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::PushComplete { bytes_written },
    )
    .await?;
    Ok(())
}

async fn handle_pull(stream: &mut TcpStream, request_id: u32, path: &str) -> Result<()> {
    let file_meta = metadata(path)
        .await
        .with_context(|| format!("failed to stat remote path: {path}"))?;
    if !file_meta.is_file() {
        bail!("remote path is not a regular file: {path}");
    }

    let mut file = File::open(path)
        .await
        .with_context(|| format!("failed to open remote path for read: {path}"))?;
    let mode = file_mode(&file_meta);

    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::PullMetadata {
            size: file_meta.len(),
            mode,
        },
    )
    .await?;

    let mut bytes_sent = 0_u64;
    let mut buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        bytes_sent += read as u64;
        write_stream_frame(
            stream,
            request_id,
            StreamChannel::File,
            false,
            &buffer[..read],
        )
        .await?;
    }

    write_stream_frame(stream, request_id, StreamChannel::File, true, &[]).await?;
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::PullComplete { bytes_sent },
    )
    .await?;
    Ok(())
}

async fn handle_shell(
    stream: &mut TcpStream,
    request_id: u32,
    command: Option<String>,
    args: Vec<String>,
) -> Result<()> {
    let (display_command, mut child) = spawn_shell(command, args)?;
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::ShellStarted {
            command: display_command,
        },
    )
    .await?;

    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("child stdin was not captured"))?;
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("child stdout was not captured"))?;
    let mut child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("child stderr was not captured"))?;

    let mut stdin_open = true;
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut exit_status = None;
    let mut stdout_buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
    let mut stderr_buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];

    while stdout_open || stderr_open || exit_status.is_none() {
        tokio::select! {
            frame = read_frame(stream), if stdin_open => {
                match frame {
                    Ok(frame) => {
                        let chunk = decode_stream_frame(&frame)?;
                        expect_stream_chunk(&chunk, request_id, StreamChannel::Stdin)?;
                        if !chunk.payload.is_empty() {
                            child_stdin.write_all(&chunk.payload).await?;
                            child_stdin.flush().await?;
                        }
                        if chunk.eof {
                            child_stdin.shutdown().await?;
                            stdin_open = false;
                        }
                    }
                    Err(ProtocolError::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        let _ = child_stdin.shutdown().await;
                        stdin_open = false;
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            read = child_stdout.read(&mut stdout_buffer), if stdout_open => {
                let read = read?;
                if read == 0 {
                    stdout_open = false;
                    write_stream_frame(stream, request_id, StreamChannel::Stdout, true, &[]).await?;
                } else {
                    write_stream_frame(
                        stream,
                        request_id,
                        StreamChannel::Stdout,
                        false,
                        &stdout_buffer[..read],
                    ).await?;
                }
            }
            read = child_stderr.read(&mut stderr_buffer), if stderr_open => {
                let read = read?;
                if read == 0 {
                    stderr_open = false;
                    write_stream_frame(stream, request_id, StreamChannel::Stderr, true, &[]).await?;
                } else {
                    write_stream_frame(
                        stream,
                        request_id,
                        StreamChannel::Stderr,
                        false,
                        &stderr_buffer[..read],
                    ).await?;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)), if exit_status.is_none() => {
                if let Some(status) = child.try_wait().context("failed to poll shell process")? {
                    exit_status = Some(status);
                    stdout_open = false;
                    stderr_open = false;
                }
            }
        }
    }

    if stdin_open {
        let _ = child_stdin.shutdown().await;
    }

    let status = match exit_status {
        Some(status) => status,
        None => child
            .wait()
            .await
            .context("failed to wait for shell process")?,
    };
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::ShellExit {
            status: status.code().unwrap_or(-1),
        },
    )
    .await?;
    Ok(())
}

fn spawn_shell(
    command: Option<String>,
    args: Vec<String>,
) -> Result<(String, tokio::process::Child)> {
    let shell = default_shell();
    let (display_command, program, argv) = match command {
        Some(command) if !command.trim().is_empty() => {
            let command_line = join_shell_command(&command, &args);
            (
                format!("{shell} -c {command_line}"),
                shell.clone(),
                vec!["-c".to_string(), command_line],
            )
        }
        Some(_) => bail!("shell command must not be empty"),
        None => (format!("{shell} -i"), shell.clone(), vec!["-i".to_string()]),
    };

    let mut child = Command::new(&program);
    child.args(&argv);
    child.stdin(Stdio::piped());
    child.stdout(Stdio::piped());
    child.stderr(Stdio::piped());

    let child = child
        .spawn()
        .with_context(|| format!("failed to spawn shell command: {program}"))?;
    Ok((display_command, child))
}

fn default_shell() -> String {
    std::env::var("RSDB_DEFAULT_SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn join_shell_command(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    if value
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'+' | b'='))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn expect_stream_chunk(
    chunk: &rsdb_proto::StreamFrame,
    request_id: u32,
    channel: StreamChannel,
) -> Result<()> {
    if chunk.request_id != request_id {
        bail!(
            "stream frame request id mismatch: expected {}, got {}",
            request_id,
            chunk.request_id
        );
    }
    if chunk.channel != channel {
        bail!(
            "unexpected stream channel: expected {:?}, got {:?}",
            channel,
            chunk.channel
        );
    }
    Ok(())
}

async fn send_error(
    stream: &mut TcpStream,
    request_id: u32,
    code: ErrorCode,
    err: anyhow::Error,
) -> Result<()> {
    let response = ControlResponse::Error {
        code,
        message: err.to_string(),
    };
    write_json_frame(stream, FrameKind::Response, request_id, &response).await?;
    Ok(())
}

#[cfg(unix)]
fn normalize_mode(mode: u32) -> u32 {
    let mode = mode & 0o7777;
    if mode == 0 { 0o644 } else { mode }
}

#[cfg(not(unix))]
fn normalize_mode(_mode: u32) -> u32 {
    0
}

#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn file_mode(_metadata: &std::fs::Metadata) -> u32 {
    0
}
