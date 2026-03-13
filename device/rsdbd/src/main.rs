use base64::Engine as _;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs::File as StdFile, fs::OpenOptions as StdOpenOptions, io};

use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::fd::FromRawFd as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use glob::{MatchOptions, glob_with};
use rsdb_proto::{
    AgentFsEntry, AgentFsMkdirResult, AgentFsMoveResult, AgentFsReadResult, AgentFsRmResult,
    AgentFsStat, AgentFsWriteResult, CapabilitySet, ContentEncoding, ControlRequest,
    ControlResponse, DEFAULT_STREAM_CHUNK_SIZE, DiscoveryRequest, DiscoveryResponse, ErrorCode,
    FrameKind, FsEntryKind, HEADER_LEN, HashAlgorithm, MAX_DISCOVERY_PAYLOAD_LEN, PROTOCOL_VERSION,
    ProtocolError, StreamChannel, TransferEntry, TransferEntryKind, TransferRoot,
    decode_discovery_message, decode_json, decode_stream_frame, encode_discovery_message,
    read_frame, read_stream_frame_into, write_json_frame, write_stream_frame,
};
use rustix_openpty::rustix::fd::IntoRawFd as _;
use rustix_openpty::rustix::termios::Winsize;
use rustix_openpty::{login_tty, openpty};
use sha2::{Digest, Sha256};
use tokio::fs::{File, OpenOptions, metadata};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Command;
use tokio::task::spawn_blocking;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_AGENT_FS_BYTES: u64 = 4 * 1024 * 1024;

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

#[derive(Debug, Clone)]
struct BatchFileSource {
    path: PathBuf,
    size: u64,
}

#[derive(Debug, Clone)]
struct BatchManifest {
    roots: Vec<TransferRoot>,
    entries: Vec<TransferEntry>,
    files: Vec<BatchFileSource>,
    total_bytes: u64,
}

#[derive(Debug)]
struct FsPreconditionError(String);

impl std::fmt::Display for FsPreconditionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FsPreconditionError {}

#[derive(Debug)]
struct FsInvalidRequestError(String);

impl std::fmt::Display for FsInvalidRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FsInvalidRequestError {}

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
        if let Err(err) = stream.set_nodelay(true) {
            warn!(peer = %peer, error = %err, "failed to set TCP_NODELAY");
        }
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
        ControlRequest::Exec {
            command,
            args,
            cwd,
            timeout_secs,
            stream: stream_output,
        } => {
            return handle_exec_request(
                stream,
                request_id,
                &command,
                &args,
                cwd.as_deref(),
                timeout_secs,
                stream_output,
                state,
            )
            .await;
        }
        ControlRequest::FsStat { path, hash } => {
            if let Err(err) = handle_fs_stat(&mut stream, request_id, &path, hash).await {
                let code = classify_fs_error(&err);
                send_error(&mut stream, request_id, code, err).await?;
            }
        }
        ControlRequest::FsList {
            path,
            recursive,
            max_depth,
            include_hidden,
            hash,
        } => {
            if let Err(err) = handle_fs_list(
                &mut stream,
                request_id,
                &path,
                recursive,
                max_depth,
                include_hidden,
                hash,
            )
            .await
            {
                let code = classify_fs_error(&err);
                send_error(&mut stream, request_id, code, err).await?;
            }
        }
        ControlRequest::FsRead {
            path,
            encoding,
            max_bytes,
        } => {
            if let Err(err) =
                handle_fs_read(&mut stream, request_id, &path, encoding, max_bytes).await
            {
                let code = classify_fs_error(&err);
                send_error(&mut stream, request_id, code, err).await?;
            }
        }
        ControlRequest::FsWrite {
            path,
            content,
            encoding,
            mode,
            create_parent,
            atomic,
            if_missing,
            if_sha256,
        } => {
            if let Err(err) = handle_fs_write(
                &mut stream,
                request_id,
                &path,
                &content,
                encoding,
                mode,
                create_parent,
                atomic,
                if_missing,
                if_sha256.as_deref(),
            )
            .await
            {
                let code = classify_fs_error(&err);
                send_error(&mut stream, request_id, code, err).await?;
            }
        }
        ControlRequest::FsMkdir {
            path,
            parents,
            mode,
        } => {
            if let Err(err) = handle_fs_mkdir(&mut stream, request_id, &path, parents, mode).await {
                let code = classify_fs_error(&err);
                send_error(&mut stream, request_id, code, err).await?;
            }
        }
        ControlRequest::FsRm {
            path,
            recursive,
            force,
            if_exists,
        } => {
            if let Err(err) =
                handle_fs_rm(&mut stream, request_id, &path, recursive, force, if_exists).await
            {
                let code = classify_fs_error(&err);
                send_error(&mut stream, request_id, code, err).await?;
            }
        }
        ControlRequest::FsMove {
            source,
            destination,
            overwrite,
        } => {
            if let Err(err) =
                handle_fs_move(&mut stream, request_id, &source, &destination, overwrite).await
            {
                let code = classify_fs_error(&err);
                send_error(&mut stream, request_id, code, err).await?;
            }
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
        ControlRequest::PushBatch {
            destination,
            roots,
            entries,
        } => {
            if let Err(err) =
                handle_push_batch(&mut stream, request_id, &destination, &roots, &entries).await
            {
                send_error(&mut stream, request_id, ErrorCode::FileTransferFailed, err).await?;
            }
        }
        ControlRequest::PullBatch { sources } => {
            if let Err(err) = handle_pull_batch(&mut stream, request_id, &sources).await {
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
            "exec.direct".to_string(),
            "exec.stream".to_string(),
            "agent.exec.v1".to_string(),
            "agent.fs.stat.v1".to_string(),
            "agent.fs.list.v1".to_string(),
            "agent.fs.read.v1".to_string(),
            "agent.fs.write.v1".to_string(),
            "agent.fs.mkdir.v1".to_string(),
            "agent.fs.rm.v1".to_string(),
            "agent.fs.move.v1".to_string(),
            "agent.transfer.push.v1".to_string(),
            "agent.transfer.pull.v1".to_string(),
            "shell.exec".to_string(),
            "shell.interactive".to_string(),
            "shell.pty".to_string(),
            "fs.push".to_string(),
            "fs.pull".to_string(),
            "fs.push.batch".to_string(),
            "fs.pull.batch".to_string(),
        ],
    }
}

async fn handle_exec_request(
    mut stream: TcpStream,
    request_id: u32,
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_secs: Option<u64>,
    stream_output: bool,
    state: &ServerState,
) -> Result<()> {
    if stream_output {
        return match stream_command_output(
            &mut stream,
            request_id,
            command,
            args,
            cwd,
            timeout_secs,
            state,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                let response = ControlResponse::Error {
                    code: ErrorCode::ExecFailed,
                    message: err.to_string(),
                };
                write_json_frame(&mut stream, FrameKind::Response, request_id, &response).await?;
                Ok(())
            }
        };
    }

    let response = match execute_command(command, args, cwd, timeout_secs, state).await {
        Ok(result) => result,
        Err(err) => ControlResponse::Error {
            code: ErrorCode::ExecFailed,
            message: err.to_string(),
        },
    };
    write_json_frame(&mut stream, FrameKind::Response, request_id, &response).await?;
    Ok(())
}

async fn execute_command(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_secs: Option<u64>,
    state: &ServerState,
) -> Result<ControlResponse> {
    if command.trim().is_empty() {
        return Err(anyhow!("command must not be empty"));
    }

    let mut child = Command::new(command);
    child.args(args);
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }
    child.stdout(Stdio::piped());
    child.stderr(Stdio::piped());
    child.kill_on_drop(true);

    let exec_timeout_duration = exec_timeout(timeout_secs, state.exec_timeout);

    let output = match timeout(exec_timeout_duration, child.output()).await {
        Ok(output) => output.with_context(|| format!("failed to execute command: {command}"))?,
        Err(_) => {
            return Ok(ControlResponse::Error {
                code: ErrorCode::ExecTimeout,
                message: format!(
                    "command timed out after {} seconds",
                    exec_timeout_duration.as_secs()
                ),
            });
        }
    };

    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(ControlResponse::ExecResult {
        status,
        stdout,
        stderr,
        timed_out: false,
    })
}

async fn stream_command_output(
    stream: &mut TcpStream,
    request_id: u32,
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_secs: Option<u64>,
    state: &ServerState,
) -> Result<()> {
    if command.trim().is_empty() {
        bail!("command must not be empty");
    }

    let exec_timeout_duration = exec_timeout(timeout_secs, state.exec_timeout);
    let stream_result = timeout(exec_timeout_duration, async {
        let mut child = Command::new(command);
        child.args(args);
        if let Some(cwd) = cwd {
            child.current_dir(cwd);
        }
        child.stdout(Stdio::piped());
        child.stderr(Stdio::piped());
        child.kill_on_drop(true);

        let mut child = child
            .spawn()
            .with_context(|| format!("failed to execute command: {command}"))?;
        let mut child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("child stdout was not captured"))?;
        let mut child_stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("child stderr was not captured"))?;

        let mut writer = tokio::io::BufWriter::new(&mut *stream);
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut exit_status = None;
        let mut wait = Box::pin(child.wait());
        let mut stdout_buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
        let mut stderr_buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];

        while stdout_open || stderr_open || exit_status.is_none() {
            tokio::select! {
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
                    writer.flush().await?;
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
                    writer.flush().await?;
                }
                status = &mut wait, if exit_status.is_none() => {
                    exit_status = Some(status.context("failed to wait for exec process")?);
                }
            }
        }

        let status = match exit_status {
            Some(status) => status,
            None => wait.await.context("failed to wait for exec process")?,
        };
        write_json_frame(
            &mut writer,
            FrameKind::Response,
            request_id,
            &ControlResponse::ExecResult {
                status: status.code().unwrap_or(-1),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            },
        )
        .await?;
        writer.flush().await?;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match stream_result {
        Ok(result) => result,
        Err(_) => {
            write_json_frame(
                stream,
                FrameKind::Response,
                request_id,
                &ControlResponse::Error {
                    code: ErrorCode::ExecTimeout,
                    message: format!(
                        "command timed out after {} seconds",
                        exec_timeout_duration.as_secs()
                    ),
                },
            )
            .await?;
            Ok(())
        }
    }
}

fn exec_timeout(timeout_secs: Option<u64>, default_timeout: Duration) -> Duration {
    timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(default_timeout)
}

async fn handle_fs_stat(
    stream: &mut TcpStream,
    request_id: u32,
    path: &str,
    hash: Option<HashAlgorithm>,
) -> Result<()> {
    let requested = validate_fs_path(path)?;
    let stat = run_blocking_fs({
        let requested = requested.clone();
        move || blocking_fs_stat(requested, hash)
    })
    .await?;

    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::FsStat { stat },
    )
    .await?;
    Ok(())
}

async fn handle_fs_list(
    stream: &mut TcpStream,
    request_id: u32,
    path: &str,
    recursive: bool,
    max_depth: Option<u32>,
    include_hidden: bool,
    hash: Option<HashAlgorithm>,
) -> Result<()> {
    let requested = validate_fs_path(path)?;
    let entries = run_blocking_fs({
        let requested = requested.clone();
        move || blocking_fs_list(requested, recursive, max_depth, include_hidden, hash)
    })
    .await?;

    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::FsList { entries },
    )
    .await?;
    Ok(())
}

async fn handle_fs_read(
    stream: &mut TcpStream,
    request_id: u32,
    path: &str,
    encoding: ContentEncoding,
    max_bytes: Option<u64>,
) -> Result<()> {
    let requested = validate_fs_path(path)?;
    let metadata = std::fs::symlink_metadata(&requested)
        .with_context(|| format!("failed to stat {}", requested.display()))?;
    if !metadata.is_file() {
        return Err(FsInvalidRequestError(format!(
            "path is not a regular file: {}",
            requested.display()
        ))
        .into());
    }

    let read_limit = max_bytes
        .unwrap_or(MAX_AGENT_FS_BYTES)
        .min(MAX_AGENT_FS_BYTES);
    let file = File::open(&requested)
        .await
        .with_context(|| format!("failed to open {}", requested.display()))?;
    let mut bytes = Vec::new();
    let mut take = file.take(read_limit.saturating_add(1));
    take.read_to_end(&mut bytes).await?;
    let truncated = bytes.len() as u64 > read_limit;
    if truncated {
        bytes.truncate(read_limit as usize);
    }

    let bytes_len = bytes.len() as u64;
    let content = encode_fs_content(bytes, &encoding)?;
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::FsReadResult {
            result: AgentFsReadResult {
                path: requested.display().to_string(),
                encoding,
                content,
                bytes: bytes_len,
                truncated,
            },
        },
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_fs_write(
    stream: &mut TcpStream,
    request_id: u32,
    path: &str,
    content: &str,
    encoding: ContentEncoding,
    mode: Option<u32>,
    create_parent: bool,
    atomic: bool,
    if_missing: bool,
    if_sha256: Option<&str>,
) -> Result<()> {
    let requested = validate_fs_path(path)?;
    let bytes = decode_fs_content(content, &encoding)?;
    if bytes.len() as u64 > MAX_AGENT_FS_BYTES {
        return Err(FsInvalidRequestError(format!(
            "write exceeds maximum payload of {MAX_AGENT_FS_BYTES} bytes"
        ))
        .into());
    }

    if let Some(parent) = requested.parent() {
        if !parent.as_os_str().is_empty() && create_parent {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }

    let existing_metadata = match std::fs::symlink_metadata(&requested) {
        Ok(metadata) => Some(metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| format!("failed to stat {}", requested.display()));
        }
    };
    if existing_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.is_dir())
    {
        return Err(
            FsInvalidRequestError(format!("path is a directory: {}", requested.display())).into(),
        );
    }
    if if_missing && existing_metadata.is_some() {
        return Err(fs_precondition_failed(format!(
            "path already exists: {}",
            requested.display()
        )));
    }
    let desired_sha256 = sha256_bytes(&bytes);
    let existing_file_metadata = existing_metadata
        .as_ref()
        .filter(|metadata| metadata.is_file());
    let mut previous_sha256 = None;

    if let Some(expected) = if_sha256 {
        let metadata = existing_metadata.as_ref().ok_or_else(|| {
            fs_precondition_failed(format!("path does not exist: {}", requested.display()))
        })?;
        if !metadata.is_file() {
            return Err(fs_precondition_failed(format!(
                "path is not a regular file: {}",
                requested.display()
            )));
        }
        let actual = sha256_path(&requested)?;
        if actual != expected {
            return Err(fs_precondition_failed(format!(
                "sha256 precondition failed for {}",
                requested.display()
            )));
        }
        previous_sha256 = Some(actual);
    }

    let changed = match existing_file_metadata {
        None => true,
        Some(metadata) if metadata.len() != bytes.len() as u64 => true,
        Some(_) => {
            if previous_sha256.is_none() {
                previous_sha256 = Some(sha256_path(&requested)?);
            }
            previous_sha256.as_deref() != Some(desired_sha256.as_str())
        }
    };

    if atomic {
        write_atomic_file(&requested, &bytes, mode).await?;
    } else {
        write_direct_file(&requested, &bytes, mode).await?;
    }

    let final_mode = std::fs::symlink_metadata(&requested)
        .ok()
        .map(|metadata| file_mode(&metadata));
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::FsWriteResult {
            result: AgentFsWriteResult {
                path: requested.display().to_string(),
                bytes_written: bytes.len() as u64,
                mode: final_mode,
                sha256: Some(desired_sha256.clone()),
                changed,
            },
        },
    )
    .await?;
    Ok(())
}

async fn handle_fs_mkdir(
    stream: &mut TcpStream,
    request_id: u32,
    path: &str,
    parents: bool,
    mode: Option<u32>,
) -> Result<()> {
    let requested = validate_fs_path(path)?;
    let created = match std::fs::symlink_metadata(&requested) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(fs_precondition_failed(format!(
                    "path exists and is not a directory: {}",
                    requested.display()
                )));
            }
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if parents {
                tokio::fs::create_dir_all(&requested)
                    .await
                    .with_context(|| format!("failed to create {}", requested.display()))?;
            } else {
                tokio::fs::create_dir(&requested)
                    .await
                    .with_context(|| format!("failed to create {}", requested.display()))?;
            }
            true
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to stat {}", requested.display()));
        }
    };

    #[cfg(unix)]
    if let Some(mode) = mode {
        tokio::fs::set_permissions(
            &requested,
            std::fs::Permissions::from_mode(normalize_mode(mode)),
        )
        .await
        .with_context(|| format!("failed to set permissions on {}", requested.display()))?;
    }

    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::FsMkdirResult {
            result: AgentFsMkdirResult {
                path: requested.display().to_string(),
                created,
            },
        },
    )
    .await?;
    Ok(())
}

async fn handle_fs_rm(
    stream: &mut TcpStream,
    request_id: u32,
    path: &str,
    recursive: bool,
    force: bool,
    if_exists: bool,
) -> Result<()> {
    let requested = validate_fs_path(path)?;
    let metadata = match std::fs::symlink_metadata(&requested) {
        Ok(metadata) => Some(metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && (force || if_exists) => None,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(err).with_context(|| format!("failed to stat {}", requested.display()));
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to stat {}", requested.display()));
        }
    };

    let removed = if let Some(metadata) = metadata {
        if metadata.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&requested)
                    .await
                    .with_context(|| format!("failed to remove {}", requested.display()))?;
            } else {
                tokio::fs::remove_dir(&requested)
                    .await
                    .with_context(|| format!("failed to remove {}", requested.display()))?;
            }
        } else {
            tokio::fs::remove_file(&requested)
                .await
                .with_context(|| format!("failed to remove {}", requested.display()))?;
        }
        true
    } else {
        false
    };

    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::FsRmResult {
            result: AgentFsRmResult {
                path: requested.display().to_string(),
                removed,
            },
        },
    )
    .await?;
    Ok(())
}

async fn handle_fs_move(
    stream: &mut TcpStream,
    request_id: u32,
    source: &str,
    destination: &str,
    overwrite: bool,
) -> Result<()> {
    let source_path = validate_fs_path(source)?;
    let destination_path = validate_fs_path(destination)?;
    std::fs::symlink_metadata(&source_path)
        .with_context(|| format!("failed to stat {}", source_path.display()))?;

    let destination_metadata = match std::fs::symlink_metadata(&destination_path) {
        Ok(metadata) => Some(metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to stat {}", destination_path.display()));
        }
    };

    if destination_metadata.is_some() && !overwrite {
        return Err(fs_precondition_failed(format!(
            "destination already exists: {}",
            destination_path.display()
        )));
    }

    let overwritten = destination_metadata.is_some();
    if let Some(metadata) = destination_metadata {
        if metadata.is_dir() {
            tokio::fs::remove_dir_all(&destination_path)
                .await
                .with_context(|| format!("failed to remove {}", destination_path.display()))?;
        } else {
            tokio::fs::remove_file(&destination_path)
                .await
                .with_context(|| format!("failed to remove {}", destination_path.display()))?;
        }
    }

    tokio::fs::rename(&source_path, &destination_path)
        .await
        .with_context(|| {
            format!(
                "failed to move {} to {}",
                source_path.display(),
                destination_path.display()
            )
        })?;

    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::FsMoveResult {
            result: AgentFsMoveResult {
                source: source_path.display().to_string(),
                destination: destination_path.display().to_string(),
                overwritten,
            },
        },
    )
    .await?;
    Ok(())
}

fn validate_fs_path(path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty() {
        return Err(FsInvalidRequestError("path must not be empty".to_string()).into());
    }
    Ok(path)
}

async fn run_blocking_fs<F, T>(operation: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    spawn_blocking(operation)
        .await
        .map_err(|err| anyhow!("blocking fs task failed: {err}"))?
}

fn blocking_fs_stat(path: PathBuf, hash: Option<HashAlgorithm>) -> Result<AgentFsStat> {
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => build_fs_stat(&path, &metadata, hash.as_ref()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(AgentFsStat {
            path: path.display().to_string(),
            exists: false,
            kind: None,
            size: None,
            mode: None,
            mtime_unix_ms: None,
            sha256: None,
        }),
        Err(err) => Err(err).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn blocking_fs_list(
    path: PathBuf,
    recursive: bool,
    max_depth: Option<u32>,
    include_hidden: bool,
    hash: Option<HashAlgorithm>,
) -> Result<Vec<AgentFsEntry>> {
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let mut entries = Vec::new();

    if metadata.is_dir() {
        collect_fs_entries(
            &path,
            recursive,
            max_depth,
            include_hidden,
            hash.as_ref(),
            0,
            &mut entries,
        )?;
    } else {
        entries.push(build_fs_entry(&path, &metadata, hash.as_ref())?);
    }

    Ok(entries)
}

fn build_fs_stat(
    path: &Path,
    metadata: &std::fs::Metadata,
    hash: Option<&HashAlgorithm>,
) -> Result<AgentFsStat> {
    Ok(AgentFsStat {
        path: path.display().to_string(),
        exists: true,
        kind: Some(fs_entry_kind(metadata)),
        size: Some(metadata.len()),
        mode: Some(file_mode(metadata)),
        mtime_unix_ms: metadata_mtime_unix_ms(metadata),
        sha256: match hash {
            Some(HashAlgorithm::Sha256) if metadata.is_file() => Some(sha256_path(path)?),
            _ => None,
        },
    })
}

fn collect_fs_entries(
    path: &Path,
    recursive: bool,
    max_depth: Option<u32>,
    include_hidden: bool,
    hash: Option<&HashAlgorithm>,
    depth: u32,
    entries: &mut Vec<AgentFsEntry>,
) -> Result<()> {
    let mut children = std::fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read directory {}", path.display()))?;
    children.sort();

    for child in children {
        let hidden = child
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'));
        if hidden && !include_hidden {
            continue;
        }

        let metadata = std::fs::symlink_metadata(&child)
            .with_context(|| format!("failed to stat {}", child.display()))?;
        entries.push(build_fs_entry(&child, &metadata, hash)?);

        let child_depth = depth + 1;
        let can_descend =
            recursive && metadata.is_dir() && max_depth.is_none_or(|limit| child_depth < limit);
        if can_descend {
            collect_fs_entries(
                &child,
                recursive,
                max_depth,
                include_hidden,
                hash,
                child_depth,
                entries,
            )?;
        }
    }

    Ok(())
}

fn build_fs_entry(
    path: &Path,
    metadata: &std::fs::Metadata,
    hash: Option<&HashAlgorithm>,
) -> Result<AgentFsEntry> {
    Ok(AgentFsEntry {
        path: path.display().to_string(),
        kind: fs_entry_kind(metadata),
        size: metadata.len(),
        mode: file_mode(metadata),
        mtime_unix_ms: metadata_mtime_unix_ms(metadata),
        sha256: match hash {
            Some(HashAlgorithm::Sha256) if metadata.is_file() => Some(sha256_path(path)?),
            _ => None,
        },
    })
}

fn fs_entry_kind(metadata: &std::fs::Metadata) -> FsEntryKind {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        FsEntryKind::File
    } else if file_type.is_dir() {
        FsEntryKind::Directory
    } else if file_type.is_symlink() {
        FsEntryKind::Symlink
    } else {
        FsEntryKind::Other
    }
}

fn metadata_mtime_unix_ms(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn encode_fs_content(bytes: Vec<u8>, encoding: &ContentEncoding) -> Result<String> {
    match encoding {
        ContentEncoding::Utf8 => String::from_utf8(bytes)
            .map_err(|_| FsInvalidRequestError("file is not valid UTF-8".to_string()).into()),
        ContentEncoding::Base64 => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
    }
}

fn decode_fs_content(content: &str, encoding: &ContentEncoding) -> Result<Vec<u8>> {
    match encoding {
        ContentEncoding::Utf8 => Ok(content.as_bytes().to_vec()),
        ContentEncoding::Base64 => base64::engine::general_purpose::STANDARD
            .decode(content)
            .map_err(|err| FsInvalidRequestError(format!("invalid base64 content: {err}")).into()),
    }
}

async fn write_direct_file(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        options.mode(normalize_mode(mode));
    }

    let mut file = options
        .open(path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(bytes).await?;
    file.flush().await?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(normalize_mode(mode)))
            .await
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

async fn write_atomic_file(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let temp_path = atomic_temp_path(path);
    let result = async {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).truncate(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            options.mode(normalize_mode(mode));
        }
        let mut file = options
            .open(&temp_path)
            .await
            .with_context(|| format!("failed to open {}", temp_path.display()))?;
        file.write_all(bytes).await?;
        file.flush().await?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            tokio::fs::set_permissions(
                &temp_path,
                std::fs::Permissions::from_mode(normalize_mode(mode)),
            )
            .await
            .with_context(|| format!("failed to set permissions on {}", temp_path.display()))?;
        }
        tokio::fs::rename(&temp_path, path).await.with_context(|| {
            format!(
                "failed to rename {} to {}",
                temp_path.display(),
                path.display()
            )
        })?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    result
}

fn atomic_temp_path(path: &Path) -> PathBuf {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("rsdb-temp");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{file_name}.rsdb-tmp-{nonce}"))
}

fn sha256_path(path: &Path) -> Result<String> {
    let mut file = StdFile::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(&hasher.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn fs_precondition_failed(message: impl Into<String>) -> anyhow::Error {
    FsPreconditionError(message.into()).into()
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
    let mut chunk_payload = Vec::with_capacity(DEFAULT_STREAM_CHUNK_SIZE);
    loop {
        let chunk =
            read_file_stream_chunk_into(stream, request_id, "push", &mut chunk_payload).await?;

        if !chunk_payload.is_empty() {
            file.write_all(&chunk_payload).await?;
            bytes_written += chunk_payload.len() as u64;
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

    let mut writer =
        tokio::io::BufWriter::with_capacity(HEADER_LEN + DEFAULT_STREAM_CHUNK_SIZE, &mut *stream);
    write_json_frame(
        &mut writer,
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
            &mut writer,
            request_id,
            StreamChannel::File,
            false,
            &buffer[..read],
        )
        .await?;
    }

    write_stream_frame(&mut writer, request_id, StreamChannel::File, true, &[]).await?;
    write_json_frame(
        &mut writer,
        FrameKind::Response,
        request_id,
        &ControlResponse::PullComplete { bytes_sent },
    )
    .await?;
    Ok(())
}

async fn handle_push_batch(
    stream: &mut TcpStream,
    request_id: u32,
    destination: &str,
    roots: &[TransferRoot],
    entries: &[TransferEntry],
) -> Result<()> {
    let root_paths = resolve_batch_destination_roots(destination, roots).await?;
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::PushBatchReady,
    )
    .await?;

    let mut entries_written = 0_u64;
    let mut bytes_written = 0_u64;
    let mut chunk_payload = Vec::with_capacity(DEFAULT_STREAM_CHUNK_SIZE);
    for entry in entries {
        let output_path = resolve_transfer_entry_path(&root_paths, entry)?;
        match entry.kind {
            TransferEntryKind::Directory => {
                tokio::fs::create_dir_all(&output_path)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to create remote directory {}",
                            output_path.display()
                        )
                    })?;
                #[cfg(unix)]
                if entry.mode != 0 {
                    tokio::fs::set_permissions(
                        &output_path,
                        std::fs::Permissions::from_mode(entry.mode & 0o7777),
                    )
                    .await
                    .with_context(|| {
                        format!("failed to set permissions on {}", output_path.display())
                    })?;
                }
            }
            TransferEntryKind::File => {
                if let Some(parent) = output_path.parent() {
                    if !parent.as_os_str().is_empty() {
                        tokio::fs::create_dir_all(parent).await.with_context(|| {
                            format!("failed to create remote directory {}", parent.display())
                        })?;
                    }
                }

                let mut options = OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                options.mode(normalize_mode(entry.mode));

                let mut file = options.open(&output_path).await.with_context(|| {
                    format!(
                        "failed to open remote path for write: {}",
                        output_path.display()
                    )
                })?;
                let mut remaining = entry.size;
                while remaining > 0 {
                    let chunk =
                        read_file_stream_chunk_into(stream, request_id, "push", &mut chunk_payload)
                            .await?;
                    if chunk.eof {
                        bail!(
                            "unexpected end of push stream while writing {}",
                            output_path.display()
                        );
                    }
                    if chunk_payload.len() as u64 > remaining {
                        bail!(
                            "push stream overflow while writing {}",
                            output_path.display()
                        );
                    }
                    if !chunk_payload.is_empty() {
                        file.write_all(&chunk_payload).await?;
                        remaining -= chunk_payload.len() as u64;
                        bytes_written += chunk_payload.len() as u64;
                    }
                }

                file.flush().await?;
                #[cfg(unix)]
                if entry.mode != 0 {
                    tokio::fs::set_permissions(
                        &output_path,
                        std::fs::Permissions::from_mode(normalize_mode(entry.mode)),
                    )
                    .await
                    .with_context(|| {
                        format!("failed to set permissions on {}", output_path.display())
                    })?;
                }
            }
        }
        entries_written += 1;
    }

    expect_file_stream_eof(stream, request_id, "push", &mut chunk_payload).await?;
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::PushBatchComplete {
            entries_written,
            bytes_written,
        },
    )
    .await?;
    Ok(())
}

async fn handle_pull_batch(
    stream: &mut TcpStream,
    request_id: u32,
    sources: &[String],
) -> Result<()> {
    let manifest = build_batch_manifest_from_sources(sources)?;
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::PullBatchMetadata {
            roots: manifest.roots.clone(),
            entries: manifest.entries.clone(),
            total_bytes: manifest.total_bytes,
        },
    )
    .await?;

    let bytes_sent = stream_batch_files(stream, request_id, &manifest.files).await?;
    if bytes_sent != manifest.total_bytes {
        bail!(
            "pull size mismatch before completion: expected {} bytes, streamed {bytes_sent}",
            manifest.total_bytes
        );
    }
    write_stream_frame(stream, request_id, StreamChannel::File, true, &[]).await?;
    write_json_frame(
        stream,
        FrameKind::Response,
        request_id,
        &ControlResponse::PullBatchComplete {
            entries_sent: manifest.entries.len() as u64,
            bytes_sent,
        },
    )
    .await?;
    Ok(())
}

fn build_batch_manifest_from_sources(source_specs: &[String]) -> Result<BatchManifest> {
    let sources = expand_remote_sources(source_specs)?;
    let mut roots = Vec::with_capacity(sources.len());
    let mut entries = Vec::new();
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;

    for source in sources {
        let metadata = std::fs::symlink_metadata(&source)
            .with_context(|| format!("failed to stat remote path {}", source.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "symbolic links are not supported in batch pull: {}",
                source.display()
            );
        }

        let kind = if metadata.is_dir() {
            TransferEntryKind::Directory
        } else if metadata.is_file() {
            TransferEntryKind::File
        } else {
            bail!("unsupported remote path type: {}", source.display());
        };

        let root_index = roots.len() as u32;
        roots.push(TransferRoot {
            source_name: transfer_source_name(&source)?,
            kind: kind.clone(),
            mode: file_mode(&metadata),
        });

        append_manifest_entry(
            root_index,
            &source,
            Path::new(""),
            &metadata,
            &mut entries,
            &mut files,
            &mut total_bytes,
        )?;
    }

    ensure_unique_root_names(&roots)?;
    Ok(BatchManifest {
        roots,
        entries,
        files,
        total_bytes,
    })
}

fn expand_remote_sources(source_specs: &[String]) -> Result<Vec<PathBuf>> {
    let mut sources = Vec::new();
    for spec in source_specs {
        let literal_path = PathBuf::from(spec);
        if has_glob_pattern(spec) && !literal_path.exists() {
            let mut matches = Vec::new();
            for entry in glob_with(spec, glob_match_options())
                .with_context(|| format!("invalid remote glob pattern: {spec}"))?
            {
                matches.push(
                    entry.with_context(|| format!("failed to resolve remote match for {spec}"))?,
                );
            }
            matches.sort();
            if matches.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("remote pattern matched no paths: {spec}"),
                )
                .into());
            }
            sources.extend(matches);
            continue;
        }

        std::fs::metadata(&literal_path)
            .with_context(|| format!("failed to stat remote path {}", literal_path.display()))?;
        sources.push(literal_path);
    }
    Ok(sources)
}

fn append_manifest_entry(
    root_index: u32,
    path: &Path,
    relative_path: &Path,
    metadata: &std::fs::Metadata,
    entries: &mut Vec<TransferEntry>,
    files: &mut Vec<BatchFileSource>,
    total_bytes: &mut u64,
) -> Result<()> {
    if metadata.file_type().is_symlink() {
        bail!(
            "symbolic links are not supported in batch pull: {}",
            path.display()
        );
    }

    if metadata.is_dir() {
        entries.push(TransferEntry {
            root_index,
            relative_path: path_to_transfer_string(relative_path)?,
            kind: TransferEntryKind::Directory,
            mode: file_mode(metadata),
            size: 0,
        });

        let mut children = std::fs::read_dir(path)
            .with_context(|| format!("failed to read remote directory {}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("failed to read remote directory {}", path.display()))?;
        children.sort();

        for child in children {
            let child_name = child
                .file_name()
                .ok_or_else(|| anyhow!("path has no file name: {}", child.display()))?;
            let child_relative = if relative_path.as_os_str().is_empty() {
                PathBuf::from(child_name)
            } else {
                relative_path.join(child_name)
            };
            let child_metadata = std::fs::symlink_metadata(&child)
                .with_context(|| format!("failed to stat remote path {}", child.display()))?;
            append_manifest_entry(
                root_index,
                &child,
                &child_relative,
                &child_metadata,
                entries,
                files,
                total_bytes,
            )?;
        }
        return Ok(());
    }

    if !metadata.is_file() {
        bail!("unsupported remote path type: {}", path.display());
    }

    let size = metadata.len();
    entries.push(TransferEntry {
        root_index,
        relative_path: path_to_transfer_string(relative_path)?,
        kind: TransferEntryKind::File,
        mode: file_mode(metadata),
        size,
    });
    files.push(BatchFileSource {
        path: path.to_path_buf(),
        size,
    });
    *total_bytes += size;
    Ok(())
}

async fn stream_batch_files(
    stream: &mut TcpStream,
    request_id: u32,
    files: &[BatchFileSource],
) -> Result<u64> {
    let mut writer =
        tokio::io::BufWriter::with_capacity(HEADER_LEN + DEFAULT_STREAM_CHUNK_SIZE, &mut *stream);
    let mut buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
    let mut total_bytes = 0_u64;

    for file_entry in files {
        let mut file = File::open(&file_entry.path).await.with_context(|| {
            format!(
                "failed to open remote path for read: {}",
                file_entry.path.display()
            )
        })?;
        let mut file_bytes = 0_u64;
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }

            write_stream_frame(
                &mut writer,
                request_id,
                StreamChannel::File,
                false,
                &buffer[..read],
            )
            .await?;
            total_bytes += read as u64;
            file_bytes += read as u64;
        }

        if file_bytes != file_entry.size {
            bail!(
                "remote file changed during pull: expected {} bytes, read {} from {}",
                file_entry.size,
                file_bytes,
                file_entry.path.display()
            );
        }
    }

    writer.flush().await?;
    Ok(total_bytes)
}

async fn read_file_stream_chunk_into(
    stream: &mut TcpStream,
    request_id: u32,
    context_name: &str,
    payload: &mut Vec<u8>,
) -> Result<rsdb_proto::StreamFrameHeader> {
    let chunk = read_stream_frame_into(stream, payload)
        .await
        .with_context(|| format!("failed to read {context_name} frame"))?;
    if chunk.request_id != request_id {
        bail!(
            "stream frame request id mismatch: expected {}, got {}",
            request_id,
            chunk.request_id
        );
    }
    if chunk.channel != StreamChannel::File {
        bail!(
            "unexpected stream channel: expected {:?}, got {:?}",
            StreamChannel::File,
            chunk.channel
        );
    }
    Ok(chunk)
}

async fn expect_file_stream_eof(
    stream: &mut TcpStream,
    request_id: u32,
    context_name: &str,
    payload: &mut Vec<u8>,
) -> Result<()> {
    let chunk = read_file_stream_chunk_into(stream, request_id, context_name, payload).await?;
    if !chunk.eof || !payload.is_empty() {
        bail!("expected end of {context_name} file stream");
    }
    Ok(())
}

async fn resolve_batch_destination_roots(
    destination: &str,
    roots: &[TransferRoot],
) -> Result<Vec<PathBuf>> {
    if roots.is_empty() {
        bail!("transfer manifest did not include any roots");
    }
    ensure_unique_root_names(roots)?;

    let destination_path = PathBuf::from(destination);
    if destination_path.as_os_str().is_empty() {
        bail!("remote path must not be empty");
    }

    let destination_metadata = match metadata(&destination_path).await {
        Ok(metadata) => Some(metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to stat remote destination {}",
                    destination_path.display()
                )
            });
        }
    };

    if roots.len() == 1 {
        let treat_as_directory = destination_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.is_dir())
            || destination.ends_with('/');
        let root = &roots[0];
        let base = if treat_as_directory {
            destination_path.join(&root.source_name)
        } else {
            destination_path
        };
        return Ok(vec![base]);
    }

    if destination_metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_dir())
    {
        bail!("remote destination is not a directory: {destination}");
    }

    Ok(roots
        .iter()
        .map(|root| destination_path.join(&root.source_name))
        .collect())
}

fn resolve_transfer_entry_path(root_paths: &[PathBuf], entry: &TransferEntry) -> Result<PathBuf> {
    let root_path = root_paths
        .get(entry.root_index as usize)
        .ok_or_else(|| anyhow!("invalid transfer root index {}", entry.root_index))?;
    join_transfer_relative_path(root_path, &entry.relative_path)
}

fn join_transfer_relative_path(base: &Path, relative_path: &str) -> Result<PathBuf> {
    if relative_path.is_empty() {
        return Ok(base.to_path_buf());
    }

    let mut resolved = base.to_path_buf();
    for component in Path::new(relative_path).components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            _ => bail!("invalid transfer path: {relative_path}"),
        }
    }
    Ok(resolved)
}

fn ensure_unique_root_names(roots: &[TransferRoot]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        if !seen.insert(root.source_name.clone()) {
            bail!(
                "duplicate transfer root name `{}` is not supported",
                root.source_name
            );
        }
    }
    Ok(())
}

fn transfer_source_name(path: &Path) -> Result<String> {
    if let Some(name) = path.file_name().filter(|name| !name.is_empty()) {
        return Ok(name.to_string_lossy().into_owned());
    }

    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    canonical
        .file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("path has no usable file name: {}", path.display()))
}

fn path_to_transfer_string(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() {
        return Ok(String::new());
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            _ => bail!("invalid transfer path: {}", path.display()),
        }
    }
    Ok(parts.join("/"))
}

fn has_glob_pattern(value: &str) -> bool {
    value.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

fn glob_match_options() -> MatchOptions {
    MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    }
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
    let (mut reader, writer) = stream.into_split();
    let mut writer = tokio::io::BufWriter::new(writer);
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
    writer.flush().await?;

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
                        let chunk = decode_stream_frame(frame)?;
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
                        writer.flush().await?;
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
                writer.flush().await?;
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
                writer.flush().await?;
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
    writer.flush().await?;
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
    let (mut reader, writer) = stream.into_split();
    let mut writer = tokio::io::BufWriter::new(writer);
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
    writer.flush().await?;

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
                        let chunk = decode_stream_frame(frame)?;
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
                        writer.flush().await?;
                    }
                    Some(PtyOutput::Eof) | None => {
                        stdout_open = false;
                        write_stream_frame(&mut writer, request_id, StreamChannel::Stdout, true, &[]).await?;
                        writer.flush().await?;
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
    writer.flush().await?;
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

fn classify_fs_error(err: &anyhow::Error) -> ErrorCode {
    if is_invalid_request_error(err) {
        ErrorCode::InvalidRequest
    } else if is_precondition_error(err) {
        ErrorCode::FsPreconditionFailed
    } else if has_io_error_kind(err, std::io::ErrorKind::NotFound) {
        ErrorCode::FsNotFound
    } else if has_io_error_kind(err, std::io::ErrorKind::PermissionDenied) {
        ErrorCode::FsPermissionDenied
    } else if has_storage_full_error(err) {
        ErrorCode::FsNoSpace
    } else {
        ErrorCode::Internal
    }
}

fn has_io_error_kind(err: &anyhow::Error, expected: std::io::ErrorKind) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == expected)
    })
}

fn has_storage_full_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| {
                io_err.kind() == std::io::ErrorKind::StorageFull
                    || io_err.raw_os_error() == Some(28)
            })
    })
}

fn is_precondition_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<FsPreconditionError>().is_some())
}

fn is_invalid_request_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<FsInvalidRequestError>().is_some())
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
