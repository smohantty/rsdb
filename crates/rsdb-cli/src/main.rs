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
const DEFAULT_RSDB_PORT: u16 = 27101;

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
        #[arg(value_name = "ADDR")]
        target: Option<String>,
    },
    Devices,
    Discover {
        #[arg(long, default_value = "255.255.255.255")]
        probe_addr: String,
        #[arg(long, default_value_t = DEFAULT_RSDB_PORT)]
        port: u16,
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
    },
    Ping {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
    },
    Capability {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
    },
    Shell {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
        #[arg(allow_hyphen_values = true)]
        command: Vec<String>,
    },
    Push {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
        local_path: PathBuf,
        remote_path: String,
    },
    Pull {
        #[arg(long, value_name = "ADDR")]
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
    protocol_version: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Commands::Connect { addr, name } => connect_command(&addr, name.as_deref()).await,
        Commands::Disconnect { target } => disconnect_command(target.as_deref()),
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
    let addr = normalize_connect_addr(addr);
    let discovered = discover_target_at(&addr, 500).await.ok().flatten();
    if let Some(warning) = discovered
        .as_ref()
        .and_then(|target| protocol_warning_text(target.protocol_version))
    {
        eprintln!(
            "warning: {warning}; commands may fail until rsdb and rsdbd are updated together"
        );
    }
    let response = request(&addr, ControlRequest::Ping).await?;
    let protocol_version = match response {
        ControlResponse::Pong {
            protocol_version, ..
        } => protocol_version,
        other => bail!("unexpected response from daemon: {other:?}"),
    };

    if protocol_version != PROTOCOL_VERSION {
        eprintln!(
            "warning: {}; commands may fail until rsdb and rsdbd are updated together",
            protocol_warning_label(protocol_version)
        );
    }

    let mut registry = load_registry()?;
    let existing_target = registry.targets.iter().find(|target| target.addr == addr);
    let name = if let Some(name) = requested_name {
        name.to_owned()
    } else if let Some(target) =
        existing_target.filter(|target| !is_legacy_auto_name(&target.name, &addr))
    {
        target.name.clone()
    } else if let Some(target) = discovered
        .as_ref()
        .filter(|target| !target.device_name.trim().is_empty())
    {
        target.device_name.clone()
    } else {
        default_name(&addr)
    };
    registry.targets.retain(|target| target.addr != addr);
    registry.targets.push(StoredTarget {
        name: name.clone(),
        addr: addr.clone(),
    });
    registry.current_target = Some(addr.clone());
    save_registry(&registry)?;

    println!("connected {name} -> {}", display_addr(&addr));
    Ok(())
}

fn disconnect_command(name: Option<&str>) -> Result<()> {
    let mut registry = load_registry()?;
    let index = if let Some(selector) = name {
        find_target_index_by_addr(&registry, selector)?
    } else {
        primary_target_index(&registry)
    }
    .ok_or_else(|| anyhow!("no saved targets; use `rsdb connect <addr>` first"))?;
    let target = registry.targets.remove(index);
    let disconnected = target.name.to_string();
    if registry.targets.is_empty() {
        registry.current_target = None;
    } else if registry.current_target.as_deref() == Some(&target.addr)
        || registry.current_target.as_deref() == Some(&target.name)
    {
        registry.current_target = registry.targets.last().map(|target| target.addr.clone());
    }
    save_registry(&registry)?;
    println!("disconnected {disconnected}");
    Ok(())
}

async fn devices_command() -> Result<()> {
    let mut registry = load_registry()?;
    if registry.targets.is_empty() {
        println!("no saved devices");
        return Ok(());
    }
    let current_addr = primary_target(&registry).map(|target| target.addr.clone());
    let mut rows = Vec::with_capacity(registry.targets.len());
    let mut changed = false;

    for target in &mut registry.targets {
        let mut display_name = target.name.clone();
        let mut platform = "-".to_string();
        let mut warning = String::new();
        let status =
            if let Some(discovered) = discover_target_at(&target.addr, 300).await.ok().flatten() {
                display_name = discovered.device_name.clone();
                platform = discovered.platform;
                warning = protocol_warning_text(discovered.protocol_version).unwrap_or_default();
                if is_legacy_auto_name(&target.name, &target.addr)
                    && discovered.device_name != target.name
                {
                    target.name = discovered.device_name.clone();
                    changed = true;
                }
                "online".to_string()
            } else {
                match request(&target.addr, ControlRequest::Ping).await {
                    Ok(ControlResponse::Pong { .. }) => "online".to_string(),
                    Ok(other) => format!("unexpected ({other:?})"),
                    Err(err) => format!("offline ({err})"),
                }
            };
        rows.push((
            current_addr.as_deref() == Some(&target.addr),
            display_name,
            display_addr(&target.addr),
            platform,
            status,
            warning,
        ));
    }

    if changed {
        save_registry(&registry)?;
    }

    let name_width = rows
        .iter()
        .map(|(_, name, _, _, _, _)| name.len())
        .max()
        .unwrap_or(0)
        .max(4);
    let addr_width = rows
        .iter()
        .map(|(_, _, addr, _, _, _)| addr.len())
        .max()
        .unwrap_or(0)
        .max(7);
    let platform_width = rows
        .iter()
        .map(|(_, _, _, platform, _, _)| platform.len())
        .max()
        .unwrap_or(0)
        .max(8);
    let warning_width = rows
        .iter()
        .map(|(_, _, _, _, _, warning)| warning.len())
        .max()
        .unwrap_or(0);
    if warning_width > 0 {
        println!(
            "{:<7}   {:<name_width$}   {:<addr_width$}   {:<platform_width$}   {:<6}   WARNING",
            "CURRENT", "NAME", "ADDRESS", "PLATFORM", "STATUS",
        );
    } else {
        println!(
            "{:<7}   {:<name_width$}   {:<addr_width$}   {:<platform_width$}   STATUS",
            "CURRENT", "NAME", "ADDRESS", "PLATFORM",
        );
    }
    for (is_current, display_name, addr, platform, status, warning) in rows {
        let current = if is_current { "*" } else { "" };
        if warning_width > 0 {
            println!(
                "{:<7}   {:<name_width$}   {:<addr_width$}   {:<platform_width$}   {:<6}   {}",
                current, display_name, addr, platform, status, warning,
            );
        } else {
            println!(
                "{:<7}   {:<name_width$}   {:<addr_width$}   {:<platform_width$}   {}",
                current, display_name, addr, platform, status,
            );
        }
    }
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
        .map(|t| display_socket_addr(t.addr).len())
        .max()
        .unwrap_or(0)
        .max(7);
    let warning_width = targets
        .iter()
        .filter_map(|target| protocol_warning_text(target.protocol_version))
        .map(|warning| warning.len())
        .max()
        .unwrap_or(0);
    if warning_width > 0 {
        println!(
            "{:<name_width$}   {:<addr_width$}   {:<44}   WARNING",
            "NAME", "ADDRESS", "PLATFORM",
        );
    } else {
        println!(
            "{:<name_width$}   {:<addr_width$}   PLATFORM",
            "NAME", "ADDRESS",
        );
    }
    for target in &targets {
        let warning = protocol_warning_text(target.protocol_version).unwrap_or_default();
        if warning_width > 0 {
            println!(
                "{:<name_width$}   {:<addr_width$}   {:<44}   {}",
                target.device_name,
                display_socket_addr(target.addr),
                target.platform,
                warning,
            );
        } else {
            println!(
                "{:<name_width$}   {:<addr_width$}   {}",
                target.device_name,
                display_socket_addr(target.addr),
                target.platform,
            );
        }
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

async fn discover_target_at(addr: &str, timeout_ms: u64) -> Result<Option<DiscoveredTarget>> {
    let mut destinations = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("failed to resolve discovery target {addr}"))?;
    let destination = destinations
        .next()
        .ok_or_else(|| anyhow!("no socket addresses resolved for {addr}"))?;
    let bind_addr = match destination {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind discovery socket on {bind_addr}"))?;

    let nonce = discovery_nonce();
    let request = DiscoveryRequest::Probe { nonce };
    let payload = encode_discovery_message(&request).context("failed to encode discovery probe")?;
    socket
        .send_to(&payload, destination)
        .await
        .with_context(|| format!("failed to send discovery probe to {destination}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut buffer = vec![0_u8; MAX_DISCOVERY_PAYLOAD_LEN + rsdb_proto::DISCOVERY_MAGIC.len()];

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }

        let remaining = deadline - now;
        let received = timeout(remaining, socket.recv_from(&mut buffer)).await;
        let (len, peer) = match received {
            Ok(Ok(packet)) => packet,
            Ok(Err(err)) => return Err(err).context("failed to receive discovery response"),
            Err(_) => return Ok(None),
        };

        let response: DiscoveryResponse = match decode_discovery_message(&buffer[..len]) {
            Ok(response) => response,
            Err(_) => continue,
        };
        if response.nonce != nonce {
            continue;
        }

        return Ok(Some(DiscoveredTarget {
            device_name: response.device_name,
            addr: SocketAddr::new(peer.ip(), response.tcp_port),
            platform: response.platform,
            protocol_version: response.protocol_version,
        }));
    }
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
                protocol_version: response.protocol_version,
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
            protocol_version, ..
        } => {
            println!("pong\t{}\tprotocol={protocol_version}", display_addr(&addr));
            if let Some(warning) = protocol_warning_text(protocol_version) {
                eprintln!(
                    "warning: {warning}; commands may fail until rsdb and rsdbd are updated together"
                );
            }
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
    let status = run_shell_session(&addr, program, args, interactive).await?;
    if status != 0 {
        std::process::exit(normalize_exit_status(status));
    }
    Ok(())
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
) -> Result<i32> {
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
                                    return Ok(status);
                                }
                                return Ok(status);
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

fn normalize_exit_status(status: i32) -> i32 {
    if (0..=255).contains(&status) {
        status
    } else {
        1
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
        return normalize_target_addr(value);
    }

    let registry = load_registry()?;
    primary_target(&registry)
        .map(|target| target.addr.clone())
        .ok_or_else(|| anyhow!("no saved targets; use `rsdb connect <addr>` first"))
}

fn primary_target(registry: &Registry) -> Option<&StoredTarget> {
    primary_target_index(registry).map(|index| &registry.targets[index])
}

fn primary_target_index(registry: &Registry) -> Option<usize> {
    if let Some(current_target) = registry.current_target.as_deref() {
        if let Some(index) = registry
            .targets
            .iter()
            .position(|entry| entry.addr == current_target)
        {
            return Some(index);
        }

        if let Some(index) = registry
            .targets
            .iter()
            .position(|entry| entry.name == current_target)
        {
            return Some(index);
        }
    }

    registry.targets.len().checked_sub(1)
}

fn find_target_index_by_addr(registry: &Registry, selector: &str) -> Result<Option<usize>> {
    let normalized = normalize_target_addr(selector)?;
    if let Some(index) = registry
        .targets
        .iter()
        .position(|target| target.addr == normalized)
    {
        return Ok(Some(index));
    }
    Ok(None)
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
    let current_addr = primary_target(&registry).map(|target| target.addr.clone());
    let mut deduped = Vec::with_capacity(registry.targets.len());
    for target in registry.targets.drain(..) {
        if let Some(index) = deduped
            .iter()
            .position(|entry: &StoredTarget| entry.addr == target.addr)
        {
            deduped.remove(index);
        }
        deduped.push(target);
    }
    registry.targets = deduped;
    registry.current_target = current_addr
        .filter(|current_addr| {
            registry
                .targets
                .iter()
                .any(|target| target.addr == *current_addr)
        })
        .or_else(|| registry.targets.last().map(|target| target.addr.clone()));
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

fn is_legacy_auto_name(name: &str, addr: &str) -> bool {
    name == default_name(addr)
}

fn display_addr(addr: &str) -> String {
    match addr.parse::<SocketAddr>() {
        Ok(addr) => display_socket_addr(addr),
        Err(_) => addr.to_string(),
    }
}

fn display_socket_addr(addr: SocketAddr) -> String {
    if addr.port() == DEFAULT_RSDB_PORT {
        addr.ip().to_string()
    } else {
        addr.to_string()
    }
}

fn protocol_warning_text(protocol_version: u16) -> Option<String> {
    (protocol_version != PROTOCOL_VERSION).then(|| protocol_warning_label(protocol_version))
}

fn protocol_warning_label(protocol_version: u16) -> String {
    format!(
        "protocol mismatch (host={} daemon={protocol_version})",
        PROTOCOL_VERSION
    )
}

fn normalize_connect_addr(value: &str) -> String {
    if let Ok(addr) = value.parse::<SocketAddr>() {
        return addr.to_string();
    }

    if let Ok(ip) = value.parse::<IpAddr>() {
        return SocketAddr::new(ip, DEFAULT_RSDB_PORT).to_string();
    }

    if has_port_suffix(value) {
        return value.to_string();
    }

    format!("{value}:{DEFAULT_RSDB_PORT}")
}

fn normalize_target_addr(value: &str) -> Result<String> {
    if let Ok(addr) = value.parse::<SocketAddr>() {
        return Ok(addr.to_string());
    }

    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_RSDB_PORT).to_string());
    }

    if has_port_suffix(value) {
        return Ok(value.to_string());
    }

    bail!("target must be an address like <ip> or <ip>:<port>");
}

fn has_port_suffix(value: &str) -> bool {
    value
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
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
