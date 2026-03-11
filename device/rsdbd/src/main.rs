use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs::File as StdFile, fs::OpenOptions as StdOpenOptions, io};

use std::path::PathBuf;

#[cfg(unix)]
use std::os::fd::FromRawFd as _;
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
use rustix_openpty::rustix::fd::IntoRawFd as _;
use rustix_openpty::rustix::termios::Winsize;
use rustix_openpty::{login_tty, openpty};
use tokio::fs::{File, OpenOptions, metadata};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
    platform: Arc<str>,
    shell_path: Arc<str>,
    exec_timeout: Duration,
    tcp_port: u16,
}

enum PtyInput {
    Data(Vec<u8>),
    Close,
}

enum PtyOutput {
    Data(Vec<u8>),
    Eof,
    Error(String),
}

#[derive(Clone)]
struct SharedLogWriter {
    file: Arc<Mutex<StdFile>>,
}

impl io::Write for SharedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        file.flush()
    }
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
    let device_name = detect_device_name();
    let platform = detect_platform();
    let shell_path = detect_default_shell();
    let state = ServerState {
        server_id: Arc::from(server_id),
        device_name: Arc::from(device_name),
        platform: Arc::from(platform),
        shell_path: Arc::from(shell_path),
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
            warn!(error = %err, "discovery socket stopped");
        }
    });

    loop {
        let (stream, peer) = listener.accept().await.context("accept failed")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, state).await {
                warn!(peer = %peer, error = %err, "connection closed with error");
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
        let writer = SharedLogWriter {
            file: Arc::new(Mutex::new(file)),
        };
        builder.with_writer(move || writer.clone()).init();
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

fn detect_device_name() -> String {
    detect_os_release_value("NAME")
        .or_else(|| detect_os_release_value("PRETTY_NAME"))
        .or_else(|| run_command("uname -s"))
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

fn detect_platform() -> String {
    let os_name = detect_os_name();
    let arch = std::env::consts::ARCH;
    format!("{os_name} ({arch})")
}

fn detect_os_name() -> String {
    // /etc/os-release PRETTY_NAME — standard on Tizen/Linux
    if let Some(name) = detect_os_release_value("PRETTY_NAME") {
        return name;
    }

    // `uname -sr` — works everywhere, gives OS name + version (e.g. "Linux 5.10.0")
    run_command("uname -sr").unwrap_or_else(|| std::env::consts::OS.to_string())
}

fn detect_os_release_value(key: &str) -> Option<String> {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                let value = line.strip_prefix(&format!("{key}="))?;
                let value = value.trim().trim_matches('"');
                (!value.is_empty()).then(|| value.to_string())
            })
        })
}

fn run_command(cmd: &str) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    let program = parts.next()?;
    let args: Vec<&str> = parts.collect();
    let output = std::process::Command::new(program)
        .args(&args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
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
                    platform: state.platform.to_string(),
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
    let frame = match timeout(INITIAL_REQUEST_TIMEOUT, read_frame(&mut stream)).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(ProtocolError::Io(err))) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(());
        }
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => bail!(
            "timed out waiting for initial request after {} seconds",
            INITIAL_REQUEST_TIMEOUT.as_secs()
        ),
    };

    let request_id = frame.header.request_id;
    let request: ControlRequest = decode_json(&frame, FrameKind::Request)?;
    handle_request(stream, request_id, request, &state).await
}

async fn handle_request(
    mut stream: TcpStream,
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
            write_json_frame(&mut stream, FrameKind::Response, request_id, &response).await?;
        }
        ControlRequest::GetCapabilities => {
            let response = ControlResponse::Capabilities {
                server_id: state.server_id.to_string(),
                capability: capabilities(),
            };
            write_json_frame(&mut stream, FrameKind::Response, request_id, &response).await?;
        }
        ControlRequest::Exec { command, args } => {
            let response = match execute_command(&command, &args, state).await {
                Ok(result) => result,
                Err(err) => ControlResponse::Error {
                    code: ErrorCode::ExecFailed,
                    message: err.to_string(),
                },
            };
            write_json_frame(&mut stream, FrameKind::Response, request_id, &response).await?;
        }
        ControlRequest::Push {
            path,
            mode,
            source_name,
        } => {
            if let Err(err) = handle_push(&mut stream, request_id, &path, mode, source_name).await {
                send_error(&mut stream, request_id, ErrorCode::FileTransferFailed, err).await?;
            }
        }
        ControlRequest::Pull { path } => {
            if let Err(err) = handle_pull(&mut stream, request_id, &path).await {
                let code = classify_pull_error(&err);
                send_error(&mut stream, request_id, code, err).await?;
            }
        }
        ControlRequest::Shell {
            command,
            args,
            pty,
            term,
            rows,
            cols,
        } => {
            if let Err(err) = handle_shell(
                stream, request_id, command, args, pty, term, rows, cols, state,
            )
            .await
            {
                return Err(err);
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
            "shell.pty".to_string(),
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

async fn handle_push(
    stream: &mut TcpStream,
    request_id: u32,
    path: &str,
    mode: u32,
    source_name: Option<String>,
) -> Result<()> {
    let resolved_path = resolve_push_destination(path, source_name.as_deref()).await?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(normalize_mode(mode));

    let mut file = options.open(&resolved_path).await.with_context(|| {
        format!(
            "failed to open remote path for write: {}",
            resolved_path.display()
        )
    })?;
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
        tokio::fs::set_permissions(
            &resolved_path,
            std::fs::Permissions::from_mode(normalize_mode(mode)),
        )
        .await
        .with_context(|| format!("failed to set permissions on {}", resolved_path.display()))?;
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

async fn resolve_push_destination(path: &str, source_name: Option<&str>) -> Result<PathBuf> {
    let requested = PathBuf::from(path);
    if requested.as_os_str().is_empty() {
        bail!("remote path must not be empty");
    }

    let remote_metadata = match metadata(&requested).await {
        Ok(metadata) => Some(metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("failed to stat remote path: {path}")),
    };

    if remote_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.is_dir())
        || path.ends_with('/')
    {
        let file_name = source_name
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("remote path refers to a directory but source file name is missing")
            })?;
        return Ok(requested.join(file_name));
    }

    Ok(requested)
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
    stream: TcpStream,
    request_id: u32,
    command: Option<String>,
    args: Vec<String>,
    pty: bool,
    term: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
    state: &ServerState,
) -> Result<()> {
    if pty {
        return handle_pty_shell(stream, request_id, command, args, term, rows, cols, state).await;
    }

    handle_pipe_shell(stream, request_id, command, args, state).await
}

async fn handle_pipe_shell(
    stream: TcpStream,
    request_id: u32,
    command: Option<String>,
    args: Vec<String>,
    state: &ServerState,
) -> Result<()> {
    let (display_command, mut child) = spawn_pipe_shell(command, args, &state.shell_path)?;
    let (mut reader, mut writer) = stream.into_split();
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        loop {
            let frame = read_frame(&mut reader).await;
            let should_stop = frame.is_err();
            if frame_tx.send(frame).await.is_err() {
                break;
            }
            if should_stop {
                break;
            }
        }
    });
    write_json_frame(
        &mut writer,
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
    let mut wait = Box::pin(child.wait());
    let mut stdout_buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
    let mut stderr_buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];

    while stdout_open || stderr_open || exit_status.is_none() {
        tokio::select! {
            frame = frame_rx.recv(), if stdin_open => {
                match frame {
                    Some(Ok(frame)) => {
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
                    Some(Err(ProtocolError::Io(err))) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        let _ = child_stdin.shutdown().await;
                        stdin_open = false;
                    }
                    Some(Err(err)) => {
                        send_error(&mut writer, request_id, ErrorCode::ExecFailed, err.into()).await?;
                        return Ok(());
                    }
                    None => {
                        let _ = child_stdin.shutdown().await;
                        stdin_open = false;
                    }
                }
            }
            read = child_stdout.read(&mut stdout_buffer), if stdout_open => {
                let read = read?;
                if read == 0 {
                    stdout_open = false;
                    write_stream_frame(&mut writer, request_id, StreamChannel::Stdout, true, &[]).await?;
                } else {
                    write_stream_frame(
                        &mut writer,
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
                    write_stream_frame(&mut writer, request_id, StreamChannel::Stderr, true, &[]).await?;
                } else {
                    write_stream_frame(
                        &mut writer,
                        request_id,
                        StreamChannel::Stderr,
                        false,
                        &stderr_buffer[..read],
                    ).await?;
                }
            }
            status = &mut wait, if exit_status.is_none() => {
                exit_status = Some(status.context("failed to wait for shell process")?);
            }
        }
    }

    if stdin_open {
        let _ = child_stdin.shutdown().await;
    }

    let status = match exit_status {
        Some(status) => status,
        None => wait.await.context("failed to wait for shell process")?,
    };
    write_json_frame(
        &mut writer,
        FrameKind::Response,
        request_id,
        &ControlResponse::ShellExit {
            status: status.code().unwrap_or(-1),
        },
    )
    .await?;
    Ok(())
}

async fn handle_pty_shell(
    stream: TcpStream,
    request_id: u32,
    command: Option<String>,
    args: Vec<String>,
    term: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
    state: &ServerState,
) -> Result<()> {
    let (display_command, mut child, controller) =
        spawn_pty_shell(command, args, term, rows, cols, &state.shell_path)?;
    let (mut reader, mut writer) = stream.into_split();
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        loop {
            let frame = read_frame(&mut reader).await;
            let should_stop = frame.is_err();
            if frame_tx.send(frame).await.is_err() {
                break;
            }
            if should_stop {
                break;
            }
        }
    });
    write_json_frame(
        &mut writer,
        FrameKind::Response,
        request_id,
        &ControlResponse::ShellStarted {
            command: display_command,
        },
    )
    .await?;

    let controller_reader = controller
        .try_clone()
        .context("failed to clone PTY controller for output")?;
    let (pty_output_tx, mut pty_output_rx) = tokio::sync::mpsc::channel(32);
    start_pty_output_pump(controller_reader, pty_output_tx);

    let (pty_input_tx, pty_input_rx) = tokio::sync::mpsc::channel(32);
    start_pty_input_pump(controller, pty_input_rx);
    let mut pty_input_tx = Some(pty_input_tx);
    let mut stdin_open = true;
    let mut stdout_open = true;
    let mut wait = Box::pin(child.wait());
    let mut exit_status = None;

    while stdout_open || exit_status.is_none() {
        tokio::select! {
            frame = frame_rx.recv(), if stdin_open => {
                match frame {
                    Some(Ok(frame)) => {
                        let chunk = decode_stream_frame(&frame)?;
                        expect_stream_chunk(&chunk, request_id, StreamChannel::Stdin)?;
                        if !chunk.payload.is_empty() {
                            if let Some(tx) = &pty_input_tx {
                                tx.send(PtyInput::Data(chunk.payload)).await
                                    .context("failed to forward PTY stdin chunk")?;
                            }
                        }
                        if chunk.eof {
                            if let Some(tx) = pty_input_tx.take() {
                                let _ = tx.send(PtyInput::Close).await;
                            }
                            stdin_open = false;
                        }
                    }
                    Some(Err(ProtocolError::Io(err))) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        if let Some(tx) = pty_input_tx.take() {
                            let _ = tx.send(PtyInput::Close).await;
                        }
                        stdin_open = false;
                    }
                    Some(Err(err)) => return Err(err.into()),
                    None => {
                        if let Some(tx) = pty_input_tx.take() {
                            let _ = tx.send(PtyInput::Close).await;
                        }
                        stdin_open = false;
                    }
                }
            }
            output = pty_output_rx.recv(), if stdout_open => {
                match output {
                    Some(PtyOutput::Data(data)) => {
                        write_stream_frame(
                            &mut writer,
                            request_id,
                            StreamChannel::Stdout,
                            false,
                            &data,
                        )
                        .await?;
                    }
                    Some(PtyOutput::Eof) | None => {
                        stdout_open = false;
                        write_stream_frame(&mut writer, request_id, StreamChannel::Stdout, true, &[]).await?;
                    }
                    Some(PtyOutput::Error(message)) => {
                        return Err(anyhow!("PTY stream failed: {message}"));
                    }
                }
            }
            status = &mut wait, if exit_status.is_none() => {
                exit_status = Some(status.context("failed to wait for shell process")?);
            }
        }
    }

    let status = exit_status.ok_or_else(|| anyhow!("shell process ended without exit status"))?;
    write_json_frame(
        &mut writer,
        FrameKind::Response,
        request_id,
        &ControlResponse::ShellExit {
            status: status.code().unwrap_or(-1),
        },
    )
    .await?;
    Ok(())
}

fn spawn_pipe_shell(
    command: Option<String>,
    args: Vec<String>,
    shell: &str,
) -> Result<(String, tokio::process::Child)> {
    let (display_command, program, argv) = match command {
        Some(command) if !command.trim().is_empty() => {
            let command_line = join_shell_command(&command, &args);
            (
                format!("{shell} -c {command_line}"),
                shell.to_string(),
                vec!["-c".to_string(), command_line],
            )
        }
        Some(_) => bail!("shell command must not be empty"),
        None => (
            format!("{shell} -i"),
            shell.to_string(),
            vec!["-i".to_string()],
        ),
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

fn spawn_pty_shell(
    command: Option<String>,
    args: Vec<String>,
    term: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
    shell: &str,
) -> Result<(String, tokio::process::Child, StdFile)> {
    let (display_command, program, argv) = match command {
        Some(command) if !command.trim().is_empty() => {
            let command_line = join_shell_command(&command, &args);
            (
                format!("{shell} -c {command_line}"),
                shell.to_string(),
                vec!["-c".to_string(), command_line],
            )
        }
        Some(_) => bail!("shell command must not be empty"),
        None => (
            format!("{shell} -i"),
            shell.to_string(),
            vec!["-i".to_string()],
        ),
    };

    let winsize = shell_winsize(rows, cols);
    let pty = openpty(None, winsize.as_ref())
        .map_err(|err| anyhow!("failed to allocate PTY: os error {}", err.raw_os_error()))?;
    let controller = unsafe { StdFile::from_raw_fd(pty.controller.into_raw_fd()) };
    let mut user = Some(pty.user);

    let mut child = Command::new(&program);
    child.args(&argv);
    if let Some(term) = term.filter(|value| !value.trim().is_empty()) {
        child.env("TERM", term);
    }
    // SAFETY: `login_tty` is run in the post-fork child process immediately before `exec`.
    unsafe {
        child.pre_exec(move || {
            let user = user
                .take()
                .ok_or_else(|| std::io::Error::other("PTY slave already consumed"))?;
            login_tty(user).map_err(rustix_errno_to_io_error)?;
            Ok(())
        });
    }

    let child = child
        .spawn()
        .with_context(|| format!("failed to spawn PTY shell command: {program}"))?;
    Ok((display_command, child, controller))
}

fn start_pty_output_pump(mut controller: StdFile, tx: tokio::sync::mpsc::Sender<PtyOutput>) {
    std::thread::spawn(move || {
        let mut buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
        loop {
            match std::io::Read::read(&mut controller, &mut buffer) {
                Ok(0) => {
                    let _ = tx.blocking_send(PtyOutput::Eof);
                    break;
                }
                Ok(read) => {
                    if tx
                        .blocking_send(PtyOutput::Data(buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) if is_pty_closed_error(&err) => {
                    let _ = tx.blocking_send(PtyOutput::Eof);
                    break;
                }
                Err(err) => {
                    let _ = tx.blocking_send(PtyOutput::Error(err.to_string()));
                    break;
                }
            }
        }
    });
}

fn start_pty_input_pump(mut controller: StdFile, mut rx: tokio::sync::mpsc::Receiver<PtyInput>) {
    std::thread::spawn(move || {
        while let Some(input) = rx.blocking_recv() {
            match input {
                PtyInput::Data(data) => {
                    if data.is_empty() {
                        continue;
                    }
                    if let Err(err) = std::io::Write::write_all(&mut controller, &data) {
                        if !is_pty_closed_error(&err) {
                            trace!(error = %err, "PTY input pump stopped");
                        }
                        break;
                    }
                }
                PtyInput::Close => break,
            }
        }
    });
}

fn shell_winsize(rows: Option<u16>, cols: Option<u16>) -> Option<Winsize> {
    match (
        rows.filter(|value| *value > 0),
        cols.filter(|value| *value > 0),
    ) {
        (Some(rows), Some(cols)) => Some(Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        _ => None,
    }
}

fn is_pty_closed_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
    ) || err.raw_os_error() == Some(rustix_openpty::rustix::io::Errno::IO.raw_os_error())
}

fn rustix_errno_to_io_error(err: rustix_openpty::rustix::io::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(err.raw_os_error())
}

fn detect_default_shell() -> String {
    if let Some(shell) = std::env::var("RSDB_DEFAULT_SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return shell;
    }

    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh", "/usr/bin/sh"] {
        if std::fs::metadata(candidate).is_ok() {
            return candidate.to_string();
        }
    }

    "/bin/sh".to_string()
}

fn classify_pull_error(err: &anyhow::Error) -> ErrorCode {
    if has_io_error_kind(err, std::io::ErrorKind::NotFound) {
        ErrorCode::NotFound
    } else {
        ErrorCode::FileTransferFailed
    }
}

fn has_io_error_kind(err: &anyhow::Error, expected: std::io::ErrorKind) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == expected)
    })
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

async fn send_error<W>(
    stream: &mut W,
    request_id: u32,
    code: ErrorCode,
    err: anyhow::Error,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
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
