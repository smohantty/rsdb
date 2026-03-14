use base64::Engine as _;
use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::IsTerminal as _;
use std::io::Read as _;
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size as terminal_size};
use glob::{MatchOptions, glob_with};
use rsdb_proto::{
    AgentFsEntry, AgentFsReadResult, AgentFsStat, AgentFsWriteResult, CapabilitySet,
    ContentEncoding, ControlRequest, ControlResponse, DEFAULT_STREAM_CHUNK_SIZE, DiscoveryRequest,
    DiscoveryResponse, FrameKind, FsEntryKind, HEADER_LEN, HashAlgorithm,
    MAX_DISCOVERY_PAYLOAD_LEN, PROTOCOL_VERSION, StreamChannel, TransferEntry, TransferEntryKind,
    TransferRoot, decode_discovery_message, decode_json, decode_stream_frame,
    encode_discovery_message, read_frame, read_stream_frame_into, write_json_frame,
    write_stream_frame,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, trace};

const REQUEST_ID: u32 = 1;
const DEFAULT_RSDB_PORT: u16 = 27101;
const DEFAULT_INLINE_EDIT_MAX_BYTES: u64 = 4 * 1024 * 1024;

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
    /// Save a device and make it the current target.
    Connect {
        addr: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a saved device or the current target.
    Disconnect {
        #[arg(value_name = "ADDR")]
        target: Option<String>,
    },
    /// List saved devices and their current status.
    Devices,
    /// Discover reachable RSDB daemons on the network.
    Discover {
        #[arg(long, default_value = "255.255.255.255")]
        probe_addr: String,
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
    },
    /// Check whether a target daemon responds.
    Ping {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
    },
    /// Print daemon protocol features for a target.
    Capability {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
    },
    /// Open an interactive shell or run one remote command.
    Shell {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
        #[arg(allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Push local files or directories to the target.
    Push {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
        #[arg(value_name = "PATH", required = true, num_args = 2..)]
        paths: Vec<String>,
    },
    /// Pull remote files or directories from the target.
    Pull {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
        #[arg(value_name = "PATH", required = true, num_args = 2..)]
        paths: Vec<String>,
    },
    /// Edit one remote file in a local host editor.
    Edit {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
        /// Editor command. Falls back to RSDB_EDITOR, VISUAL, EDITOR, then `vi`.
        #[arg(long, value_name = "CMD")]
        editor: Option<String>,
        #[arg(value_name = "REMOTE_PATH")]
        path: String,
    },
    #[command(hide = true, name = "__complete-edit-path")]
    CompleteEditPath {
        #[arg(long, value_name = "ADDR")]
        target: Option<String>,
        #[arg(long, default_value = "")]
        partial: String,
    },
    /// Machine-facing agent command surface.
    Agent {
        #[arg(long, global = true, value_name = "ADDR")]
        target: Option<String>,
        #[command(subcommand)]
        command: AgentCommands,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommands {
    /// Print the machine-readable agent command schema as JSON.
    Schema,
    /// Discover reachable RSDB daemons for autonomous agents.
    Discover {
        #[arg(long, default_value = "255.255.255.255")]
        probe_addr: String,
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
    },
    /// Execute one direct remote process with JSON output.
    Exec {
        #[arg(long)]
        stream: bool,
        #[arg(long)]
        check: bool,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        timeout_secs: Option<u64>,
        #[arg(
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<String>,
    },
    /// Small structured remote filesystem operations.
    Fs {
        #[command(subcommand)]
        command: AgentFsCommands,
    },
    /// Bulk transfer files or directories with JSON output.
    Transfer {
        #[command(subcommand)]
        command: AgentTransferCommands,
    },
}

#[derive(Debug, Subcommand)]
enum AgentFsCommands {
    Stat {
        path: String,
        #[arg(long)]
        hash: Option<CliHashAlgorithm>,
    },
    List {
        path: String,
        #[arg(long)]
        recursive: bool,
        #[arg(long)]
        max_depth: Option<u32>,
        #[arg(long)]
        include_hidden: bool,
        #[arg(long)]
        hash: Option<CliHashAlgorithm>,
    },
    Read {
        path: String,
        #[arg(long, default_value = "utf8")]
        encoding: CliContentEncoding,
        #[arg(long)]
        max_bytes: Option<u64>,
    },
    Write {
        path: String,
        #[arg(long)]
        input_file: Option<String>,
        #[arg(long)]
        stdin: bool,
        #[arg(long, default_value = "utf8")]
        encoding: CliContentEncoding,
        #[arg(long)]
        mode: Option<String>,
        #[arg(long)]
        create_parent: bool,
        #[arg(long)]
        atomic: bool,
        #[arg(long)]
        if_missing: bool,
        #[arg(long)]
        if_sha256: Option<String>,
    },
    Mkdir {
        path: String,
        #[arg(long)]
        parents: bool,
        #[arg(long)]
        mode: Option<String>,
    },
    Rm {
        path: String,
        #[arg(long)]
        recursive: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        if_exists: bool,
    },
    Move {
        source: String,
        destination: String,
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AgentTransferCommands {
    Push {
        #[arg(long)]
        verify: Option<CliTransferVerifyMode>,
        #[arg(long)]
        atomic: bool,
        #[arg(long)]
        if_changed: bool,
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },
    Pull {
        #[arg(long)]
        verify: Option<CliTransferVerifyMode>,
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliHashAlgorithm {
    Sha256,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliContentEncoding {
    Utf8,
    Base64,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliTransferVerifyMode {
    Size,
    Sha256,
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
    server_id: String,
    device_name: String,
    addr: SocketAddr,
    platform: String,
    protocol_version: u16,
    features: Vec<String>,
}

#[derive(Debug, Clone)]
struct LocalBatchFile {
    root_index: u32,
    path: PathBuf,
    size: u64,
}

#[derive(Debug, Clone)]
struct LocalBatchManifest {
    sources: Vec<PathBuf>,
    roots: Vec<TransferRoot>,
    entries: Vec<TransferEntry>,
    files: Vec<LocalBatchFile>,
    total_bytes: u64,
}

#[derive(Debug, Serialize)]
struct AgentEnvelope<T> {
    schema_version: &'static str,
    command: String,
    ok: bool,
    data: Option<T>,
    error: Option<AgentErrorBody>,
}

#[derive(Debug, Serialize)]
struct AgentEventEnvelope<T> {
    schema_version: &'static str,
    command: &'static str,
    event: &'static str,
    data: Option<T>,
    error: Option<AgentErrorBody>,
}

#[derive(Debug, Serialize)]
struct AgentErrorBody {
    code: String,
    message: String,
    target: Option<String>,
    retryable: bool,
    details: serde_json::Value,
}

#[derive(Debug)]
struct AgentCommandFailure {
    code: String,
    message: String,
    target: Option<String>,
    retryable: bool,
    details: serde_json::Value,
    exit_code: i32,
}

#[derive(Debug, Serialize)]
struct AgentDiscoverData {
    targets: Vec<AgentDiscoverTarget>,
}

#[derive(Debug, Serialize)]
struct AgentDiscoverTarget {
    target: String,
    server_id: String,
    device_name: Option<String>,
    platform: Option<String>,
    protocol_version: u16,
    compatible: bool,
    supported_operations: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AgentSchemaData {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    usage_rules: Vec<String>,
    response_envelope: String,
    error_fields: String,
    operations: Vec<AgentSchemaOperation>,
}

#[derive(Debug, Serialize)]
struct AgentSchemaOperation {
    name: String,
    purpose: String,
    #[serde(default, skip_serializing_if = "is_false")]
    target_required: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    options: Vec<AgentSchemaOption>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    positional: Vec<AgentSchemaPositional>,
    returns: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_events: Option<String>,
}

#[derive(Debug, Serialize)]
struct AgentSchemaOption {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "default", skip_serializing_if = "Option::is_none")]
    default_value: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    required: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    multiple: bool,
}

#[derive(Debug, Serialize)]
struct AgentSchemaPositional {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, skip_serializing_if = "is_false")]
    required: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    multiple: bool,
}

#[derive(Debug, Serialize)]
struct AgentExecData {
    target: String,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    status: i32,
    timed_out: bool,
    stdout: String,
    stderr: String,
    duration_ms: u64,
}

#[derive(Debug, Serialize)]
struct AgentExecChunkData {
    target: String,
    chunk: String,
}

#[derive(Debug, Serialize)]
struct AgentExecCompletionData {
    target: String,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    status: i32,
    timed_out: bool,
    duration_ms: u64,
}

#[derive(Debug, Serialize)]
struct AgentTransferPushData {
    target: String,
    sources: Vec<String>,
    destination: String,
    entries_written: u64,
    bytes_written: u64,
    skipped: bool,
    verification: Option<String>,
    verified: bool,
    atomic: bool,
}

#[derive(Debug, Serialize)]
struct AgentTransferPullData {
    target: String,
    sources: Vec<String>,
    destination: String,
    entries_received: u64,
    bytes_received: u64,
    total_bytes: u64,
    verification: Option<String>,
    verified: bool,
}

#[derive(Debug)]
struct PushSummary {
    entries_written: u64,
    bytes_written: u64,
    skipped: bool,
    verification: Option<String>,
    verified: bool,
}

#[derive(Debug)]
struct PullSummary {
    entries_received: u64,
    bytes_received: u64,
    total_bytes: u64,
    verification: Option<String>,
    verified: bool,
    roots: Vec<TransferRoot>,
    entries: Vec<TransferEntry>,
}

#[derive(Debug, Serialize)]
struct AgentFsListData {
    entries: Vec<AgentFsEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransferPathState {
    kind: FsEntryKind,
    size: Option<u64>,
    sha256: Option<String>,
}

#[derive(Debug, Clone)]
struct RemoteControlError {
    code: rsdb_proto::ErrorCode,
    message: String,
}

impl fmt::Display for RemoteControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "remote error {:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for RemoteControlError {}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let exit_code = match cli.command {
        Commands::Connect { addr, name } => {
            connect_command(&addr, name.as_deref()).await?;
            0
        }
        Commands::Disconnect { target } => {
            disconnect_command(target.as_deref())?;
            0
        }
        Commands::Devices => {
            devices_command().await?;
            0
        }
        Commands::Discover {
            probe_addr,
            timeout_ms,
        } => {
            discover_command(&probe_addr, timeout_ms).await?;
            0
        }
        Commands::Ping { target } => {
            ping_command(target.as_deref()).await?;
            0
        }
        Commands::Capability { target } => {
            capability_command(target.as_deref()).await?;
            0
        }
        Commands::Shell { target, command } => {
            shell_command(target.as_deref(), &command).await?;
            0
        }
        Commands::Push { target, paths } => {
            let (sources, destination) = split_sources_and_destination(&paths, "push")?;
            push_command(target.as_deref(), &sources, &destination).await?;
            0
        }
        Commands::Pull { target, paths } => {
            let (sources, destination) = split_sources_and_destination(&paths, "pull")?;
            pull_command(target.as_deref(), &sources, &destination).await?;
            0
        }
        Commands::Edit {
            target,
            editor,
            path,
        } => {
            edit_command(target.as_deref(), editor.as_deref(), &path).await?;
            0
        }
        Commands::CompleteEditPath { target, partial } => {
            complete_edit_path_command(target.as_deref(), &partial).await?;
            0
        }
        Commands::Agent { target, command } => agent_command(target.as_deref(), command).await?,
    };

    if exit_code != 0 {
        std::process::exit(normalize_exit_status(exit_code));
    }
    Ok(())
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

async fn discover_command(probe_addr: &str, timeout_ms: u64) -> Result<()> {
    let targets = discover_targets(probe_addr, timeout_ms).await?;
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

async fn discover_targets(probe_addr: &str, timeout_ms: u64) -> Result<Vec<DiscoveredTarget>> {
    let mut discovered = BTreeMap::<String, DiscoveredTarget>::new();
    for probe_ip in discovery_probe_addresses(probe_addr)? {
        for target in discover_targets_once(probe_ip, DEFAULT_RSDB_PORT, timeout_ms).await? {
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
            server_id: response.server_id,
            device_name: response.device_name,
            addr: SocketAddr::new(peer.ip(), response.tcp_port),
            platform: response.platform,
            protocol_version: response.protocol_version,
            features: response.features,
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
                server_id: response.server_id,
                device_name: response.device_name,
                addr,
                platform: response.platform,
                protocol_version: response.protocol_version,
                features: response.features,
            },
        );
    }

    Ok(discovered.into_values().collect())
}

type AgentResult<T> = std::result::Result<T, AgentCommandFailure>;

async fn agent_command(target: Option<&str>, command: AgentCommands) -> Result<i32> {
    match command {
        AgentCommands::Schema => agent_schema_command(target).await,
        AgentCommands::Discover {
            probe_addr,
            timeout_ms,
        } => agent_discover_command(target, &probe_addr, timeout_ms).await,
        AgentCommands::Exec {
            stream,
            check,
            cwd,
            timeout_secs,
            command,
        } => {
            agent_exec_command(
                target,
                stream,
                check,
                cwd.as_deref(),
                timeout_secs,
                &command,
            )
            .await
        }
        AgentCommands::Fs { command } => agent_fs_command(target, command).await,
        AgentCommands::Transfer { command } => agent_transfer_command(target, command).await,
    }
}

async fn agent_schema_command(target: Option<&str>) -> Result<i32> {
    if target.is_some() {
        return emit_agent_failure(
            "schema",
            invalid_request_failure(
                "schema does not accept --target",
                None,
                serde_json::json!({"field": "target"}),
            ),
        );
    }

    emit_agent_success("schema", agent_schema())?;
    Ok(0)
}

async fn agent_discover_command(
    target: Option<&str>,
    probe_addr: &str,
    timeout_ms: u64,
) -> Result<i32> {
    if target.is_some() {
        return emit_agent_failure(
            "discover",
            invalid_request_failure(
                "discover does not accept --target",
                None,
                serde_json::json!({"field": "target"}),
            ),
        );
    }

    let targets = match discover_targets(probe_addr, timeout_ms).await {
        Ok(targets) => targets,
        Err(err) => {
            return emit_agent_failure("discover", map_transport_error("discover", None, &err));
        }
    };

    let data = AgentDiscoverData {
        targets: targets
            .into_iter()
            .map(|target| {
                let compatible = target.protocol_version == PROTOCOL_VERSION;
                AgentDiscoverTarget {
                    target: target.addr.to_string(),
                    server_id: target.server_id,
                    device_name: Some(target.device_name),
                    platform: Some(target.platform),
                    protocol_version: target.protocol_version,
                    compatible,
                    supported_operations: supported_agent_operations(&target.features, compatible),
                }
            })
            .collect(),
    };
    emit_agent_success("discover", data)?;
    Ok(0)
}

async fn agent_exec_command(
    target: Option<&str>,
    stream: bool,
    check: bool,
    cwd: Option<&str>,
    timeout_secs: Option<u64>,
    command: &[String],
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("exec", err),
    };

    let (program, args) = match command.split_first() {
        Some((program, args)) => (program.clone(), args.to_vec()),
        None => {
            return emit_agent_failure(
                "exec",
                invalid_request_failure(
                    "exec requires a command after `--`",
                    Some(target),
                    serde_json::json!({"field": "command"}),
                ),
            );
        }
    };

    let exit_code = if stream {
        match run_agent_exec_stream(&target, &program, &args, cwd, timeout_secs, check).await {
            Ok(code) => code,
            Err(err) => return emit_agent_failure("exec", err),
        }
    } else {
        match run_agent_exec(&target, &program, &args, cwd, timeout_secs, check).await {
            Ok(code) => code,
            Err(err) => return emit_agent_failure("exec", err),
        }
    };
    Ok(exit_code)
}

async fn agent_transfer_command(
    target: Option<&str>,
    command: AgentTransferCommands,
) -> Result<i32> {
    match command {
        AgentTransferCommands::Push {
            verify,
            atomic,
            if_changed,
            paths,
        } => agent_transfer_push_command(target, &paths, verify, atomic, if_changed).await,
        AgentTransferCommands::Pull { verify, paths } => {
            agent_transfer_pull_command(target, &paths, verify).await
        }
    }
}

async fn agent_fs_command(target: Option<&str>, command: AgentFsCommands) -> Result<i32> {
    match command {
        AgentFsCommands::Stat { path, hash } => agent_fs_stat_command(target, &path, hash).await,
        AgentFsCommands::List {
            path,
            recursive,
            max_depth,
            include_hidden,
            hash,
        } => agent_fs_list_command(target, &path, recursive, max_depth, include_hidden, hash).await,
        AgentFsCommands::Read {
            path,
            encoding,
            max_bytes,
        } => agent_fs_read_command(target, &path, encoding, max_bytes).await,
        AgentFsCommands::Write {
            path,
            input_file,
            stdin,
            encoding,
            mode,
            create_parent,
            atomic,
            if_missing,
            if_sha256,
        } => {
            agent_fs_write_command(
                target,
                &path,
                input_file.as_deref(),
                stdin,
                encoding,
                mode.as_deref(),
                create_parent,
                atomic,
                if_missing,
                if_sha256.as_deref(),
            )
            .await
        }
        AgentFsCommands::Mkdir {
            path,
            parents,
            mode,
        } => agent_fs_mkdir_command(target, &path, parents, mode.as_deref()).await,
        AgentFsCommands::Rm {
            path,
            recursive,
            force,
            if_exists,
        } => agent_fs_rm_command(target, &path, recursive, force, if_exists).await,
        AgentFsCommands::Move {
            source,
            destination,
            overwrite,
        } => agent_fs_move_command(target, &source, &destination, overwrite).await,
    }
}

async fn agent_transfer_push_command(
    target: Option<&str>,
    paths: &[String],
    verify: Option<CliTransferVerifyMode>,
    atomic: bool,
    if_changed: bool,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("transfer.push", err),
    };
    let (sources, destination) = match split_sources_and_destination(paths, "agent transfer push") {
        Ok(parts) => parts,
        Err(err) => {
            return emit_agent_failure(
                "transfer.push",
                invalid_request_failure(err.to_string(), Some(target), serde_json::json!({})),
            );
        }
    };

    match run_agent_transfer_push(&target, &sources, &destination, verify, atomic, if_changed).await
    {
        Ok(summary) => {
            emit_agent_success(
                "transfer.push",
                AgentTransferPushData {
                    target,
                    sources,
                    destination,
                    entries_written: summary.entries_written,
                    bytes_written: summary.bytes_written,
                    skipped: summary.skipped,
                    verification: summary.verification,
                    verified: summary.verified,
                    atomic,
                },
            )?;
            Ok(0)
        }
        Err(err) => emit_agent_failure("transfer.push", err),
    }
}

async fn agent_transfer_pull_command(
    target: Option<&str>,
    paths: &[String],
    verify: Option<CliTransferVerifyMode>,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("transfer.pull", err),
    };
    let (sources, destination) = match split_sources_and_destination(paths, "agent transfer pull") {
        Ok(parts) => parts,
        Err(err) => {
            return emit_agent_failure(
                "transfer.pull",
                invalid_request_failure(err.to_string(), Some(target), serde_json::json!({})),
            );
        }
    };

    match run_agent_transfer_pull(&target, &sources, &destination, verify).await {
        Ok(summary) => {
            emit_agent_success(
                "transfer.pull",
                AgentTransferPullData {
                    target,
                    sources,
                    destination,
                    entries_received: summary.entries_received,
                    bytes_received: summary.bytes_received,
                    total_bytes: summary.total_bytes,
                    verification: summary.verification,
                    verified: summary.verified,
                },
            )?;
            Ok(0)
        }
        Err(err) => emit_agent_failure("transfer.pull", err),
    }
}

async fn run_agent_exec(
    target: &str,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_secs: Option<u64>,
    check: bool,
) -> AgentResult<i32> {
    let started = Instant::now();
    let response = request(
        target,
        ControlRequest::Exec {
            command: program.to_string(),
            args: args.to_vec(),
            cwd: cwd.map(ToOwned::to_owned),
            timeout_secs,
            stream: false,
        },
    )
    .await
    .map_err(|err| map_transport_error("exec", Some(target.to_string()), &err))?;

    match response {
        ControlResponse::ExecResult {
            status,
            stdout,
            stderr,
            timed_out,
        } => {
            let data = AgentExecData {
                target: target.to_string(),
                command: program.to_string(),
                args: args.to_vec(),
                cwd: cwd.map(ToOwned::to_owned),
                status,
                timed_out,
                stdout,
                stderr,
                duration_ms: elapsed_ms(started),
            };
            if check && status != 0 {
                return Err(exec_nonzero_failure(target.to_string(), &data));
            }
            emit_agent_success("exec", data).map_err(internal_agent_failure)?;
            Ok(0)
        }
        ControlResponse::Error { code, message } => Err(map_remote_exec_error(
            target.to_string(),
            code,
            message,
            serde_json::json!({
                "command": program,
                "args": args,
                "cwd": cwd,
                "timeout_secs": timeout_secs,
                "duration_ms": elapsed_ms(started),
            }),
        )),
        other => Err(internal_agent_failure(anyhow!(
            "unexpected response from daemon: {other:?}"
        ))),
    }
}

async fn run_agent_exec_stream(
    target: &str,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_secs: Option<u64>,
    check: bool,
) -> AgentResult<i32> {
    let started = Instant::now();
    let mut stream = open_connection(target)
        .await
        .map_err(|err| map_transport_error("exec", Some(target.to_string()), &err))?;
    write_json_frame(
        &mut stream,
        FrameKind::Request,
        REQUEST_ID,
        &ControlRequest::Exec {
            command: program.to_string(),
            args: args.to_vec(),
            cwd: cwd.map(ToOwned::to_owned),
            timeout_secs,
            stream: true,
        },
    )
    .await
    .map_err(|err| internal_agent_failure(anyhow!("failed to send exec request: {err}")))?;

    loop {
        let frame = match read_frame(&mut stream).await {
            Ok(frame) => frame,
            Err(err) => {
                let failure =
                    map_transport_error("exec", Some(target.to_string()), &anyhow!("{err}"));
                emit_agent_event_failure::<serde_json::Value>("exec", "failed", failure, None)
                    .map_err(internal_agent_failure)?;
                return Ok(1);
            }
        };
        match frame.header.kind {
            FrameKind::Stream => {
                let chunk = match decode_stream_frame(frame) {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        emit_agent_event_failure::<serde_json::Value>(
                            "exec",
                            "failed",
                            internal_agent_failure(anyhow!("invalid stream frame: {err}")),
                            None,
                        )
                        .map_err(internal_agent_failure)?;
                        return Ok(1);
                    }
                };
                if let Err(err) = ensure_request_id(chunk.request_id, REQUEST_ID) {
                    emit_agent_event_failure::<serde_json::Value>(
                        "exec",
                        "failed",
                        internal_agent_failure(err),
                        None,
                    )
                    .map_err(internal_agent_failure)?;
                    return Ok(1);
                }
                let event = match chunk.channel {
                    StreamChannel::Stdout => Some("stdout"),
                    StreamChannel::Stderr => Some("stderr"),
                    _ => None,
                };
                if let Some(event) = event {
                    if !chunk.payload.is_empty() {
                        emit_agent_event(
                            "exec",
                            event,
                            Some(AgentExecChunkData {
                                target: target.to_string(),
                                chunk: String::from_utf8_lossy(&chunk.payload).into_owned(),
                            }),
                        )
                        .map_err(internal_agent_failure)?;
                    }
                }
            }
            FrameKind::Response => {
                let response: ControlResponse = match decode_json(&frame, FrameKind::Response) {
                    Ok(response) => response,
                    Err(err) => {
                        emit_agent_event_failure::<serde_json::Value>(
                            "exec",
                            "failed",
                            internal_agent_failure(anyhow!("invalid response frame: {err}")),
                            None,
                        )
                        .map_err(internal_agent_failure)?;
                        return Ok(1);
                    }
                };
                match response {
                    ControlResponse::ExecResult {
                        status,
                        stdout,
                        stderr,
                        timed_out,
                    } => {
                        if !stdout.is_empty() {
                            emit_agent_event(
                                "exec",
                                "stdout",
                                Some(AgentExecChunkData {
                                    target: target.to_string(),
                                    chunk: stdout,
                                }),
                            )
                            .map_err(internal_agent_failure)?;
                        }
                        if !stderr.is_empty() {
                            emit_agent_event(
                                "exec",
                                "stderr",
                                Some(AgentExecChunkData {
                                    target: target.to_string(),
                                    chunk: stderr,
                                }),
                            )
                            .map_err(internal_agent_failure)?;
                        }

                        let data = AgentExecCompletionData {
                            target: target.to_string(),
                            command: program.to_string(),
                            args: args.to_vec(),
                            cwd: cwd.map(ToOwned::to_owned),
                            status,
                            timed_out,
                            duration_ms: elapsed_ms(started),
                        };
                        if check && status != 0 {
                            emit_agent_event_failure(
                                "exec",
                                "failed",
                                exec_nonzero_failure_from_completion(target.to_string(), &data),
                                Some(data),
                            )
                            .map_err(internal_agent_failure)?;
                            return Ok(status);
                        }

                        emit_agent_event("exec", "completed", Some(data))
                            .map_err(internal_agent_failure)?;
                        return Ok(0);
                    }
                    ControlResponse::Error { code, message } => {
                        let failure = map_remote_exec_error(
                            target.to_string(),
                            code,
                            message,
                            serde_json::json!({
                                "command": program,
                                "args": args,
                                "cwd": cwd,
                                "timeout_secs": timeout_secs,
                                "duration_ms": elapsed_ms(started),
                            }),
                        );
                        emit_agent_event_failure::<serde_json::Value>(
                            "exec", "failed", failure, None,
                        )
                        .map_err(internal_agent_failure)?;
                        return Ok(1);
                    }
                    other => {
                        emit_agent_event_failure::<serde_json::Value>(
                            "exec",
                            "failed",
                            internal_agent_failure(anyhow!(
                                "unexpected response from daemon: {other:?}"
                            )),
                            None,
                        )
                        .map_err(internal_agent_failure)?;
                        return Ok(1);
                    }
                }
            }
            other => {
                emit_agent_event_failure::<serde_json::Value>(
                    "exec",
                    "failed",
                    internal_agent_failure(anyhow!(
                        "unexpected frame kind during exec stream: {other:?}"
                    )),
                    None,
                )
                .map_err(internal_agent_failure)?;
                return Ok(1);
            }
        }
    }
}

async fn agent_fs_stat_command(
    target: Option<&str>,
    path: &str,
    hash: Option<CliHashAlgorithm>,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("fs.stat", err),
    };
    match agent_request(
        "fs.stat",
        &target,
        ControlRequest::FsStat {
            path: path.to_string(),
            hash: hash.map(cli_hash_algorithm),
        },
        serde_json::json!({ "path": path }),
    )
    .await
    {
        Ok(ControlResponse::FsStat { stat }) => {
            emit_agent_success("fs.stat", stat)?;
            Ok(0)
        }
        Ok(other) => emit_agent_failure(
            "fs.stat",
            internal_agent_failure(anyhow!("unexpected response from daemon: {other:?}")),
        ),
        Err(err) => emit_agent_failure("fs.stat", err),
    }
}

async fn agent_fs_list_command(
    target: Option<&str>,
    path: &str,
    recursive: bool,
    max_depth: Option<u32>,
    include_hidden: bool,
    hash: Option<CliHashAlgorithm>,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("fs.list", err),
    };
    match agent_request(
        "fs.list",
        &target,
        ControlRequest::FsList {
            path: path.to_string(),
            recursive,
            max_depth,
            include_hidden,
            hash: hash.map(cli_hash_algorithm),
        },
        serde_json::json!({ "path": path }),
    )
    .await
    {
        Ok(ControlResponse::FsList { entries }) => {
            emit_agent_success("fs.list", AgentFsListData { entries })?;
            Ok(0)
        }
        Ok(other) => emit_agent_failure(
            "fs.list",
            internal_agent_failure(anyhow!("unexpected response from daemon: {other:?}")),
        ),
        Err(err) => emit_agent_failure("fs.list", err),
    }
}

async fn agent_fs_read_command(
    target: Option<&str>,
    path: &str,
    encoding: CliContentEncoding,
    max_bytes: Option<u64>,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("fs.read", err),
    };
    match agent_request(
        "fs.read",
        &target,
        ControlRequest::FsRead {
            path: path.to_string(),
            encoding: cli_content_encoding(encoding),
            max_bytes,
        },
        serde_json::json!({ "path": path }),
    )
    .await
    {
        Ok(ControlResponse::FsReadResult { result }) => {
            emit_agent_success("fs.read", result)?;
            Ok(0)
        }
        Ok(other) => emit_agent_failure(
            "fs.read",
            internal_agent_failure(anyhow!("unexpected response from daemon: {other:?}")),
        ),
        Err(err) => emit_agent_failure("fs.read", err),
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent_fs_write_command(
    target: Option<&str>,
    path: &str,
    input_file: Option<&str>,
    stdin: bool,
    encoding: CliContentEncoding,
    mode: Option<&str>,
    create_parent: bool,
    atomic: bool,
    if_missing: bool,
    if_sha256: Option<&str>,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("fs.write", err),
    };
    let mode = match mode.map(parse_mode) {
        Some(Ok(mode)) => Some(mode),
        Some(Err(err)) => return emit_agent_failure("fs.write", err),
        None => None,
    };
    let bytes = match read_fs_write_input(input_file, stdin) {
        Ok(bytes) => bytes,
        Err(err) => return emit_agent_failure("fs.write", err),
    };
    let content = match encode_local_content(bytes, &encoding) {
        Ok(content) => content,
        Err(err) => return emit_agent_failure("fs.write", err),
    };

    match agent_request(
        "fs.write",
        &target,
        ControlRequest::FsWrite {
            path: path.to_string(),
            content,
            encoding: cli_content_encoding(encoding),
            mode,
            create_parent,
            atomic,
            if_missing,
            if_sha256: if_sha256.map(ToOwned::to_owned),
        },
        serde_json::json!({ "path": path }),
    )
    .await
    {
        Ok(ControlResponse::FsWriteResult { result }) => {
            emit_agent_success("fs.write", result)?;
            Ok(0)
        }
        Ok(other) => emit_agent_failure(
            "fs.write",
            internal_agent_failure(anyhow!("unexpected response from daemon: {other:?}")),
        ),
        Err(err) => emit_agent_failure("fs.write", err),
    }
}

async fn agent_fs_mkdir_command(
    target: Option<&str>,
    path: &str,
    parents: bool,
    mode: Option<&str>,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("fs.mkdir", err),
    };
    let mode = match mode.map(parse_mode) {
        Some(Ok(mode)) => Some(mode),
        Some(Err(err)) => return emit_agent_failure("fs.mkdir", err),
        None => None,
    };

    match agent_request(
        "fs.mkdir",
        &target,
        ControlRequest::FsMkdir {
            path: path.to_string(),
            parents,
            mode,
        },
        serde_json::json!({ "path": path }),
    )
    .await
    {
        Ok(ControlResponse::FsMkdirResult { result }) => {
            emit_agent_success("fs.mkdir", result)?;
            Ok(0)
        }
        Ok(other) => emit_agent_failure(
            "fs.mkdir",
            internal_agent_failure(anyhow!("unexpected response from daemon: {other:?}")),
        ),
        Err(err) => emit_agent_failure("fs.mkdir", err),
    }
}

async fn agent_fs_rm_command(
    target: Option<&str>,
    path: &str,
    recursive: bool,
    force: bool,
    if_exists: bool,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("fs.rm", err),
    };

    match agent_request(
        "fs.rm",
        &target,
        ControlRequest::FsRm {
            path: path.to_string(),
            recursive,
            force,
            if_exists,
        },
        serde_json::json!({ "path": path }),
    )
    .await
    {
        Ok(ControlResponse::FsRmResult { result }) => {
            emit_agent_success("fs.rm", result)?;
            Ok(0)
        }
        Ok(other) => emit_agent_failure(
            "fs.rm",
            internal_agent_failure(anyhow!("unexpected response from daemon: {other:?}")),
        ),
        Err(err) => emit_agent_failure("fs.rm", err),
    }
}

async fn agent_fs_move_command(
    target: Option<&str>,
    source: &str,
    destination: &str,
    overwrite: bool,
) -> Result<i32> {
    let target = match require_agent_target(target) {
        Ok(target) => target,
        Err(err) => return emit_agent_failure("fs.move", err),
    };

    match agent_request(
        "fs.move",
        &target,
        ControlRequest::FsMove {
            source: source.to_string(),
            destination: destination.to_string(),
            overwrite,
        },
        serde_json::json!({ "source": source, "destination": destination }),
    )
    .await
    {
        Ok(ControlResponse::FsMoveResult { result }) => {
            emit_agent_success("fs.move", result)?;
            Ok(0)
        }
        Ok(other) => emit_agent_failure(
            "fs.move",
            internal_agent_failure(anyhow!("unexpected response from daemon: {other:?}")),
        ),
        Err(err) => emit_agent_failure("fs.move", err),
    }
}

async fn run_agent_transfer_push(
    target: &str,
    source_specs: &[String],
    destination: &str,
    verify: Option<CliTransferVerifyMode>,
    atomic: bool,
    if_changed: bool,
) -> AgentResult<PushSummary> {
    let details = serde_json::json!({
        "sources": source_specs,
        "destination": destination,
        "verify": verify.map(transfer_verify_name),
        "atomic": atomic,
        "if_changed": if_changed,
    });
    let manifest = build_local_batch_manifest(source_specs).map_err(|err| {
        map_operation_error(
            "transfer.push",
            Some(target.to_string()),
            &err,
            details.clone(),
        )
    })?;
    let final_root_paths = resolve_remote_batch_roots(target, destination, &manifest.roots).await?;

    if !atomic && !if_changed {
        let summary = execute_push_manifest(target, &manifest, destination)
            .await
            .map_err(|err| {
                map_operation_error(
                    "transfer.push",
                    Some(target.to_string()),
                    &err,
                    details.clone(),
                )
            })?;
        if let Some(mode) = verify {
            for (source, remote_root) in manifest.sources.iter().zip(final_root_paths.iter()) {
                let local_state = collect_local_transfer_state(source, verify_uses_hash(mode))
                    .map_err(|err| {
                        map_operation_error(
                            "transfer.push",
                            Some(target.to_string()),
                            &err,
                            serde_json::json!({
                                "source": source.display().to_string(),
                                "destination": remote_root.display().to_string(),
                            }),
                        )
                    })?;
                let remote_state =
                    collect_remote_transfer_state(target, remote_root, verify_uses_hash(mode))
                        .await?;
                let verified = remote_state
                    .as_ref()
                    .is_some_and(|remote_state| transfer_states_match(&local_state, remote_state));
                if !verified {
                    return Err(transfer_verification_failure(
                        target.to_string(),
                        format!(
                            "remote destination does not match pushed content: {}",
                            remote_root.display()
                        ),
                        serde_json::json!({
                            "source": source.display().to_string(),
                            "destination": remote_root.display().to_string(),
                            "verification": transfer_verify_name(mode),
                        }),
                    ));
                }
            }
        }

        return Ok(PushSummary {
            entries_written: summary.entries_written,
            bytes_written: summary.bytes_written,
            skipped: false,
            verification: verify.map(transfer_verify_name).map(ToOwned::to_owned),
            verified: verify.is_some(),
        });
    }

    let mut entries_written = 0_u64;
    let mut bytes_written = 0_u64;
    let mut changed_roots = 0_u64;

    for (index, (source, remote_root)) in manifest
        .sources
        .iter()
        .zip(final_root_paths.iter())
        .enumerate()
    {
        if if_changed {
            let local_state = collect_local_transfer_state(source, true).map_err(|err| {
                map_operation_error(
                    "transfer.push",
                    Some(target.to_string()),
                    &err,
                    serde_json::json!({
                        "source": source.display().to_string(),
                        "destination": remote_root.display().to_string(),
                    }),
                )
            })?;
            let remote_state = collect_remote_transfer_state(target, remote_root, true).await?;
            if remote_state
                .as_ref()
                .is_some_and(|remote_state| transfer_states_match(&local_state, remote_state))
            {
                continue;
            }
        }

        let temp_root = staged_remote_transfer_path(remote_root, index);
        let temp_root_string = temp_root.display().to_string();
        let final_root_string = remote_root.display().to_string();
        let source_string = source.display().to_string();
        let manifest_source_string = source_string.clone();
        let manifest_destination_string = final_root_string.clone();
        let single_manifest =
            filter_local_batch_manifest(&manifest, &[index as u32]).map_err(|err| {
                map_operation_error(
                    "transfer.push",
                    Some(target.to_string()),
                    &err,
                    serde_json::json!({
                        "source": manifest_source_string,
                        "destination": manifest_destination_string,
                    }),
                )
            })?;

        let push_result = execute_push_manifest(target, &single_manifest, &temp_root_string).await;
        let push_summary = match push_result {
            Ok(summary) => summary,
            Err(err) => {
                let _ = cleanup_remote_path(target, &temp_root_string).await;
                return Err(map_operation_error(
                    "transfer.push",
                    Some(target.to_string()),
                    &err,
                    serde_json::json!({
                        "source": source_string,
                        "destination": final_root_string,
                    }),
                ));
            }
        };

        if let Err(err) =
            remote_fs_move_request(target, &temp_root_string, &final_root_string, true).await
        {
            let _ = cleanup_remote_path(target, &temp_root_string).await;
            return Err(err);
        }

        entries_written += push_summary.entries_written;
        bytes_written += push_summary.bytes_written;
        changed_roots += 1;
    }

    if let Some(mode) = verify {
        for (source, remote_root) in manifest.sources.iter().zip(final_root_paths.iter()) {
            let local_state = collect_local_transfer_state(source, verify_uses_hash(mode))
                .map_err(|err| {
                    map_operation_error(
                        "transfer.push",
                        Some(target.to_string()),
                        &err,
                        serde_json::json!({
                            "source": source.display().to_string(),
                            "destination": remote_root.display().to_string(),
                        }),
                    )
                })?;
            let remote_state =
                collect_remote_transfer_state(target, remote_root, verify_uses_hash(mode)).await?;
            let verified = remote_state
                .as_ref()
                .is_some_and(|remote_state| transfer_states_match(&local_state, remote_state));
            if !verified {
                return Err(transfer_verification_failure(
                    target.to_string(),
                    format!(
                        "remote destination does not match pushed content: {}",
                        remote_root.display()
                    ),
                    serde_json::json!({
                        "source": source.display().to_string(),
                        "destination": remote_root.display().to_string(),
                        "verification": transfer_verify_name(mode),
                    }),
                ));
            }
        }
    }

    Ok(PushSummary {
        entries_written,
        bytes_written,
        skipped: if_changed && changed_roots == 0,
        verification: verify.map(transfer_verify_name).map(ToOwned::to_owned),
        verified: verify.is_some(),
    })
}

async fn run_agent_transfer_pull(
    target: &str,
    source_specs: &[String],
    destination: &str,
    verify: Option<CliTransferVerifyMode>,
) -> AgentResult<PullSummary> {
    let details = serde_json::json!({
        "sources": source_specs,
        "destination": destination,
        "verify": verify.map(transfer_verify_name),
    });
    let remote_verify_roots = if matches!(verify, Some(CliTransferVerifyMode::Sha256)) {
        Some(resolve_sha256_pull_sources(target, source_specs).await?)
    } else {
        None
    };

    let summary = execute_pull(target, source_specs, destination)
        .await
        .map_err(|err| {
            map_operation_error(
                "transfer.pull",
                Some(target.to_string()),
                &err,
                details.clone(),
            )
        })?;

    if let Some(mode) = verify {
        match mode {
            CliTransferVerifyMode::Size => verify_local_pull_against_manifest(
                Path::new(destination),
                destination,
                &summary.roots,
                &summary.entries,
            )
            .map_err(|err| {
                transfer_verification_failure(
                    target.to_string(),
                    err.to_string(),
                    serde_json::json!({
                        "sources": source_specs,
                        "destination": destination,
                        "verification": transfer_verify_name(mode),
                    }),
                )
            })?,
            CliTransferVerifyMode::Sha256 => {
                let remote_roots = remote_verify_roots.ok_or_else(|| {
                    internal_agent_failure(anyhow!("missing remote verify roots"))
                })?;
                let destination_path = PathBuf::from(destination);
                let local_root_paths =
                    resolve_local_batch_roots(&destination_path, destination, &summary.roots)
                        .map_err(internal_agent_failure)?;
                if remote_roots.len() != local_root_paths.len() {
                    return Err(transfer_verification_failure(
                        target.to_string(),
                        "pull root count did not match requested sources".to_string(),
                        serde_json::json!({
                            "sources": source_specs,
                            "destination": destination,
                            "verification": transfer_verify_name(mode),
                        }),
                    ));
                }

                for (remote_root, local_root) in remote_roots.iter().zip(local_root_paths.iter()) {
                    let expected = collect_remote_transfer_state(target, remote_root, true).await?;
                    let expected = expected.ok_or_else(|| {
                        transfer_verification_failure(
                            target.to_string(),
                            format!(
                                "remote source disappeared during verification: {}",
                                remote_root.display()
                            ),
                            serde_json::json!({
                                "source": remote_root.display().to_string(),
                                "destination": local_root.display().to_string(),
                                "verification": transfer_verify_name(mode),
                            }),
                        )
                    })?;
                    let actual = collect_local_transfer_state(local_root, true).map_err(|err| {
                        transfer_verification_failure(
                            target.to_string(),
                            err.to_string(),
                            serde_json::json!({
                                "source": remote_root.display().to_string(),
                                "destination": local_root.display().to_string(),
                                "verification": transfer_verify_name(mode),
                            }),
                        )
                    })?;
                    if !transfer_states_match(&expected, &actual) {
                        return Err(transfer_verification_failure(
                            target.to_string(),
                            format!(
                                "local destination does not match remote source: {}",
                                remote_root.display()
                            ),
                            serde_json::json!({
                                "source": remote_root.display().to_string(),
                                "destination": local_root.display().to_string(),
                                "verification": transfer_verify_name(mode),
                            }),
                        ));
                    }
                }
            }
        }
    }

    Ok(PullSummary {
        entries_received: summary.entries_received,
        bytes_received: summary.bytes_received,
        total_bytes: summary.total_bytes,
        verification: verify.map(transfer_verify_name).map(ToOwned::to_owned),
        verified: verify.is_some(),
        roots: summary.roots,
        entries: summary.entries,
    })
}

async fn resolve_remote_batch_roots(
    target: &str,
    destination: &str,
    roots: &[TransferRoot],
) -> AgentResult<Vec<PathBuf>> {
    ensure_unique_root_names(roots).map_err(internal_agent_failure)?;
    let destination_stat = remote_fs_stat_request(target, destination, None).await?;
    let destination_path = PathBuf::from(destination);

    if roots.len() == 1 {
        let treat_as_directory = destination_stat.exists
            && matches!(destination_stat.kind, Some(FsEntryKind::Directory))
            || destination.ends_with('/');
        let root = &roots[0];
        let base = if treat_as_directory {
            destination_path.join(&root.source_name)
        } else {
            destination_path
        };
        return Ok(vec![base]);
    }

    if destination_stat.exists && !matches!(destination_stat.kind, Some(FsEntryKind::Directory)) {
        return Err(invalid_request_failure(
            format!("remote destination is not a directory: {destination}"),
            Some(target.to_string()),
            serde_json::json!({ "destination": destination }),
        ));
    }

    Ok(roots
        .iter()
        .map(|root| destination_path.join(&root.source_name))
        .collect())
}

async fn resolve_sha256_pull_sources(
    target: &str,
    source_specs: &[String],
) -> AgentResult<Vec<PathBuf>> {
    let mut concrete = Vec::with_capacity(source_specs.len());
    for source in source_specs {
        let stat = remote_fs_stat_request(target, source, None).await?;
        if !stat.exists && has_glob_pattern(source) {
            return Err(invalid_request_failure(
                format!(
                    "sha256 verification requires literal remote sources; wildcard source `{source}` is ambiguous"
                ),
                Some(target.to_string()),
                serde_json::json!({ "source": source }),
            ));
        }
        concrete.push(PathBuf::from(source));
    }
    Ok(concrete)
}

async fn remote_fs_stat_request(
    target: &str,
    path: &str,
    hash: Option<HashAlgorithm>,
) -> AgentResult<AgentFsStat> {
    match agent_request(
        "fs.stat",
        target,
        ControlRequest::FsStat {
            path: path.to_string(),
            hash,
        },
        serde_json::json!({ "path": path }),
    )
    .await?
    {
        ControlResponse::FsStat { stat } => Ok(stat),
        other => Err(internal_agent_failure(anyhow!(
            "unexpected response from daemon: {other:?}"
        ))),
    }
}

async fn remote_fs_list_request(
    target: &str,
    path: &str,
    hash: Option<HashAlgorithm>,
) -> AgentResult<Vec<AgentFsEntry>> {
    match agent_request(
        "fs.list",
        target,
        ControlRequest::FsList {
            path: path.to_string(),
            recursive: true,
            max_depth: None,
            include_hidden: true,
            hash,
        },
        serde_json::json!({ "path": path }),
    )
    .await?
    {
        ControlResponse::FsList { entries } => Ok(entries),
        other => Err(internal_agent_failure(anyhow!(
            "unexpected response from daemon: {other:?}"
        ))),
    }
}

async fn remote_fs_move_request(
    target: &str,
    source: &str,
    destination: &str,
    overwrite: bool,
) -> AgentResult<()> {
    match agent_request(
        "fs.move",
        target,
        ControlRequest::FsMove {
            source: source.to_string(),
            destination: destination.to_string(),
            overwrite,
        },
        serde_json::json!({ "source": source, "destination": destination }),
    )
    .await?
    {
        ControlResponse::FsMoveResult { .. } => Ok(()),
        other => Err(internal_agent_failure(anyhow!(
            "unexpected response from daemon: {other:?}"
        ))),
    }
}

async fn cleanup_remote_path(target: &str, path: &str) -> AgentResult<()> {
    match agent_request(
        "fs.rm",
        target,
        ControlRequest::FsRm {
            path: path.to_string(),
            recursive: true,
            force: true,
            if_exists: true,
        },
        serde_json::json!({ "path": path }),
    )
    .await?
    {
        ControlResponse::FsRmResult { .. } => Ok(()),
        other => Err(internal_agent_failure(anyhow!(
            "unexpected response from daemon: {other:?}"
        ))),
    }
}

fn verify_uses_hash(mode: CliTransferVerifyMode) -> bool {
    matches!(mode, CliTransferVerifyMode::Sha256)
}

fn staged_remote_transfer_path(final_root: &Path, index: usize) -> PathBuf {
    let parent = final_root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = final_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("rsdb-transfer");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{file_name}.rsdb-transfer-{nonce}-{index}"))
}

fn collect_local_transfer_state(
    root: &Path,
    include_hash: bool,
) -> Result<BTreeMap<String, TransferPathState>> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("failed to stat local path {}", root.display()))?;
    let mut state = BTreeMap::new();
    collect_local_transfer_state_recursive(root, root, &metadata, include_hash, &mut state)?;
    Ok(state)
}

fn collect_local_transfer_state_recursive(
    root: &Path,
    path: &Path,
    metadata: &std::fs::Metadata,
    include_hash: bool,
    state: &mut BTreeMap<String, TransferPathState>,
) -> Result<()> {
    let relative = relative_transfer_path(root, path)?;
    state.insert(
        relative,
        TransferPathState {
            kind: local_fs_entry_kind(metadata),
            size: metadata.is_file().then_some(metadata.len()),
            sha256: if include_hash && metadata.is_file() {
                Some(sha256_local_path(path)?)
            } else {
                None
            },
        },
    );

    if metadata.is_dir() {
        let mut children = fs::read_dir(path)
            .with_context(|| format!("failed to read local directory {}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("failed to read local directory {}", path.display()))?;
        children.sort();
        for child in children {
            let child_metadata = fs::symlink_metadata(&child)
                .with_context(|| format!("failed to stat local path {}", child.display()))?;
            collect_local_transfer_state_recursive(
                root,
                &child,
                &child_metadata,
                include_hash,
                state,
            )?;
        }
    }

    Ok(())
}

async fn collect_remote_transfer_state(
    target: &str,
    root: &Path,
    include_hash: bool,
) -> AgentResult<Option<BTreeMap<String, TransferPathState>>> {
    let root_string = root.display().to_string();
    let stat = remote_fs_stat_request(
        target,
        &root_string,
        include_hash.then_some(HashAlgorithm::Sha256),
    )
    .await?;
    if !stat.exists {
        return Ok(None);
    }

    let mut state = BTreeMap::new();
    state.insert(String::new(), transfer_state_from_stat(&stat)?);

    if matches!(stat.kind, Some(FsEntryKind::Directory)) {
        let entries = remote_fs_list_request(
            target,
            &root_string,
            include_hash.then_some(HashAlgorithm::Sha256),
        )
        .await?;
        for entry in entries {
            let relative = relative_transfer_path(root, Path::new(&entry.path))
                .map_err(internal_agent_failure)?;
            state.insert(relative, transfer_state_from_entry(&entry));
        }
    }

    Ok(Some(state))
}

fn transfer_states_match(
    expected: &BTreeMap<String, TransferPathState>,
    actual: &BTreeMap<String, TransferPathState>,
) -> bool {
    expected.iter().all(|(path, expected_state)| {
        actual.get(path).is_some_and(|actual_state| {
            expected_state.kind == actual_state.kind
                && expected_state.size == actual_state.size
                && match (&expected_state.sha256, &actual_state.sha256) {
                    (Some(expected), Some(actual)) => expected == actual,
                    (Some(_), None) => false,
                    _ => true,
                }
        })
    })
}

fn verify_local_pull_against_manifest(
    destination: &Path,
    destination_arg: &str,
    roots: &[TransferRoot],
    entries: &[TransferEntry],
) -> Result<()> {
    let local_root_paths = resolve_local_batch_roots(destination, destination_arg, roots)?;
    for (index, local_root) in local_root_paths.iter().enumerate() {
        let expected = expected_transfer_state_for_root(entries, index as u32);
        let actual = collect_local_transfer_state(local_root, false)?;
        if !transfer_states_match(&expected, &actual) {
            bail!(
                "local destination does not match pulled manifest: {}",
                local_root.display()
            );
        }
    }
    Ok(())
}

fn expected_transfer_state_for_root(
    entries: &[TransferEntry],
    root_index: u32,
) -> BTreeMap<String, TransferPathState> {
    let mut state = BTreeMap::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.root_index == root_index)
    {
        state.insert(
            entry.relative_path.clone(),
            TransferPathState {
                kind: transfer_entry_kind_to_fs_kind(&entry.kind),
                size: matches!(entry.kind, TransferEntryKind::File).then_some(entry.size),
                sha256: None,
            },
        );
    }
    state
}

fn transfer_entry_kind_to_fs_kind(kind: &TransferEntryKind) -> FsEntryKind {
    match kind {
        TransferEntryKind::File => FsEntryKind::File,
        TransferEntryKind::Directory => FsEntryKind::Directory,
    }
}

fn transfer_state_from_stat(stat: &AgentFsStat) -> AgentResult<TransferPathState> {
    let kind = stat.kind.clone().ok_or_else(|| {
        internal_agent_failure(anyhow!(
            "fs.stat returned no kind for existing path {}",
            stat.path
        ))
    })?;
    Ok(TransferPathState {
        size: matches!(kind, FsEntryKind::File).then_some(stat.size.unwrap_or(0)),
        sha256: stat.sha256.clone(),
        kind,
    })
}

fn transfer_state_from_entry(entry: &AgentFsEntry) -> TransferPathState {
    TransferPathState {
        kind: entry.kind.clone(),
        size: matches!(entry.kind, FsEntryKind::File).then_some(entry.size),
        sha256: entry.sha256.clone(),
    }
}

fn relative_transfer_path(root: &Path, path: &Path) -> Result<String> {
    if root == path {
        return Ok(String::new());
    }
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "path {} is not under root {}",
            path.display(),
            root.display()
        )
    })?;
    path_to_transfer_string(relative)
}

fn local_fs_entry_kind(metadata: &std::fs::Metadata) -> FsEntryKind {
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

fn sha256_local_path(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn transfer_verification_failure(
    target: String,
    message: impl Into<String>,
    details: serde_json::Value,
) -> AgentCommandFailure {
    AgentCommandFailure {
        code: "transfer.verification_failed".to_string(),
        message: message.into(),
        target: Some(target),
        retryable: false,
        details,
        exit_code: 1,
    }
}

fn map_operation_error(
    command: &str,
    target: Option<String>,
    err: &anyhow::Error,
    details: serde_json::Value,
) -> AgentCommandFailure {
    if let Some(remote) = remote_control_error(err) {
        return map_remote_error(
            command,
            target.unwrap_or_default(),
            remote.code.clone(),
            remote.message.clone(),
            details,
        );
    }

    if error_has_io_kind(err, std::io::ErrorKind::PermissionDenied) {
        return AgentCommandFailure {
            code: "fs.permission_denied".to_string(),
            message: err.to_string(),
            target,
            retryable: false,
            details,
            exit_code: 1,
        };
    }

    if err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| {
                io_err.kind() == std::io::ErrorKind::StorageFull
                    || io_err.raw_os_error() == Some(28)
            })
    }) {
        return AgentCommandFailure {
            code: "fs.no_space".to_string(),
            message: err.to_string(),
            target,
            retryable: false,
            details,
            exit_code: 1,
        };
    }

    let mut failure = map_transport_error(command, target, err);
    failure.details = details;
    failure
}

fn remote_control_error(err: &anyhow::Error) -> Option<&RemoteControlError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<RemoteControlError>())
}

fn require_agent_target(target: Option<&str>) -> AgentResult<String> {
    match target {
        Some(value) => normalize_target_addr(value).map_err(internal_agent_failure),
        None => Err(AgentCommandFailure {
            code: "target.required".to_string(),
            message: "agent command requires --target <addr>".to_string(),
            target: None,
            retryable: false,
            details: serde_json::json!({}),
            exit_code: 1,
        }),
    }
}

async fn agent_request(
    command: &str,
    target: &str,
    request_value: ControlRequest,
    details: serde_json::Value,
) -> AgentResult<ControlResponse> {
    let response = request(target, request_value)
        .await
        .map_err(|err| map_transport_error(command, Some(target.to_string()), &err))?;
    if let ControlResponse::Error { code, message } = response {
        return Err(map_remote_error(
            command,
            target.to_string(),
            code,
            message,
            details,
        ));
    }
    Ok(response)
}

fn cli_hash_algorithm(value: CliHashAlgorithm) -> HashAlgorithm {
    match value {
        CliHashAlgorithm::Sha256 => HashAlgorithm::Sha256,
    }
}

fn cli_content_encoding(value: CliContentEncoding) -> ContentEncoding {
    match value {
        CliContentEncoding::Utf8 => ContentEncoding::Utf8,
        CliContentEncoding::Base64 => ContentEncoding::Base64,
    }
}

fn transfer_verify_name(value: CliTransferVerifyMode) -> &'static str {
    match value {
        CliTransferVerifyMode::Size => "size",
        CliTransferVerifyMode::Sha256 => "sha256",
    }
}

fn parse_mode(value: &str) -> AgentResult<u32> {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_prefix("0o").unwrap_or(trimmed);
    let trimmed = trimmed.strip_prefix('0').unwrap_or(trimmed);
    u32::from_str_radix(if trimmed.is_empty() { "0" } else { trimmed }, 8).map_err(|_| {
        invalid_request_failure(
            format!("invalid mode `{value}`; use an octal value like 755 or 0644"),
            None,
            serde_json::json!({"field": "mode"}),
        )
    })
}

fn read_fs_write_input(input_file: Option<&str>, stdin: bool) -> AgentResult<Vec<u8>> {
    match (input_file, stdin) {
        (Some(_), true) => Err(invalid_request_failure(
            "use either --input-file or --stdin, not both",
            None,
            serde_json::json!({}),
        )),
        (None, false) => Err(invalid_request_failure(
            "fs.write requires --input-file <path> or --stdin",
            None,
            serde_json::json!({}),
        )),
        (Some(path), false) => fs::read(path).map_err(|err| AgentCommandFailure {
            code: "fs.not_found".to_string(),
            message: err.to_string(),
            target: None,
            retryable: false,
            details: serde_json::json!({"path": path}),
            exit_code: 1,
        }),
        (None, true) => {
            let mut buffer = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buffer)
                .map_err(|err| internal_agent_failure(err.into()))?;
            Ok(buffer)
        }
    }
}

fn encode_local_content(bytes: Vec<u8>, encoding: &CliContentEncoding) -> AgentResult<String> {
    match encoding {
        CliContentEncoding::Utf8 => String::from_utf8(bytes).map_err(|_| {
            invalid_request_failure(
                "input is not valid UTF-8; use --encoding base64 instead",
                None,
                serde_json::json!({}),
            )
        }),
        CliContentEncoding::Base64 => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
    }
}

fn supported_agent_operations(features: &[String], compatible: bool) -> Vec<String> {
    if !compatible {
        return Vec::new();
    }

    let mut operations = vec!["discover".to_string()];
    if has_feature(features, "agent.exec.v1") || has_feature(features, "exec.direct") {
        operations.push("exec".to_string());
        operations.push("exec.stream".to_string());
    }
    if has_feature(features, "agent.fs.stat.v1") {
        operations.push("fs.stat".to_string());
    }
    if has_feature(features, "agent.fs.list.v1") {
        operations.push("fs.list".to_string());
    }
    if has_feature(features, "agent.fs.read.v1") {
        operations.push("fs.read".to_string());
    }
    if has_feature(features, "agent.fs.write.v1") {
        operations.push("fs.write".to_string());
    }
    if has_feature(features, "agent.fs.mkdir.v1") {
        operations.push("fs.mkdir".to_string());
    }
    if has_feature(features, "agent.fs.rm.v1") {
        operations.push("fs.rm".to_string());
    }
    if has_feature(features, "agent.fs.move.v1") {
        operations.push("fs.move".to_string());
    }
    if has_feature(features, "agent.transfer.push.v1") || has_feature(features, "fs.push.batch") {
        operations.push("transfer.push".to_string());
    }
    if has_feature(features, "agent.transfer.pull.v1") || has_feature(features, "fs.pull.batch") {
        operations.push("transfer.pull".to_string());
    }
    operations
}

fn agent_schema() -> AgentSchemaData {
    AgentSchemaData {
        usage_rules: vec![
            "call discover when no target is known".to_string(),
            "use exec for direct remote process execution".to_string(),
            "use exec with --stream for long-running commands or live output".to_string(),
            "use fs.* for small structured filesystem work".to_string(),
            "use transfer.* for bulk files or directories".to_string(),
            "pass --target for every target-scoped operation".to_string(),
            "use -- before the remote command argv for exec".to_string(),
        ],
        response_envelope: "schema_version, command, ok, data?, error?".to_string(),
        error_fields: "code, message, target?, retryable, details?".to_string(),
        operations: vec![
            AgentSchemaOperation {
                name: "schema".to_string(),
                purpose: "return the static rsdb agent command contract".to_string(),
                target_required: false,
                options: Vec::new(),
                positional: Vec::new(),
                returns:
                    "usage_rules[], response_envelope, error_fields, operations[]".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "discover".to_string(),
                purpose: "find reachable targets and report per-target supported operations"
                    .to_string(),
                target_required: false,
                options: vec![
                    schema_option("probe-addr", "string", false, false),
                    schema_option("timeout-ms", "u64", false, false),
                ],
                positional: Vec::new(),
                returns: "targets[{target,server_id,device_name?,platform?,protocol_version,compatible,supported_operations[]}]".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "exec".to_string(),
                purpose: "run one direct remote process".to_string(),
                target_required: true,
                options: vec![
                    schema_option("stream", "bool", false, false),
                    schema_option("check", "bool", false, false),
                    schema_option("cwd", "string", false, false),
                    schema_option("timeout-secs", "u64", false, false),
                ],
                positional: vec![schema_positional("command", "string", true, true)],
                returns:
                    "target, command, args[], cwd?, status, timed_out, stdout, stderr, duration_ms"
                        .to_string(),
                stream_events: Some("stdout{target,chunk}; stderr{target,chunk}; completed{target,command,args[],cwd?,status,timed_out,duration_ms}; failed{error,data?}".to_string()),
            },
            AgentSchemaOperation {
                name: "fs.stat".to_string(),
                purpose: "read structured metadata for one remote path".to_string(),
                target_required: true,
                options: vec![schema_option("hash", "enum(sha256)", false, false)],
                positional: vec![schema_positional("path", "string", true, false)],
                returns: "path, exists, kind?, size?, mode?, mtime_unix_ms?, sha256?"
                    .to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "fs.list".to_string(),
                purpose: "list a remote path or directory tree".to_string(),
                target_required: true,
                options: vec![
                    schema_option("recursive", "bool", false, false),
                    schema_option("max-depth", "u32", false, false),
                    schema_option("include-hidden", "bool", false, false),
                    schema_option("hash", "enum(sha256)", false, false),
                ],
                positional: vec![schema_positional("path", "string", true, false)],
                returns: "entries[{path,kind,size,mode,mtime_unix_ms?,sha256?}]".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "fs.read".to_string(),
                purpose: "read one small remote file inline".to_string(),
                target_required: true,
                options: vec![
                    schema_option_with_default(
                        "encoding",
                        "enum(utf8|base64)",
                        "utf8",
                        false,
                        false,
                    ),
                    schema_option("max-bytes", "u64", false, false),
                ],
                positional: vec![schema_positional("path", "string", true, false)],
                returns: "path, encoding, content, bytes, truncated".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "fs.write".to_string(),
                purpose: "write one small remote file".to_string(),
                target_required: true,
                options: vec![
                    schema_option("input-file", "string", false, false),
                    schema_option("stdin", "bool", false, false),
                    schema_option_with_default(
                        "encoding",
                        "enum(utf8|base64)",
                        "utf8",
                        false,
                        false,
                    ),
                    schema_option("mode", "octal-string", false, false),
                    schema_option("create-parent", "bool", false, false),
                    schema_option("atomic", "bool", false, false),
                    schema_option("if-missing", "bool", false, false),
                    schema_option("if-sha256", "string", false, false),
                ],
                positional: vec![schema_positional("path", "string", true, false)],
                returns: "path, bytes_written, mode?, sha256?, changed".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "fs.mkdir".to_string(),
                purpose: "create a remote directory".to_string(),
                target_required: true,
                options: vec![
                    schema_option("parents", "bool", false, false),
                    schema_option("mode", "octal-string", false, false),
                ],
                positional: vec![schema_positional("path", "string", true, false)],
                returns: "path, created".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "fs.rm".to_string(),
                purpose: "remove a remote file or directory".to_string(),
                target_required: true,
                options: vec![
                    schema_option("recursive", "bool", false, false),
                    schema_option("force", "bool", false, false),
                    schema_option("if-exists", "bool", false, false),
                ],
                positional: vec![schema_positional("path", "string", true, false)],
                returns: "path, removed".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "fs.move".to_string(),
                purpose: "rename or move one remote path".to_string(),
                target_required: true,
                options: vec![schema_option("overwrite", "bool", false, false)],
                positional: vec![
                    schema_positional("source", "string", true, false),
                    schema_positional("destination", "string", true, false),
                ],
                returns: "source, destination, overwritten".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "transfer.push".to_string(),
                purpose: "push bulk local files or directories".to_string(),
                target_required: true,
                options: vec![
                    schema_option("verify", "enum(size|sha256)", false, false),
                    schema_option("atomic", "bool", false, false),
                    schema_option("if-changed", "bool", false, false),
                ],
                positional: vec![schema_positional("paths", "string", true, true)],
                returns: "target, sources[], destination, entries_written, bytes_written, skipped, verification?, verified, atomic".to_string(),
                stream_events: None,
            },
            AgentSchemaOperation {
                name: "transfer.pull".to_string(),
                purpose: "pull bulk remote files or directories".to_string(),
                target_required: true,
                options: vec![schema_option("verify", "enum(size|sha256)", false, false)],
                positional: vec![schema_positional("paths", "string", true, true)],
                returns: "target, sources[], destination, entries_received, bytes_received, total_bytes, verification?, verified".to_string(),
                stream_events: None,
            },
        ],
    }
}

fn schema_option(
    name: &'static str,
    kind: &'static str,
    required: bool,
    multiple: bool,
) -> AgentSchemaOption {
    AgentSchemaOption {
        name: name.to_string(),
        kind: kind.to_string(),
        default_value: None,
        required,
        multiple,
    }
}

fn schema_option_with_default(
    name: &'static str,
    kind: &'static str,
    default_value: &'static str,
    required: bool,
    multiple: bool,
) -> AgentSchemaOption {
    AgentSchemaOption {
        name: name.to_string(),
        kind: kind.to_string(),
        default_value: Some(default_value.to_string()),
        required,
        multiple,
    }
}

fn schema_positional(
    name: &'static str,
    kind: &'static str,
    required: bool,
    multiple: bool,
) -> AgentSchemaPositional {
    AgentSchemaPositional {
        name: name.to_string(),
        kind: kind.to_string(),
        required,
        multiple,
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn has_feature(features: &[String], expected: &str) -> bool {
    features.iter().any(|feature| feature == expected)
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn emit_agent_success<T>(command: &str, data: T) -> Result<()>
where
    T: Serialize,
{
    write_json_stdout(&AgentEnvelope {
        schema_version: "agent.v1",
        command: command.to_string(),
        ok: true,
        data: Some(data),
        error: None,
    })
}

fn emit_agent_failure(command: &str, error: AgentCommandFailure) -> Result<i32> {
    write_json_stdout(&AgentEnvelope::<serde_json::Value> {
        schema_version: "agent.v1",
        command: command.to_string(),
        ok: false,
        data: None,
        error: Some(AgentErrorBody {
            code: error.code,
            message: error.message,
            target: error.target,
            retryable: error.retryable,
            details: error.details,
        }),
    })?;
    Ok(error.exit_code)
}

fn emit_agent_event<T>(command: &'static str, event: &'static str, data: Option<T>) -> Result<()>
where
    T: Serialize,
{
    write_json_stdout(&AgentEventEnvelope {
        schema_version: "agent.v1",
        command,
        event,
        data,
        error: None,
    })
}

fn emit_agent_event_failure<T>(
    command: &'static str,
    event: &'static str,
    error: AgentCommandFailure,
    data: Option<T>,
) -> Result<()>
where
    T: Serialize,
{
    write_json_stdout(&AgentEventEnvelope {
        schema_version: "agent.v1",
        command,
        event,
        data,
        error: Some(AgentErrorBody {
            code: error.code,
            message: error.message,
            target: error.target,
            retryable: error.retryable,
            details: error.details,
        }),
    })
}

fn write_json_stdout<T>(value: &T) -> Result<()>
where
    T: Serialize,
{
    let stdout = std::io::stdout();
    let mut locked = stdout.lock();
    serde_json::to_writer(&mut locked, value)?;
    locked.write_all(b"\n")?;
    locked.flush()?;
    Ok(())
}

fn invalid_request_failure(
    message: impl Into<String>,
    target: Option<String>,
    details: serde_json::Value,
) -> AgentCommandFailure {
    AgentCommandFailure {
        code: "invalid.request".to_string(),
        message: message.into(),
        target,
        retryable: false,
        details,
        exit_code: 1,
    }
}

fn map_transport_error(
    command: &str,
    target: Option<String>,
    err: &anyhow::Error,
) -> AgentCommandFailure {
    let (code, retryable) = if let Some(remote) = remote_control_error(err) {
        return map_remote_error(
            command,
            target.unwrap_or_default(),
            remote.code.clone(),
            remote.message.clone(),
            serde_json::json!({}),
        );
    } else if error_has_elapsed(err) {
        ("connection.timeout", true)
    } else if error_has_io_kind(err, std::io::ErrorKind::ConnectionRefused) {
        ("connection.refused", true)
    } else if error_has_protocol_mismatch(err) {
        ("connection.protocol_mismatch", false)
    } else if command.starts_with("transfer")
        && error_has_io_kind(err, std::io::ErrorKind::NotFound)
    {
        ("fs.not_found", false)
    } else if error_has_io_kind(err, std::io::ErrorKind::PermissionDenied) {
        ("fs.permission_denied", false)
    } else {
        ("internal.error", false)
    };

    AgentCommandFailure {
        code: code.to_string(),
        message: err.to_string(),
        target,
        retryable,
        details: serde_json::json!({}),
        exit_code: 1,
    }
}

fn map_remote_error(
    command: &str,
    target: String,
    code: rsdb_proto::ErrorCode,
    message: String,
    details: serde_json::Value,
) -> AgentCommandFailure {
    let (mapped_code, retryable) = match code {
        rsdb_proto::ErrorCode::InvalidRequest => ("invalid.request", false),
        rsdb_proto::ErrorCode::CapabilityMissing => ("capability.missing", false),
        rsdb_proto::ErrorCode::ExecFailed if message.contains("timed out") => {
            ("exec.timeout", true)
        }
        rsdb_proto::ErrorCode::ExecFailed | rsdb_proto::ErrorCode::ExecStartFailed => {
            ("exec.start_failed", false)
        }
        rsdb_proto::ErrorCode::ExecTimeout => ("exec.timeout", true),
        rsdb_proto::ErrorCode::NotFound | rsdb_proto::ErrorCode::FsNotFound => {
            ("fs.not_found", false)
        }
        rsdb_proto::ErrorCode::FsPermissionDenied => ("fs.permission_denied", false),
        rsdb_proto::ErrorCode::FsPreconditionFailed => ("fs.precondition_failed", false),
        rsdb_proto::ErrorCode::FsNoSpace => ("fs.no_space", false),
        rsdb_proto::ErrorCode::TransferInterrupted => ("transfer.interrupted", true),
        rsdb_proto::ErrorCode::TransferVerificationFailed => {
            ("transfer.verification_failed", false)
        }
        rsdb_proto::ErrorCode::FileTransferFailed if command.starts_with("transfer") => {
            ("transfer.failed", false)
        }
        rsdb_proto::ErrorCode::Internal | rsdb_proto::ErrorCode::FileTransferFailed => {
            ("daemon.error", false)
        }
    };

    AgentCommandFailure {
        code: mapped_code.to_string(),
        message,
        target: Some(target),
        retryable,
        details,
        exit_code: 1,
    }
}

fn map_remote_exec_error(
    target: String,
    code: rsdb_proto::ErrorCode,
    message: String,
    details: serde_json::Value,
) -> AgentCommandFailure {
    map_remote_error("exec", target, code, message, details)
}

fn exec_nonzero_failure(target: String, data: &AgentExecData) -> AgentCommandFailure {
    AgentCommandFailure {
        code: "exec.nonzero".to_string(),
        message: format!("remote command exited with status {}", data.status),
        target: Some(target),
        retryable: false,
        details: serde_json::json!({
            "status": data.status,
            "cwd": data.cwd,
            "stdout": data.stdout,
            "stderr": data.stderr,
            "duration_ms": data.duration_ms,
            "timed_out": data.timed_out,
        }),
        exit_code: data.status,
    }
}

fn exec_nonzero_failure_from_completion(
    target: String,
    data: &AgentExecCompletionData,
) -> AgentCommandFailure {
    AgentCommandFailure {
        code: "exec.nonzero".to_string(),
        message: format!("remote command exited with status {}", data.status),
        target: Some(target),
        retryable: false,
        details: serde_json::json!({
            "status": data.status,
            "cwd": data.cwd,
            "duration_ms": data.duration_ms,
            "timed_out": data.timed_out,
        }),
        exit_code: data.status,
    }
}

fn internal_agent_failure(err: anyhow::Error) -> AgentCommandFailure {
    AgentCommandFailure {
        code: "internal.error".to_string(),
        message: err.to_string(),
        target: None,
        retryable: false,
        details: serde_json::json!({}),
        exit_code: 1,
    }
}

fn error_has_io_kind(err: &anyhow::Error, expected: std::io::ErrorKind) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == expected)
    })
}

fn error_has_elapsed(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    })
}

fn error_has_protocol_mismatch(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<rsdb_proto::ProtocolError>()
            .is_some_and(|protocol_err| {
                matches!(
                    protocol_err,
                    rsdb_proto::ProtocolError::UnsupportedVersion(_)
                )
            })
    })
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

async fn push_command(
    target: Option<&str>,
    source_specs: &[String],
    remote_path: &str,
) -> Result<()> {
    let addr = resolve_target(target)?;
    let summary = execute_push(&addr, source_specs, remote_path).await?;
    println!(
        "pushed {}\t{}\t{} entries\t{} bytes",
        describe_sources(source_specs),
        remote_path,
        summary.entries_written,
        summary.bytes_written
    );
    Ok(())
}

async fn pull_command(
    target: Option<&str>,
    remote_sources: &[String],
    local_destination: &str,
) -> Result<()> {
    let addr = resolve_target(target)?;
    let summary = execute_pull(&addr, remote_sources, local_destination).await?;
    let local_path = PathBuf::from(local_destination);
    println!(
        "pulled {}\t{}\t{} entries\t{} / {} bytes",
        describe_sources(remote_sources),
        local_path.display(),
        summary.entries_received,
        summary.bytes_received,
        summary.total_bytes
    );
    Ok(())
}

async fn edit_command(target: Option<&str>, editor: Option<&str>, remote_path: &str) -> Result<()> {
    let addr = resolve_target(target)?;
    let stat = remote_fs_stat(&addr, remote_path, Some(HashAlgorithm::Sha256)).await?;
    if !stat.exists {
        bail!("remote path does not exist: {remote_path}");
    }
    if !matches!(stat.kind, Some(FsEntryKind::File)) {
        bail!("remote path is not a regular file: {remote_path}");
    }

    let remote_mode = stat.mode.unwrap_or(0);
    let original_remote_sha256 = stat
        .sha256
        .clone()
        .ok_or_else(|| anyhow!("remote stat did not include a sha256 for {remote_path}"))?;
    let file_name = remote_edit_file_name(remote_path);
    let temp_dir = create_edit_temp_dir()?;
    let local_path = temp_dir.join(file_name);
    let inline_edit = stat.size.unwrap_or(0) <= DEFAULT_INLINE_EDIT_MAX_BYTES;

    if inline_edit {
        let result = remote_fs_read(&addr, remote_path, ContentEncoding::Base64, None).await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(result.content)
            .context("failed to decode inline edit content")?;
        if result.truncated {
            bail!("remote file exceeded inline edit limit: {remote_path}");
        }
        write_local_edit_file(&local_path, &bytes, remote_mode).await?;
    } else {
        execute_pull(
            &addr,
            &[remote_path.to_string()],
            local_path.to_string_lossy().as_ref(),
        )
        .await?;
    }

    let original_local_sha256 = sha256_local_path(&local_path)?;
    let editor_command = resolve_editor_command(editor)?;
    let editor_status = launch_editor(&editor_command, &local_path)?;
    if !editor_status.success() {
        bail!(
            "editor exited with status {}; kept local copy at {}",
            editor_status.code().unwrap_or(-1),
            local_path.display()
        );
    }

    let edited_local_sha256 = sha256_local_path(&local_path)?;
    if edited_local_sha256 == original_local_sha256 {
        remove_edit_temp_dir(&temp_dir)?;
        println!("no changes\t{}\t{}", display_addr(&addr), remote_path);
        return Ok(());
    }

    let write_result = if inline_edit {
        let bytes = fs::read(&local_path)
            .with_context(|| format!("failed to read edited file {}", local_path.display()))?;
        let content = base64::engine::general_purpose::STANDARD.encode(bytes);
        remote_fs_write(
            &addr,
            remote_path,
            content,
            ContentEncoding::Base64,
            Some(remote_mode),
            true,
            Some(original_remote_sha256.clone()),
        )
        .await
        .map(|_| ())
    } else {
        let latest_stat = remote_fs_stat(&addr, remote_path, Some(HashAlgorithm::Sha256)).await?;
        let latest_sha256 = latest_stat.sha256.ok_or_else(|| {
            anyhow!("remote stat did not include a sha256 for {remote_path} during save")
        })?;
        if latest_sha256 != original_remote_sha256 {
            bail!(
                "remote file changed while editing {}; kept local copy at {}",
                remote_path,
                local_path.display()
            );
        }
        execute_push(
            &addr,
            &[local_path.to_string_lossy().into_owned()],
            remote_path,
        )
        .await
        .map(|_| ())
    };

    if let Err(err) = write_result {
        bail!("{err}; kept local copy at {}", local_path.display());
    }

    remove_edit_temp_dir(&temp_dir)?;
    println!("updated\t{}\t{}", display_addr(&addr), remote_path);
    Ok(())
}

async fn complete_edit_path_command(target: Option<&str>, partial: &str) -> Result<()> {
    let addr = resolve_target(target)?;
    for candidate in complete_remote_edit_path(&addr, partial).await? {
        println!("{candidate}");
    }
    Ok(())
}

async fn execute_push(
    addr: &str,
    source_specs: &[String],
    remote_path: &str,
) -> Result<PushSummary> {
    let manifest = build_local_batch_manifest(source_specs)?;
    execute_push_manifest(addr, &manifest, remote_path).await
}

async fn execute_push_manifest(
    addr: &str,
    manifest: &LocalBatchManifest,
    remote_path: &str,
) -> Result<PushSummary> {
    let mut stream = open_connection(&addr).await?;
    write_json_frame(
        &mut stream,
        FrameKind::Request,
        REQUEST_ID,
        &ControlRequest::PushBatch {
            destination: remote_path.to_string(),
            roots: manifest.roots.clone(),
            entries: manifest.entries.clone(),
        },
    )
    .await
    .context("failed to send push request")?;

    match read_response(&mut stream).await? {
        ControlResponse::PushBatchReady => {}
        ControlResponse::Error { code, message } => {
            return Err(RemoteControlError { code, message }.into());
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }

    let bytes_sent = stream_local_batch_files(&mut stream, &manifest.files).await?;
    if bytes_sent != manifest.total_bytes {
        bail!(
            "push size mismatch before completion: expected {} bytes, streamed {bytes_sent}",
            manifest.total_bytes
        );
    }
    write_stream_frame(&mut stream, REQUEST_ID, StreamChannel::File, true, &[])
        .await
        .context("failed to finish push stream")?;

    match read_response(&mut stream).await? {
        ControlResponse::PushBatchComplete {
            entries_written,
            bytes_written,
        } => {
            if bytes_written != bytes_sent {
                bail!("push size mismatch: daemon wrote {bytes_written} bytes, sent {bytes_sent}");
            }
            Ok(PushSummary {
                entries_written,
                bytes_written,
                skipped: false,
                verification: None,
                verified: false,
            })
        }
        ControlResponse::Error { code, message } => {
            Err(RemoteControlError { code, message }.into())
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn execute_pull(
    addr: &str,
    remote_sources: &[String],
    local_destination: &str,
) -> Result<PullSummary> {
    let mut stream = open_connection(&addr).await?;
    write_json_frame(
        &mut stream,
        FrameKind::Request,
        REQUEST_ID,
        &ControlRequest::PullBatch {
            sources: remote_sources.to_vec(),
        },
    )
    .await
    .context("failed to send pull request")?;

    let (roots, entries, total_bytes) = match read_response(&mut stream).await? {
        ControlResponse::PullBatchMetadata {
            roots,
            entries,
            total_bytes,
        } => (roots, entries, total_bytes),
        ControlResponse::Error { code, message } => {
            return Err(RemoteControlError { code, message }.into());
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    };

    let local_path = PathBuf::from(local_destination);
    let (entries_received, bytes_received) = receive_local_batch(
        &mut stream,
        &local_path,
        local_destination,
        &roots,
        &entries,
    )
    .await?;

    match read_response(&mut stream).await? {
        ControlResponse::PullBatchComplete {
            entries_sent,
            bytes_sent,
        } => {
            if entries_sent != entries_received {
                bail!(
                    "pull entry mismatch: daemon sent {entries_sent} entries, received {entries_received}"
                );
            }
            if bytes_sent != bytes_received {
                bail!(
                    "pull size mismatch: daemon sent {bytes_sent} bytes, received {bytes_received}"
                );
            }
            Ok(PullSummary {
                entries_received,
                bytes_received,
                total_bytes,
                verification: None,
                verified: false,
                roots,
                entries,
            })
        }
        ControlResponse::Error { code, message } => {
            Err(RemoteControlError { code, message }.into())
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn remote_fs_stat(
    addr: &str,
    path: &str,
    hash: Option<HashAlgorithm>,
) -> Result<AgentFsStat> {
    match request(
        addr,
        ControlRequest::FsStat {
            path: path.to_string(),
            hash,
        },
    )
    .await?
    {
        ControlResponse::FsStat { stat } => Ok(stat),
        ControlResponse::Error { code, message } => {
            Err(RemoteControlError { code, message }.into())
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn remote_fs_read(
    addr: &str,
    path: &str,
    encoding: ContentEncoding,
    max_bytes: Option<u64>,
) -> Result<AgentFsReadResult> {
    match request(
        addr,
        ControlRequest::FsRead {
            path: path.to_string(),
            encoding,
            max_bytes,
        },
    )
    .await?
    {
        ControlResponse::FsReadResult { result } => Ok(result),
        ControlResponse::Error { code, message } => {
            Err(RemoteControlError { code, message }.into())
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn remote_fs_write(
    addr: &str,
    path: &str,
    content: String,
    encoding: ContentEncoding,
    mode: Option<u32>,
    atomic: bool,
    if_sha256: Option<String>,
) -> Result<AgentFsWriteResult> {
    match request(
        addr,
        ControlRequest::FsWrite {
            path: path.to_string(),
            content,
            encoding,
            mode,
            create_parent: false,
            atomic,
            if_missing: false,
            if_sha256,
        },
    )
    .await?
    {
        ControlResponse::FsWriteResult { result } => Ok(result),
        ControlResponse::Error { code, message } => {
            Err(RemoteControlError { code, message }.into())
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn remote_fs_list_completion(addr: &str, path: &str) -> Result<Vec<AgentFsEntry>> {
    match request(
        addr,
        ControlRequest::FsList {
            path: path.to_string(),
            recursive: false,
            max_depth: None,
            include_hidden: true,
            hash: None,
        },
    )
    .await?
    {
        ControlResponse::FsList { entries } => Ok(entries),
        ControlResponse::Error {
            code: rsdb_proto::ErrorCode::FsNotFound | rsdb_proto::ErrorCode::NotFound,
            ..
        } => Ok(Vec::new()),
        ControlResponse::Error { code, message } => {
            Err(RemoteControlError { code, message }.into())
        }
        other => bail!("unexpected response from daemon: {other:?}"),
    }
}

fn split_sources_and_destination(paths: &[String], command: &str) -> Result<(Vec<String>, String)> {
    let (destination, sources) = paths.split_last().ok_or_else(|| {
        anyhow!("`rsdb {command}` requires at least one source and one destination")
    })?;
    if sources.is_empty() {
        bail!("`rsdb {command}` requires at least one source and one destination");
    }
    Ok((sources.to_vec(), destination.clone()))
}

fn describe_sources(sources: &[String]) -> String {
    match sources {
        [source] => source.clone(),
        _ => format!("{} sources", sources.len()),
    }
}

fn build_local_batch_manifest(source_specs: &[String]) -> Result<LocalBatchManifest> {
    let sources = expand_local_sources(source_specs)?;
    let mut roots = Vec::with_capacity(sources.len());
    let mut entries = Vec::new();
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;

    for source in &sources {
        let metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("failed to stat local path {}", source.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "symbolic links are not supported in batch push: {}",
                source.display()
            );
        }

        let kind = if metadata.is_dir() {
            TransferEntryKind::Directory
        } else if metadata.is_file() {
            TransferEntryKind::File
        } else {
            bail!("unsupported local path type: {}", source.display());
        };

        let root_index = roots.len() as u32;
        roots.push(TransferRoot {
            source_name: transfer_source_name(&source)?,
            kind: kind.clone(),
            mode: local_file_mode(&metadata),
        });

        append_local_manifest_entry(
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
    Ok(LocalBatchManifest {
        sources,
        roots,
        entries,
        files,
        total_bytes,
    })
}

fn expand_local_sources(source_specs: &[String]) -> Result<Vec<PathBuf>> {
    let mut sources = Vec::new();
    for spec in source_specs {
        let literal_path = PathBuf::from(spec);
        if has_glob_pattern(spec) && !literal_path.exists() {
            let mut matches = Vec::new();
            for entry in glob_with(spec, glob_match_options())
                .with_context(|| format!("invalid local glob pattern: {spec}"))?
            {
                matches.push(
                    entry.with_context(|| format!("failed to resolve local match for {spec}"))?,
                );
            }
            matches.sort();
            if matches.is_empty() {
                bail!("local pattern matched no paths: {spec}");
            }
            sources.extend(matches);
            continue;
        }

        if !literal_path.exists() {
            bail!("local path does not exist: {}", literal_path.display());
        }
        sources.push(literal_path);
    }
    Ok(sources)
}

fn append_local_manifest_entry(
    root_index: u32,
    path: &Path,
    relative_path: &Path,
    metadata: &std::fs::Metadata,
    entries: &mut Vec<TransferEntry>,
    files: &mut Vec<LocalBatchFile>,
    total_bytes: &mut u64,
) -> Result<()> {
    if metadata.file_type().is_symlink() {
        bail!(
            "symbolic links are not supported in batch push: {}",
            path.display()
        );
    }

    if metadata.is_dir() {
        entries.push(TransferEntry {
            root_index,
            relative_path: path_to_transfer_string(relative_path)?,
            kind: TransferEntryKind::Directory,
            mode: local_file_mode(metadata),
            size: 0,
        });

        let mut children = fs::read_dir(path)
            .with_context(|| format!("failed to read local directory {}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("failed to read local directory {}", path.display()))?;
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
            let child_metadata = fs::symlink_metadata(&child)
                .with_context(|| format!("failed to stat local path {}", child.display()))?;
            append_local_manifest_entry(
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
        bail!("unsupported local path type: {}", path.display());
    }

    let size = metadata.len();
    entries.push(TransferEntry {
        root_index,
        relative_path: path_to_transfer_string(relative_path)?,
        kind: TransferEntryKind::File,
        mode: local_file_mode(metadata),
        size,
    });
    files.push(LocalBatchFile {
        root_index,
        path: path.to_path_buf(),
        size,
    });
    *total_bytes += size;
    Ok(())
}

fn filter_local_batch_manifest(
    manifest: &LocalBatchManifest,
    root_indexes: &[u32],
) -> Result<LocalBatchManifest> {
    let mut remapped_indexes = BTreeMap::new();
    let mut sources = Vec::with_capacity(root_indexes.len());
    let mut roots = Vec::with_capacity(root_indexes.len());

    for (new_index, root_index) in root_indexes.iter().copied().enumerate() {
        let root_usize = root_index as usize;
        let source = manifest
            .sources
            .get(root_usize)
            .ok_or_else(|| anyhow!("invalid local batch manifest root index {root_index}"))?;
        let root = manifest
            .roots
            .get(root_usize)
            .ok_or_else(|| anyhow!("invalid local batch manifest root index {root_index}"))?;
        remapped_indexes.insert(root_index, new_index as u32);
        sources.push(source.clone());
        roots.push(root.clone());
    }

    let entries = manifest
        .entries
        .iter()
        .filter_map(|entry| {
            remapped_indexes
                .get(&entry.root_index)
                .map(|&mapped_root_index| {
                    let mut entry = entry.clone();
                    entry.root_index = mapped_root_index;
                    entry
                })
        })
        .collect::<Vec<_>>();
    let files = manifest
        .files
        .iter()
        .filter_map(|file| {
            remapped_indexes
                .get(&file.root_index)
                .map(|&mapped_root_index| {
                    let mut file = file.clone();
                    file.root_index = mapped_root_index;
                    file
                })
        })
        .collect::<Vec<_>>();
    let total_bytes = files.iter().map(|file| file.size).sum();

    Ok(LocalBatchManifest {
        sources,
        roots,
        entries,
        files,
        total_bytes,
    })
}

async fn stream_local_batch_files(stream: &mut TcpStream, files: &[LocalBatchFile]) -> Result<u64> {
    let mut writer =
        tokio::io::BufWriter::with_capacity(HEADER_LEN + DEFAULT_STREAM_CHUNK_SIZE, &mut *stream);
    let mut buffer = vec![0_u8; DEFAULT_STREAM_CHUNK_SIZE];
    let mut total_bytes = 0_u64;

    for file_entry in files {
        let mut file = File::open(&file_entry.path)
            .await
            .with_context(|| format!("failed to open local file {}", file_entry.path.display()))?;
        let mut file_bytes = 0_u64;
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            write_stream_frame(
                &mut writer,
                REQUEST_ID,
                StreamChannel::File,
                false,
                &buffer[..read],
            )
            .await
            .context("failed to stream file data")?;
            total_bytes += read as u64;
            file_bytes += read as u64;
        }
        if file_bytes != file_entry.size {
            bail!(
                "local file changed during push: expected {} bytes, read {} from {}",
                file_entry.size,
                file_bytes,
                file_entry.path.display()
            );
        }
    }

    writer
        .flush()
        .await
        .context("failed to flush push stream")?;
    Ok(total_bytes)
}

async fn receive_local_batch(
    stream: &mut TcpStream,
    destination: &Path,
    destination_arg: &str,
    roots: &[TransferRoot],
    entries: &[TransferEntry],
) -> Result<(u64, u64)> {
    let root_paths = resolve_local_batch_roots(destination, destination_arg, roots)?;
    let mut entries_received = 0_u64;
    let mut bytes_received = 0_u64;
    let mut chunk_payload = Vec::with_capacity(DEFAULT_STREAM_CHUNK_SIZE);

    for entry in entries {
        let output_path = resolve_transfer_entry_path(&root_paths, entry)?;
        match entry.kind {
            TransferEntryKind::Directory => {
                tokio::fs::create_dir_all(&output_path)
                    .await
                    .with_context(|| {
                        format!("failed to create local directory {}", output_path.display())
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
                let mut file = create_local_output_file(&output_path, entry.mode).await?;
                let mut remaining = entry.size;
                while remaining > 0 {
                    let chunk =
                        read_file_stream_chunk_into(stream, "pull", &mut chunk_payload).await?;
                    if chunk.eof {
                        bail!(
                            "unexpected end of pull stream while writing {}",
                            output_path.display()
                        );
                    }
                    if chunk_payload.len() as u64 > remaining {
                        bail!(
                            "pull stream overflow while writing {}",
                            output_path.display()
                        );
                    }
                    if !chunk_payload.is_empty() {
                        file.write_all(&chunk_payload).await.with_context(|| {
                            format!("failed to write {}", output_path.display())
                        })?;
                        remaining -= chunk_payload.len() as u64;
                        bytes_received += chunk_payload.len() as u64;
                    }
                }
                file.flush().await?;
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
        }
        entries_received += 1;
    }

    expect_file_stream_eof(stream, "pull", &mut chunk_payload).await?;
    Ok((entries_received, bytes_received))
}

async fn read_file_stream_chunk_into(
    stream: &mut TcpStream,
    context_name: &str,
    payload: &mut Vec<u8>,
) -> Result<rsdb_proto::StreamFrameHeader> {
    let chunk = read_stream_frame_into(stream, payload)
        .await
        .with_context(|| format!("failed to read {context_name} frame"))?;
    ensure_request_id(chunk.request_id, REQUEST_ID)?;
    if chunk.channel != StreamChannel::File {
        bail!(
            "unexpected stream channel during {context_name}: {:?}",
            chunk.channel
        );
    }
    Ok(chunk)
}

async fn expect_file_stream_eof(
    stream: &mut TcpStream,
    context_name: &str,
    payload: &mut Vec<u8>,
) -> Result<()> {
    let chunk = read_file_stream_chunk_into(stream, context_name, payload).await?;
    if !chunk.eof || !payload.is_empty() {
        bail!("expected end of {context_name} file stream");
    }
    Ok(())
}

fn resolve_local_batch_roots(
    destination: &Path,
    destination_arg: &str,
    roots: &[TransferRoot],
) -> Result<Vec<PathBuf>> {
    if roots.is_empty() {
        bail!("transfer manifest did not include any roots");
    }
    ensure_unique_root_names(roots)?;

    let destination_metadata = match fs::metadata(destination) {
        Ok(metadata) => Some(metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to stat local destination {}", destination.display())
            });
        }
    };

    if roots.len() == 1 {
        let treat_as_directory = destination_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.is_dir())
            || destination_arg.ends_with('/')
            || destination_arg.ends_with(std::path::MAIN_SEPARATOR);
        let root = &roots[0];
        let base = if treat_as_directory {
            destination.join(&root.source_name)
        } else {
            destination.to_path_buf()
        };
        return Ok(vec![base]);
    }

    if destination_metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_dir())
    {
        bail!(
            "local destination is not a directory: {}",
            destination.display()
        );
    }

    Ok(roots
        .iter()
        .map(|root| destination.join(&root.source_name))
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
    let mut seen = BTreeMap::new();
    for root in roots {
        if let Some(previous) = seen.insert(root.source_name.clone(), root.kind.clone()) {
            bail!(
                "duplicate transfer root name `{}` is not supported ({previous:?} and {:?})",
                root.source_name,
                root.kind
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
    let (mut reader, writer) = stream.into_split();
    let mut writer = tokio::io::BufWriter::new(writer);
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
        writer
            .flush()
            .await
            .context("failed to flush remote stdin close")?;
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
                        writer
                            .flush()
                            .await
                            .context("failed to flush forwarded stdin")?;
                    }
                    Some(StdinChunk::Eof) | None => {
                        trace!("forwarding shell stdin eof");
                        write_stream_frame(&mut writer, REQUEST_ID, StreamChannel::Stdin, true, &[])
                            .await
                            .context("failed to finish stdin stream")?;
                        writer
                            .flush()
                            .await
                            .context("failed to flush stdin eof")?;
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
                        let chunk = decode_stream_frame(frame).context("invalid stream frame")?;
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

async fn complete_remote_edit_path(addr: &str, partial: &str) -> Result<Vec<String>> {
    let (parent, prefix) = split_remote_completion_input(partial);
    let entries = remote_fs_list_completion(addr, &parent).await?;
    let mut results = entries
        .into_iter()
        .filter_map(|entry| {
            let name = Path::new(&entry.path)
                .file_name()
                .and_then(|value| value.to_str())?;
            if !name.starts_with(&prefix) {
                return None;
            }
            let mut candidate = join_remote_completion_candidate(&parent, name);
            if matches!(entry.kind, FsEntryKind::Directory) {
                candidate.push('/');
            }
            Some(candidate)
        })
        .collect::<Vec<_>>();
    results.sort();
    results.dedup();
    Ok(results)
}

fn split_remote_completion_input(partial: &str) -> (String, String) {
    if partial.is_empty() {
        return (".".to_string(), String::new());
    }
    if partial == "/" {
        return ("/".to_string(), String::new());
    }
    if partial.ends_with('/') {
        return (trim_trailing_slash_preserving_root(partial), String::new());
    }
    if let Some((parent, prefix)) = partial.rsplit_once('/') {
        let parent = if parent.is_empty() { "/" } else { parent };
        return (parent.to_string(), prefix.to_string());
    }
    (".".to_string(), partial.to_string())
}

fn trim_trailing_slash_preserving_root(value: &str) -> String {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn join_remote_completion_candidate(parent: &str, name: &str) -> String {
    match parent {
        "." => name.to_string(),
        "/" => format!("/{name}"),
        _ => format!("{parent}/{name}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorCommand {
    program: String,
    args: Vec<String>,
}

fn resolve_editor_command(override_value: Option<&str>) -> Result<EditorCommand> {
    let raw = override_value
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| env::var("RSDB_EDITOR").ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| env::var("VISUAL").ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| env::var("EDITOR").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string());

    let mut parts = raw
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let program = parts
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("editor command must not be empty"))?;
    let mut args = parts.split_off(1);
    if editor_needs_wait_flag(&program) && !args.iter().any(|arg| arg == "--wait") {
        args.push("--wait".to_string());
    }
    Ok(EditorCommand { program, args })
}

fn editor_needs_wait_flag(program: &str) -> bool {
    matches!(
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program),
        "code" | "code-insiders" | "codium"
    )
}

fn launch_editor(editor: &EditorCommand, path: &Path) -> Result<std::process::ExitStatus> {
    std::process::Command::new(&editor.program)
        .args(&editor.args)
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch editor `{}`", editor.program))
}

fn create_edit_temp_dir() -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let path = env::temp_dir().join(format!("rsdb-edit-{nonce}"));
    fs::create_dir_all(&path)
        .with_context(|| format!("failed to create temp dir {}", path.display()))?;
    Ok(path)
}

fn remove_edit_temp_dir(path: &Path) -> Result<()> {
    fs::remove_dir_all(path)
        .with_context(|| format!("failed to remove temp dir {}", path.display()))
}

fn remote_edit_file_name(remote_path: &str) -> String {
    Path::new(remote_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("rsdb-edit")
        .to_string()
}

async fn write_local_edit_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = create_local_output_file(path, mode).await?;
    file.write_all(bytes)
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.flush()
        .await
        .with_context(|| format!("failed to flush {}", path.display()))?;
    Ok(())
}

async fn open_connection(addr: &str) -> Result<TcpStream> {
    debug!(target_addr = %addr, "opening tcp connection");
    let stream = timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .context("connection timed out")?
        .with_context(|| format!("failed to connect to {addr}"))?;
    stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY")?;
    Ok(stream)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_agent_operations_require_compatibility() {
        let operations = supported_agent_operations(
            &[
                "agent.exec.v1".to_string(),
                "agent.fs.read.v1".to_string(),
                "agent.transfer.push.v1".to_string(),
                "agent.transfer.pull.v1".to_string(),
            ],
            false,
        );

        assert!(operations.is_empty());
    }

    #[test]
    fn supported_agent_operations_follow_feature_advertisement() {
        let operations = supported_agent_operations(
            &[
                "agent.exec.v1".to_string(),
                "agent.fs.stat.v1".to_string(),
                "agent.fs.read.v1".to_string(),
                "agent.transfer.push.v1".to_string(),
            ],
            true,
        );

        assert_eq!(
            operations,
            vec![
                "discover".to_string(),
                "exec".to_string(),
                "exec.stream".to_string(),
                "fs.stat".to_string(),
                "fs.read".to_string(),
                "transfer.push".to_string(),
            ]
        );
    }

    #[test]
    fn supported_agent_operations_cover_full_modern_surface() {
        let operations = supported_agent_operations(
            &[
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
            ],
            true,
        );

        assert_eq!(
            operations,
            vec![
                "discover".to_string(),
                "exec".to_string(),
                "exec.stream".to_string(),
                "fs.stat".to_string(),
                "fs.list".to_string(),
                "fs.read".to_string(),
                "fs.write".to_string(),
                "fs.mkdir".to_string(),
                "fs.rm".to_string(),
                "fs.move".to_string(),
                "transfer.push".to_string(),
                "transfer.pull".to_string(),
            ]
        );
    }

    #[test]
    fn supported_agent_operations_accept_legacy_transfer_fallbacks() {
        let operations = supported_agent_operations(
            &[
                "exec.direct".to_string(),
                "fs.push.batch".to_string(),
                "fs.pull.batch".to_string(),
            ],
            true,
        );

        assert_eq!(
            operations,
            vec![
                "discover".to_string(),
                "exec".to_string(),
                "exec.stream".to_string(),
                "transfer.push".to_string(),
                "transfer.pull".to_string(),
            ]
        );
    }

    #[test]
    fn require_agent_target_rejects_missing_target() {
        let err = require_agent_target(None).expect_err("missing target should fail");
        assert_eq!(err.code, "target.required");
    }

    #[test]
    fn agent_schema_lists_core_operations() {
        let schema = agent_schema();
        let names = schema
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"schema"));
        assert!(names.contains(&"discover"));
        assert!(names.contains(&"exec"));
        assert!(names.contains(&"fs.list"));
        assert!(names.contains(&"fs.read"));
        assert!(names.contains(&"fs.write"));
        assert!(names.contains(&"fs.mkdir"));
        assert!(names.contains(&"fs.rm"));
        assert!(names.contains(&"fs.move"));
        assert!(names.contains(&"transfer.push"));
        assert!(names.contains(&"transfer.pull"));
    }

    #[test]
    fn agent_schema_marks_target_scoped_operations() {
        let schema = agent_schema();
        let by_name = schema
            .operations
            .iter()
            .map(|operation| (operation.name.as_str(), operation.target_required))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(by_name.get("exec"), Some(&true));
        assert_eq!(by_name.get("fs.write"), Some(&true));
        assert_eq!(by_name.get("transfer.pull"), Some(&true));
        assert_eq!(by_name.get("schema"), Some(&false));
        assert_eq!(by_name.get("discover"), Some(&false));
    }

    #[test]
    fn agent_schema_exec_declares_stream_and_check_options() {
        let schema = agent_schema();
        let exec = schema
            .operations
            .iter()
            .find(|operation| operation.name == "exec")
            .expect("exec operation should exist");

        let option_names = exec
            .options
            .iter()
            .map(|option| option.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(option_names, vec!["stream", "check", "cwd", "timeout-secs"]);
        assert!(exec.target_required);
        assert_eq!(exec.positional.len(), 1);
        assert_eq!(exec.positional[0].name, "command");
        assert!(exec.positional[0].multiple);
        assert_eq!(
            exec.returns,
            "target, command, args[], cwd?, status, timed_out, stdout, stderr, duration_ms"
        );
        assert_eq!(
            exec.stream_events.as_deref(),
            Some(
                "stdout{target,chunk}; stderr{target,chunk}; completed{target,command,args[],cwd?,status,timed_out,duration_ms}; failed{error,data?}"
            )
        );
    }

    #[test]
    fn agent_schema_lists_all_fs_operations_as_target_scoped() {
        let schema = agent_schema();
        let fs_operations = schema
            .operations
            .iter()
            .filter(|operation| operation.name.starts_with("fs."))
            .collect::<Vec<_>>();

        assert_eq!(fs_operations.len(), 7);
        assert!(
            fs_operations
                .iter()
                .all(|operation| operation.target_required)
        );
    }

    #[test]
    fn agent_schema_includes_shared_response_contract() {
        let schema = agent_schema();

        assert_eq!(
            schema.response_envelope,
            "schema_version, command, ok, data?, error?"
        );
        assert_eq!(
            schema.error_fields,
            "code, message, target?, retryable, details?"
        );
    }

    #[test]
    fn agent_schema_summarizes_operation_outputs() {
        let schema = agent_schema();
        let by_name = schema
            .operations
            .iter()
            .map(|operation| (operation.name.as_str(), operation.returns.as_str()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            by_name.get("schema"),
            Some(&"usage_rules[], response_envelope, error_fields, operations[]")
        );
        assert_eq!(
            by_name.get("discover"),
            Some(
                &"targets[{target,server_id,device_name?,platform?,protocol_version,compatible,supported_operations[]}]"
            )
        );
        assert_eq!(
            by_name.get("fs.read"),
            Some(&"path, encoding, content, bytes, truncated")
        );
        assert_eq!(
            by_name.get("transfer.push"),
            Some(
                &"target, sources[], destination, entries_written, bytes_written, skipped, verification?, verified, atomic"
            )
        );
    }

    #[test]
    fn agent_schema_omits_discover_port_and_marks_encoding_defaults() {
        let schema = agent_schema();
        let discover = schema
            .operations
            .iter()
            .find(|operation| operation.name == "discover")
            .expect("discover operation should exist");
        let read = schema
            .operations
            .iter()
            .find(|operation| operation.name == "fs.read")
            .expect("fs.read operation should exist");
        let write = schema
            .operations
            .iter()
            .find(|operation| operation.name == "fs.write")
            .expect("fs.write operation should exist");

        let discover_option_names = discover
            .options
            .iter()
            .map(|option| option.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(discover_option_names, vec!["probe-addr", "timeout-ms"]);

        let read_encoding = read
            .options
            .iter()
            .find(|option| option.name == "encoding")
            .expect("fs.read encoding option should exist");
        assert_eq!(read_encoding.default_value.as_deref(), Some("utf8"));

        let write_encoding = write
            .options
            .iter()
            .find(|option| option.name == "encoding")
            .expect("fs.write encoding option should exist");
        assert_eq!(write_encoding.default_value.as_deref(), Some("utf8"));
    }

    #[test]
    fn transfer_state_matching_ignores_unmanaged_extra_entries() {
        let expected = BTreeMap::from([
            (
                String::new(),
                TransferPathState {
                    kind: FsEntryKind::Directory,
                    size: None,
                    sha256: None,
                },
            ),
            (
                "app.txt".to_string(),
                TransferPathState {
                    kind: FsEntryKind::File,
                    size: Some(4),
                    sha256: Some("deadbeef".to_string()),
                },
            ),
        ]);
        let actual = BTreeMap::from([
            (
                String::new(),
                TransferPathState {
                    kind: FsEntryKind::Directory,
                    size: None,
                    sha256: None,
                },
            ),
            (
                "app.txt".to_string(),
                TransferPathState {
                    kind: FsEntryKind::File,
                    size: Some(4),
                    sha256: Some("deadbeef".to_string()),
                },
            ),
            (
                "logs/debug.txt".to_string(),
                TransferPathState {
                    kind: FsEntryKind::File,
                    size: Some(8),
                    sha256: Some("cafebabe".to_string()),
                },
            ),
        ]);

        assert!(transfer_states_match(&expected, &actual));
    }

    #[test]
    fn filter_local_batch_manifest_remaps_roots_and_files() {
        let manifest = LocalBatchManifest {
            sources: vec![PathBuf::from("alpha"), PathBuf::from("beta")],
            roots: vec![
                TransferRoot {
                    source_name: "alpha".to_string(),
                    kind: TransferEntryKind::Directory,
                    mode: 0,
                },
                TransferRoot {
                    source_name: "beta".to_string(),
                    kind: TransferEntryKind::File,
                    mode: 0,
                },
            ],
            entries: vec![
                TransferEntry {
                    root_index: 0,
                    relative_path: String::new(),
                    kind: TransferEntryKind::Directory,
                    mode: 0,
                    size: 0,
                },
                TransferEntry {
                    root_index: 0,
                    relative_path: "nested.txt".to_string(),
                    kind: TransferEntryKind::File,
                    mode: 0,
                    size: 3,
                },
                TransferEntry {
                    root_index: 1,
                    relative_path: String::new(),
                    kind: TransferEntryKind::File,
                    mode: 0,
                    size: 7,
                },
            ],
            files: vec![
                LocalBatchFile {
                    root_index: 0,
                    path: PathBuf::from("alpha/nested.txt"),
                    size: 3,
                },
                LocalBatchFile {
                    root_index: 1,
                    path: PathBuf::from("beta"),
                    size: 7,
                },
            ],
            total_bytes: 10,
        };

        let filtered =
            filter_local_batch_manifest(&manifest, &[1]).expect("root filter should succeed");

        assert_eq!(filtered.sources, vec![PathBuf::from("beta")]);
        assert_eq!(filtered.roots.len(), 1);
        assert_eq!(filtered.roots[0].source_name, "beta");
        assert_eq!(filtered.entries.len(), 1);
        assert_eq!(filtered.entries[0].root_index, 0);
        assert_eq!(filtered.files.len(), 1);
        assert_eq!(filtered.files[0].root_index, 0);
        assert_eq!(filtered.total_bytes, 7);
    }

    #[test]
    fn split_remote_completion_input_handles_empty_root_and_relative_values() {
        assert_eq!(
            split_remote_completion_input(""),
            (".".to_string(), String::new())
        );
        assert_eq!(
            split_remote_completion_input("/"),
            ("/".to_string(), String::new())
        );
        assert_eq!(
            split_remote_completion_input("etc"),
            (".".to_string(), "etc".to_string())
        );
    }

    #[test]
    fn split_remote_completion_input_handles_parent_and_directory_prefixes() {
        assert_eq!(
            split_remote_completion_input("/tmp/rs"),
            ("/tmp".to_string(), "rs".to_string())
        );
        assert_eq!(
            split_remote_completion_input("/tmp/work/"),
            ("/tmp/work".to_string(), String::new())
        );
    }

    #[test]
    fn join_remote_completion_candidate_handles_relative_and_root_paths() {
        assert_eq!(join_remote_completion_candidate(".", "app"), "app");
        assert_eq!(join_remote_completion_candidate("/", "app"), "/app");
        assert_eq!(join_remote_completion_candidate("/tmp", "app"), "/tmp/app");
    }

    #[test]
    fn resolve_editor_command_adds_wait_for_code() {
        let editor = resolve_editor_command(Some("code")).expect("editor should resolve");
        assert_eq!(editor.program, "code");
        assert_eq!(editor.args, vec!["--wait"]);
    }

    #[test]
    fn resolve_editor_command_preserves_existing_wait_flag() {
        let editor = resolve_editor_command(Some("code --wait --reuse-window"))
            .expect("editor should resolve");
        assert_eq!(editor.program, "code");
        assert_eq!(editor.args, vec!["--wait", "--reuse-window"]);
    }
}
