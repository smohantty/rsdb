use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::IsTerminal as _;
use std::io::Read as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size as terminal_size};
use rsdb_proto::{
    CapabilitySet, ControlRequest, ControlResponse, DEFAULT_STREAM_CHUNK_SIZE, DiscoveryRequest,
    DiscoveryResponse, FrameKind, MAX_DISCOVERY_PAYLOAD_LEN, PROTOCOL_VERSION, StreamChannel,
    decode_discovery_message, decode_json, decode_stream_frame, encode_discovery_message,
    read_frame, write_json_frame, write_stream_frame,
};
use serde::{Deserialize, Serialize};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, trace};

const REQUEST_ID: u32 = 1;

struct RawModeGuard(bool);

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = disable_raw_mode();
        }
    }
}

enum StdinChunk {
    Data(Vec<u8>),
    Eof,
    Error(String),
}

#[derive(Debug, Parser)]
#[command(name = "rsdb")]
#[command(about = "Rust host client for RSDB")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Connect {
        addr: String,
        #[arg(long)]
        name: Option<String>,
    },
    Disconnect {
        name: Option<String>,
    },
    Devices,
    Discover {
        #[arg(long, default_value = "255.255.255.255")]
        probe_addr: String,
        #[arg(long, default_value_t = 27101)]
        port: u16,
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
    },
    Ping {
        #[arg(long)]
        target: Option<String>,
    },
    Capability {
        #[arg(long)]
        target: Option<String>,
    },
    Shell {
        #[arg(long)]
        target: Option<String>,
        #[arg(allow_hyphen_values = true)]
        command: Vec<String>,
    },
    Push {
        #[arg(long)]
        target: Option<String>,
        local_path: PathBuf,
        remote_path: String,
    },
    Pull {
        #[arg(long)]
        target: Option<String>,
        remote_path: String,
        local_path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredTarget {
    name: String,
    addr: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Registry {
    targets: Vec<StoredTarget>,
    current_target: Option<String>,
}

#[derive(Debug, Clone)]
struct DiscoveredTarget {
    device_name: String,
    addr: SocketAddr,
    platform: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Commands::Connect { addr, name } => connect_command(&addr, name.as_deref()).await,
        Commands::Disconnect { name } => disconnect_command(name.as_deref()),
        Commands::Devices => devices_command().await,
        Commands::Discover {
            probe_addr,
            port,
            timeout_ms,
        } => discover_command(&probe_addr, port, timeout_ms).await,
        Commands::Ping { target } => ping_command(target.as_deref()).await,
        Commands::Capability { target } => capability_command(target.as_deref()).await,
        Commands::Shell { target, command } => shell_command(target.as_deref(), &command).await,
        Commands::Push {
            target,
            local_path,
            remote_path,
        } => push_command(target.as_deref(), &local_path, &remote_path).await,
        Commands::Pull {
            target,
            remote_path,
            local_path,
        } => pull_command(target.as_deref(), &remote_path, &local_path).await,
    }
}

fn init_tracing() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "off".into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

async fn connect_command(addr: &str, requested_name: Option<&str>) -> Result<()> {
    let response = request(addr, ControlRequest::Ping).await?;
    let (server_id, protocol_version) = match response {
        ControlResponse::Pong {
            server_id,
            protocol_version,
        } => (server_id, protocol_version),
        other => bail!("unexpected response from daemon: {other:?}"),
    };

    if protocol_version != PROTOCOL_VERSION {
        bail!(
            "protocol version mismatch: host={} daemon={protocol_version}",
            PROTOCOL_VERSION
        );
    }

    let name = requested_name
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_name(addr));
    let registry = Registry {
        targets: vec![StoredTarget {
            name: name.clone(),
            addr: addr.to_string(),
        }],
        current_target: Some(name.clone()),
    };
    save_registry(&registry)?;

    println!("connected {name} -> {addr} ({server_id})");
    Ok(())
}

fn disconnect_command(name: Option<&str>) -> Result<()> {
    let mut registry = load_registry()?;
    let Some(target) = primary_target(&registry) else {
        bail!("no saved targets; use `rsdb connect <addr>` first");
    };

    if let Some(name) = name
        && target.name != name
    {
        bail!("no saved target named {name}");
    }

    let disconnected = target.name.to_string();
    registry.targets.clear();
    registry.current_target = None;
    save_registry(&registry)?;
    println!("disconnected {disconnected}");
    Ok(())
}

async fn devices_command() -> Result<()> {
    let registry = load_registry()?;
    let Some(target) = primary_target(&registry).cloned() else {
        println!("no saved devices");
        return Ok(());
    };

    let status = match request(&target.addr, ControlRequest::Ping).await {
        Ok(ControlResponse::Pong { server_id, .. }) => format!("online ({server_id})"),
        Ok(other) => format!("unexpected ({other:?})"),
        Err(err) => format!("offline ({err})"),
    };
    println!("{}\t{}\t{}", target.name, target.addr, status);
    Ok(())
}

async fn discover_command(probe_addr: &str, port: u16, timeout_ms: u64) -> Result<()> {
    let targets = discover_targets(probe_addr, port, timeout_ms).await?;
    if targets.is_empty() {
        println!("no devices discovered");
        return Ok(());
    }

    let name_width = targets
        .iter()
        .map(|t| t.device_name.len())
        .max()
        .unwrap_or(0)
        .max(4);
    let addr_width = targets
        .iter()
        .map(|t| t.addr.to_string().len())
        .max()
        .unwrap_or(0)
        .max(7);
    println!(
        "{:<name_width$}   {:<addr_width$}   PLATFORM",
        "NAME", "ADDRESS",
    );
    for target in &targets {
        println!(
            "{:<name_width$}   {:<addr_width$}   {}",
            target.device_name,
            target.addr,
            target.platform,
        );
    }
    Ok(())
}

async fn discover_targets(
    probe_addr: &str,
    port: u16,
    timeout_ms: u64,
) -> Result<Vec<DiscoveredTarget>> {
    let mut discovered = BTreeMap::<String, DiscoveredTarget>::new();
    for probe_ip in discovery_probe_addresses(probe_addr)? {
        for target in discover_targets_once(probe_ip, port, timeout_ms).await? {
            let key = format!("{}@{}", target.device_name, target.addr);
            discovered.insert(key, target);
        }
    }

    Ok(discovered.into_values().collect())
}

fn discovery_probe_addresses(probe_addr: &str) -> Result<Vec<IpAddr>> {
    let probe_ip: IpAddr = probe_addr
        .parse()
        .with_context(|| format!("invalid probe address: {probe_addr}"))?;
    let mut addresses = vec![probe_ip];
    if probe_ip == IpAddr::V4(std::net::Ipv4Addr::BROADCAST) {
        addresses.push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    Ok(addresses)
}

async fn discover_targets_once(
    probe_ip: IpAddr,
    port: u16,
    timeout_ms: u64,
) -> Result<Vec<DiscoveredTarget>> {
    let bind_addr = match probe_ip {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind discovery socket on {bind_addr}"))?;
    if probe_ip.is_ipv4() {
        socket
            .set_broadcast(true)
            .context("failed to enable UDP broadcast")?;
    }

    let nonce = discovery_nonce();
    let request = DiscoveryRequest::Probe { nonce };
    let payload = encode_discovery_message(&request).context("failed to encode discovery probe")?;
    let destination = SocketAddr::new(probe_ip, port);
    socket
        .send_to(&payload, destination)
        .await
        .with_context(|| format!("failed to send discovery probe to {destination}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut buffer = vec![0_u8; MAX_DISCOVERY_PAYLOAD_LEN + rsdb_proto::DISCOVERY_MAGIC.len()];
    let mut discovered = BTreeMap::<String, DiscoveredTarget>::new();

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }

        let remaining = deadline - now;
        let received = timeout(remaining, socket.recv_from(&mut buffer)).await;
        let (len, peer) = match received {
            Ok(Ok(packet)) => packet,
            Ok(Err(err)) => return Err(err).context("failed to receive discovery response"),
            Err(_) => break,
        };

        let response: DiscoveryResponse = match decode_discovery_message(&buffer[..len]) {
            Ok(response) => response,
            Err(_) => continue,
        };
        if response.nonce != nonce {
            continue;
        }

        let addr = SocketAddr::new(peer.ip(), response.tcp_port);
        let key = format!("{}@{}", response.server_id, addr);
        discovered.insert(
            key,
            DiscoveredTarget {
                device_name: response.device_name,
                addr,
                platform: response.platform,
            },
        );
    }

    Ok(discovered.into_values().collect())
}

fn discovery_nonce() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

async fn ping_command(target: Option<&str>) -> Result<()> {
    let addr = resolve_target(target)?;
    match request(&addr, ControlRequest::Ping).await? {
        ControlResponse::Pong {
            server_id,
            protocol_version,
        } => {
            println!("pong\t{addr}\t{server_id}\tprotocol={protocol_version}");
            Ok(())
        }
        other => bail!("unexpected response: {other:?}"),
    }
}

async fn capability_command(target: Option<&str>) -> Result<()> {
    let addr = resolve_target(target)?;
    match request(&addr, ControlRequest::GetCapabilities).await? {
        ControlResponse::Capabilities {
            server_id,
            capability,
        } => print_capability(&addr, &server_id, &capability),
        other => bail!("unexpected response: {other:?}"),
    }
}

async fn shell_command(target: Option<&str>, command: &[String]) -> Result<()> {
    let addr = resolve_target(target)?;
    let (program, args, interactive) = match command.split_first() {
        Some((program, args)) => (Some(program.clone()), args.to_vec(), false),
        None => (None, Vec::new(), true),
    };
    run_shell_session(&addr, program, args, interactive).await
}

async fn push_command(target: Option<&str>, local_path: &Path, remote_path: &str) -> Result<()> {
    let addr = resolve_target(target)?;
    let metadata = tokio::fs::metadata(local_path)
        .await
        .with_context(|| format!("failed to stat local file {}", local_path.display()))?;
    if !metadata.is_file() {
        bail!("local path is not a regular file: {}", local_path.display());
    }

    let mode = local_file_mode(&metadata);
    let source_name = local_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("local path has no file name: {}", local_path.display()))?;
    let mut file = File::open(local_path)
        .await
        .with_context(|| format!("failed to open local file {}", local_path.display()))?;
    let mut stream = open_connection(&addr).await?;

    write_json_frame(
        &mut stream,
        FrameKind::Request,
        REQUEST_ID,
        &ControlRequest::Push {
            path: remote_path.to_string(),
            mode,
            source_name: Some(source_name),
        },
    )
    .await
    .context("failed to send push request")?;

    match read_response(&mut stream).await? {
        ControlResponse::PushReady => {}
        ControlResponse::Error { code, message } => {
            bail!("remote error {code:?}: {message}");
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }

    let mut buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        write_stream_frame(
            &mut stream,
            REQUEST_ID,
            StreamChannel::File,
            false,
            &buffer[..read],
        )
        .await
        .context("failed to stream file data")?;
    }

    write_stream_frame(&mut stream, REQUEST_ID, StreamChannel::File, true, &[])
        .await
        .context("failed to finish push stream")?;

    match read_response(&mut stream).await? {
        ControlResponse::PushComplete { bytes_written } => {
            println!(
                "pushed {}\t{}\t{} bytes",
                local_path.display(),
                remote_path,
                bytes_written
            );
            Ok(())
        }
        ControlResponse::Error { code, message } => {
            bail!("remote error {code:?}: {message}");
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn pull_command(target: Option<&str>, remote_path: &str, local_path: &Path) -> Result<()> {
    let addr = resolve_target(target)?;
    let mut stream = open_connection(&addr).await?;
    write_json_frame(
        &mut stream,
        FrameKind::Request,
        REQUEST_ID,
        &ControlRequest::Pull {
            path: remote_path.to_string(),
        },
    )
    .await
    .context("failed to send pull request")?;

    let (size, mode) = match read_response(&mut stream).await? {
        ControlResponse::PullMetadata { size, mode } => (size, mode),
        ControlResponse::Error { code, message } => {
            bail!("remote error {code:?}: {message}");
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    };

    let mut file = create_local_output_file(local_path, mode).await?;
    let mut bytes_received = 0_u64;

    loop {
        let frame = read_frame(&mut stream)
            .await
            .context("failed to read pull frame")?;
        match frame.header.kind {
            FrameKind::Stream => {
                let chunk = decode_stream_frame(&frame).context("invalid stream frame")?;
                ensure_request_id(chunk.request_id, REQUEST_ID)?;
                if chunk.channel != StreamChannel::File {
                    bail!("unexpected stream channel during pull: {:?}", chunk.channel);
                }
                if !chunk.payload.is_empty() {
                    file.write_all(&chunk.payload)
                        .await
                        .with_context(|| format!("failed to write {}", local_path.display()))?;
                    bytes_received += chunk.payload.len() as u64;
                }
            }
            FrameKind::Response => {
                let response: ControlResponse =
                    decode_json(&frame, FrameKind::Response).context("invalid response frame")?;
                match response {
                    ControlResponse::PullComplete { bytes_sent } => {
                        file.flush().await?;
                        #[cfg(unix)]
                        if mode != 0 {
                            tokio::fs::set_permissions(
                                local_path,
                                std::fs::Permissions::from_mode(mode & 0o7777),
                            )
                            .await
                            .with_context(|| {
                                format!("failed to set permissions on {}", local_path.display())
                            })?;
                        }
                        if bytes_sent != bytes_received {
                            bail!(
                                "pull size mismatch: daemon sent {bytes_sent} bytes, received {bytes_received}"
                            );
                        }
                        println!(
                            "pulled {}\t{}\t{} / {} bytes",
                            remote_path,
                            local_path.display(),
                            bytes_received,
                            size
                        );
                        return Ok(());
                    }
                    ControlResponse::Error { code, message } => {
                        bail!("remote error {code:?}: {message}");
                    }
                    other => bail!("unexpected response from daemon: {other:?}"),
                }
            }
            other => bail!("unexpected frame kind during pull: {other:?}"),
        }
    }
}

async fn run_shell_session(
    addr: &str,
    command: Option<String>,
    args: Vec<String>,
    interactive: bool,
) -> Result<()> {
    let mut stream = open_connection(addr).await?;
    let term = interactive.then(remote_term);
    let (rows, cols) = interactive
        .then(remote_terminal_size)
        .transpose()?
        .unwrap_or((None, None));
    write_json_frame(
        &mut stream,
        FrameKind::Request,
        REQUEST_ID,
        &ControlRequest::Shell {
            command,
            args,
            pty: interactive,
            term,
            rows,
            cols,
        },
    )
    .await
    .context("failed to send shell request")?;

    match read_response(&mut stream).await? {
        ControlResponse::ShellStarted { .. } => {}
        ControlResponse::Error { code, message } => {
            bail!("remote error {code:?}: {message}");
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }
    let (mut reader, mut writer) = stream.into_split();
    let (frame_tx, mut frame_rx) = mpsc::channel(32);
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

    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut stdin_open = interactive;
    let mut stdin_rx = interactive.then(start_stdin_pump);
    let _raw_mode = interactive.then(enable_local_raw_mode).transpose()?;

    if !interactive {
        write_stream_frame(&mut writer, REQUEST_ID, StreamChannel::Stdin, true, &[])
            .await
            .context("failed to close remote stdin")?;
    }

    loop {
        tokio::select! {
            chunk = recv_stdin_chunk(&mut stdin_rx), if stdin_open => {
                match chunk {
                    Some(StdinChunk::Data(data)) => {
                        trace!(bytes = data.len(), eof = false, "forwarding shell stdin chunk");
                        write_stream_frame(
                            &mut writer,
                            REQUEST_ID,
                            StreamChannel::Stdin,
                            false,
                            &data,
                        )
                        .await
                        .context("failed to forward stdin")?;
                    }
                    Some(StdinChunk::Eof) | None => {
                        trace!("forwarding shell stdin eof");
                        write_stream_frame(&mut writer, REQUEST_ID, StreamChannel::Stdin, true, &[])
                            .await
                            .context("failed to finish stdin stream")?;
                        stdin_open = false;
                        stdin_rx = None;
                    }
                    Some(StdinChunk::Error(message)) => {
                        return Err(anyhow!("failed to read local stdin: {message}"));
                    }
                }
            }
            frame = frame_rx.recv() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(err)) => return Err(anyhow!("failed to read shell frame: {err}")),
                    None => return Err(anyhow!("shell connection closed unexpectedly")),
                };
                trace!(kind = ?frame.header.kind, payload = frame.payload.len(), "received shell frame");
                match frame.header.kind {
                    FrameKind::Stream => {
                        let chunk = decode_stream_frame(&frame).context("invalid stream frame")?;
                        ensure_request_id(chunk.request_id, REQUEST_ID)?;
                        match chunk.channel {
                            StreamChannel::Stdout => {
                                if !chunk.payload.is_empty() {
                                    stdout.write_all(&chunk.payload).await?;
                                    stdout.flush().await?;
                                }
                            }
                            StreamChannel::Stderr => {
                                if !chunk.payload.is_empty() {
                                    stderr.write_all(&chunk.payload).await?;
                                    stderr.flush().await?;
                                }
                            }
                            other => bail!("unexpected stream channel during shell: {other:?}"),
                        }
                    }
                    FrameKind::Response => {
                        let response: ControlResponse = decode_json(&frame, FrameKind::Response)
                            .context("invalid response frame")?;
                        match response {
                            ControlResponse::ShellExit { status } => {
                                stdout.flush().await?;
                                stderr.flush().await?;
                                if !interactive && status != 0 {
                                    bail!("remote shell exited with status {status}");
                                }
                                return Ok(());
                            }
                            ControlResponse::Error { code, message } => {
                                bail!("remote error {code:?}: {message}");
                            }
                            other => bail!("unexpected response from daemon: {other:?}"),
                        }
                    }
                    other => bail!("unexpected frame kind during shell: {other:?}"),
                }
            }
        }
    }
}

fn start_stdin_pump() -> mpsc::Receiver<StdinChunk> {
    let (tx, rx) = mpsc::channel(8);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut locked = stdin.lock();
        let mut buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
        loop {
            match locked.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.blocking_send(StdinChunk::Eof);
                    break;
                }
                Ok(read) => {
                    trace!(bytes = read, "read local shell stdin");
                    if tx
                        .blocking_send(StdinChunk::Data(buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.blocking_send(StdinChunk::Error(err.to_string()));
                    break;
                }
            }
        }
    });
    rx
}

async fn recv_stdin_chunk(stdin_rx: &mut Option<mpsc::Receiver<StdinChunk>>) -> Option<StdinChunk> {
    match stdin_rx {
        Some(rx) => rx.recv().await,
        None => None,
    }
}

fn enable_local_raw_mode() -> Result<RawModeGuard> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        Ok(RawModeGuard(true))
    } else {
        Ok(RawModeGuard(false))
    }
}

fn remote_term() -> String {
    env::var("TERM")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "xterm-256color".to_string())
}

fn remote_terminal_size() -> Result<(Option<u16>, Option<u16>)> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let (cols, rows) = terminal_size().context("failed to read local terminal size")?;
        Ok((Some(rows), Some(cols)))
    } else {
        Ok((None, None))
    }
}

fn print_capability(addr: &str, server_id: &str, capability: &CapabilitySet) -> Result<()> {
    let pretty = serde_json::to_string_pretty(&serde_json::json!({
        "addr": addr,
        "server_id": server_id,
        "protocol_version": capability.protocol_version,
        "transports": capability.transports,
        "security": capability.security,
        "features": capability.features,
    }))?;
    println!("{pretty}");
    Ok(())
}

async fn request(addr: &str, request: ControlRequest) -> Result<ControlResponse> {
    let mut stream = open_connection(addr).await?;
    write_json_frame(&mut stream, FrameKind::Request, REQUEST_ID, &request)
        .await
        .context("failed to send request")?;
    read_response(&mut stream).await
}

async fn open_connection(addr: &str) -> Result<TcpStream> {
    debug!(target_addr = %addr, "opening tcp connection");
    timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .context("connection timed out")?
        .with_context(|| format!("failed to connect to {addr}"))
}

async fn read_response(stream: &mut TcpStream) -> Result<ControlResponse> {
    let frame = read_frame(stream)
        .await
        .context("failed to read response")?;
    let response: ControlResponse =
        decode_json(&frame, FrameKind::Response).context("failed to decode response")?;
    Ok(response)
}

async fn create_local_output_file(path: &Path, mode: u32) -> Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create local directory {}", parent.display())
            })?;
        }
    }

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(normalize_mode(mode));

    options
        .open(path)
        .await
        .with_context(|| format!("failed to open local output {}", path.display()))
}

fn ensure_request_id(actual: u32, expected: u32) -> Result<()> {
    if actual != expected {
        bail!("request id mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn resolve_target(input: Option<&str>) -> Result<String> {
    if let Some(value) = input {
        if value.contains(':') {
            return Ok(value.to_string());
        }

        let registry = load_registry()?;
        let target = primary_target(&registry).ok_or_else(|| anyhow!("no saved targets"))?;
        if target.name != value {
            bail!("no saved target named {value}");
        }
        return Ok(target.addr.clone());
    }

    let registry = load_registry()?;
    primary_target(&registry)
        .map(|target| target.addr.clone())
        .ok_or_else(|| anyhow!("no saved targets; use `rsdb connect <addr>` first"))
}

fn primary_target(registry: &Registry) -> Option<&StoredTarget> {
    if let Some(current_name) = registry.current_target.as_deref()
        && let Some(target) = registry
            .targets
            .iter()
            .find(|entry| entry.name == current_name)
    {
        return Some(target);
    }

    registry.targets.last()
}

fn load_registry() -> Result<Registry> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(Registry::default());
    }

    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read registry file {}", path.display()))?;
    let registry: Registry =
        serde_json::from_str(&contents).with_context(|| format!("invalid {}", path.display()))?;
    let normalized = normalize_registry(registry.clone());
    if normalized != registry {
        write_registry(&path, &normalized)?;
    }
    Ok(normalized)
}

fn save_registry(registry: &Registry) -> Result<()> {
    let path = registry_path()?;
    let registry = normalize_registry(registry.clone());
    write_registry(&path, &registry)
}

fn write_registry(path: &Path, registry: &Registry) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("registry path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    let contents = serde_json::to_string_pretty(&registry)?;
    fs::write(&path, contents)
        .with_context(|| format!("failed to write registry file {}", path.display()))?;
    Ok(())
}

fn normalize_registry(mut registry: Registry) -> Registry {
    let target = primary_target(&registry).cloned();
    registry.targets = target.into_iter().collect();
    registry.current_target = registry.targets.first().map(|entry| entry.name.clone());
    registry
}

fn registry_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("targets.json"))
}

fn config_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(Path::new(&path).join("rsdb"));
    }

    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(Path::new(&home).join(".config").join("rsdb"))
}

fn default_name(addr: &str) -> String {
    addr.chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch,
            _ => '-',
        })
        .collect()
}

#[cfg(unix)]
fn local_file_mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn local_file_mode(_metadata: &std::fs::Metadata) -> u32 {
    0
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
