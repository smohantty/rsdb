use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAGIC: [u8; 4] = *b"RSDB";
pub const DISCOVERY_MAGIC: [u8; 8] = *b"RSDBDISC";
pub const PROTOCOL_VERSION: u16 = 2;
pub const HEADER_LEN: usize = 24;
pub const MAX_PAYLOAD_LEN: usize = 16 * 1024 * 1024;
pub const MAX_DISCOVERY_PAYLOAD_LEN: usize = 16 * 1024;
pub const STREAM_EOF_FLAG: u32 = 1 << 31;
pub const STREAM_CHANNEL_MASK: u32 = 0xff;
pub const DEFAULT_STREAM_CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum FrameKind {
    Request = 1,
    Response = 2,
    Event = 3,
    Stream = 4,
}

impl FrameKind {
    fn from_u16(value: u16) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::Request),
            2 => Ok(Self::Response),
            3 => Ok(Self::Event),
            4 => Ok(Self::Stream),
            other => Err(ProtocolError::UnknownFrameKind(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum StreamChannel {
    File = 1,
    Stdin = 2,
    Stdout = 3,
    Stderr = 4,
}

impl StreamChannel {
    fn from_u32(value: u32) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::File),
            2 => Ok(Self::Stdin),
            3 => Ok(Self::Stdout),
            4 => Ok(Self::Stderr),
            other => Err(ProtocolError::UnknownStreamChannel(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u16,
    pub kind: FrameKind,
    pub flags: u32,
    pub request_id: u32,
    pub payload_len: u32,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0_u8; HEADER_LEN];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[4..6].copy_from_slice(&self.version.to_le_bytes());
        bytes[6..8].copy_from_slice(&(self.kind as u16).to_le_bytes());
        bytes[8..12].copy_from_slice(&self.flags.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.request_id.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.payload_len.to_le_bytes());
        bytes[20..24].copy_from_slice(&0_u32.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: [u8; HEADER_LEN]) -> Result<Self, ProtocolError> {
        if bytes[0..4] != MAGIC {
            return Err(ProtocolError::InvalidMagic(
                bytes[0..4].try_into().unwrap_or_default(),
            ));
        }

        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        let kind = FrameKind::from_u16(u16::from_le_bytes([bytes[6], bytes[7]]))?;
        let flags = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let request_id = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let payload_len = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);

        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        if payload_len as usize > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge(payload_len as usize));
        }

        Ok(Self {
            version,
            kind,
            flags,
            request_id,
            payload_len,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFrame {
    pub request_id: u32,
    pub channel: StreamChannel,
    pub eof: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitySet {
    pub protocol_version: u16,
    pub transports: Vec<String>,
    pub security: Vec<String>,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscoveryRequest {
    Probe { nonce: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryResponse {
    pub nonce: u64,
    pub server_id: String,
    pub device_name: String,
    pub platform: String,
    pub tcp_port: u16,
    pub protocol_version: u16,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    CapabilityMissing,
    ExecFailed,
    ExecStartFailed,
    ExecTimeout,
    FileTransferFailed,
    NotFound,
    FsNotFound,
    FsPermissionDenied,
    FsPreconditionFailed,
    FsNoSpace,
    TransferInterrupted,
    TransferVerificationFailed,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferRoot {
    pub source_name: String,
    pub kind: TransferEntryKind,
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferEntry {
    pub root_index: u32,
    pub relative_path: String,
    pub kind: TransferEntryKind,
    pub mode: u32,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FsEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    Sha256,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContentEncoding {
    #[default]
    Utf8,
    Base64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentFsStat {
    pub path: String,
    pub exists: bool,
    pub kind: Option<FsEntryKind>,
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub mtime_unix_ms: Option<u64>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentFsEntry {
    pub path: String,
    pub kind: FsEntryKind,
    pub size: u64,
    pub mode: u32,
    pub mtime_unix_ms: Option<u64>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentFsReadResult {
    pub path: String,
    pub encoding: ContentEncoding,
    pub content: String,
    pub bytes: u64,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentFsWriteResult {
    pub path: String,
    pub bytes_written: u64,
    pub mode: Option<u32>,
    pub sha256: Option<String>,
    #[serde(default)]
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentFsMkdirResult {
    pub path: String,
    #[serde(default)]
    pub created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentFsRmResult {
    pub path: String,
    #[serde(default)]
    pub removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentFsMoveResult {
    pub source: String,
    pub destination: String,
    #[serde(default)]
    pub overwritten: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    Ping,
    GetCapabilities,
    Exec {
        command: String,
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        stream: bool,
    },
    FsStat {
        path: String,
        #[serde(default)]
        hash: Option<HashAlgorithm>,
    },
    FsList {
        path: String,
        #[serde(default)]
        recursive: bool,
        #[serde(default)]
        max_depth: Option<u32>,
        #[serde(default)]
        include_hidden: bool,
        #[serde(default)]
        hash: Option<HashAlgorithm>,
    },
    FsRead {
        path: String,
        #[serde(default)]
        encoding: ContentEncoding,
        #[serde(default)]
        max_bytes: Option<u64>,
    },
    FsWrite {
        path: String,
        content: String,
        #[serde(default)]
        encoding: ContentEncoding,
        #[serde(default)]
        mode: Option<u32>,
        #[serde(default)]
        create_parent: bool,
        #[serde(default)]
        atomic: bool,
        #[serde(default)]
        if_missing: bool,
        #[serde(default)]
        if_sha256: Option<String>,
    },
    FsMkdir {
        path: String,
        #[serde(default)]
        parents: bool,
        #[serde(default)]
        mode: Option<u32>,
    },
    FsRm {
        path: String,
        #[serde(default)]
        recursive: bool,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        if_exists: bool,
    },
    FsMove {
        source: String,
        destination: String,
        #[serde(default)]
        overwrite: bool,
    },
    Push {
        path: String,
        mode: u32,
        #[serde(default)]
        source_name: Option<String>,
    },
    Pull {
        path: String,
    },
    PushBatch {
        destination: String,
        roots: Vec<TransferRoot>,
        entries: Vec<TransferEntry>,
    },
    PullBatch {
        sources: Vec<String>,
    },
    Shell {
        command: Option<String>,
        args: Vec<String>,
        #[serde(default)]
        pty: bool,
        #[serde(default)]
        term: Option<String>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        cols: Option<u16>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    Pong {
        server_id: String,
        protocol_version: u16,
    },
    Capabilities {
        server_id: String,
        capability: CapabilitySet,
    },
    ExecResult {
        status: i32,
        stdout: String,
        stderr: String,
        #[serde(default)]
        timed_out: bool,
    },
    FsStat {
        stat: AgentFsStat,
    },
    FsList {
        entries: Vec<AgentFsEntry>,
    },
    FsReadResult {
        result: AgentFsReadResult,
    },
    FsWriteResult {
        result: AgentFsWriteResult,
    },
    FsMkdirResult {
        result: AgentFsMkdirResult,
    },
    FsRmResult {
        result: AgentFsRmResult,
    },
    FsMoveResult {
        result: AgentFsMoveResult,
    },
    PushReady,
    PushComplete {
        bytes_written: u64,
    },
    PullMetadata {
        size: u64,
        mode: u32,
    },
    PullComplete {
        bytes_sent: u64,
    },
    PushBatchReady,
    PushBatchComplete {
        entries_written: u64,
        bytes_written: u64,
    },
    PullBatchMetadata {
        roots: Vec<TransferRoot>,
        entries: Vec<TransferEntry>,
        total_bytes: u64,
    },
    PullBatchComplete {
        entries_sent: u64,
        bytes_sent: u64,
    },
    ShellStarted {
        command: String,
    },
    ShellExit {
        status: i32,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid magic bytes: {0:?}")]
    InvalidMagic([u8; 4]),
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u16),
    #[error("unknown frame kind: {0}")]
    UnknownFrameKind(u16),
    #[error("unknown stream channel: {0}")]
    UnknownStreamChannel(u32),
    #[error("invalid discovery magic bytes")]
    InvalidDiscoveryMagic,
    #[error("payload too large: {0} bytes")]
    PayloadTooLarge(usize),
    #[error("discovery payload too large: {0} bytes")]
    DiscoveryPayloadTooLarge(usize),
    #[error("expected frame kind {expected:?}, got {actual:?}")]
    UnexpectedFrameKind {
        expected: FrameKind,
        actual: FrameKind,
    },
}

pub async fn write_frame<W>(
    writer: &mut W,
    kind: FrameKind,
    request_id: u32,
    payload: &[u8],
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(ProtocolError::PayloadTooLarge(payload.len()));
    }

    let header = FrameHeader {
        version: PROTOCOL_VERSION,
        kind,
        flags: 0,
        request_id,
        payload_len: payload.len() as u32,
    };

    writer.write_all(&header.encode()).await?;
    writer.write_all(payload).await?;
    Ok(())
}

pub async fn read_frame<R>(reader: &mut R) -> Result<Frame, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut header_bytes = [0_u8; HEADER_LEN];
    reader.read_exact(&mut header_bytes).await?;
    let header = FrameHeader::decode(header_bytes)?;
    let mut payload = vec![0_u8; header.payload_len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Frame { header, payload })
}

pub async fn write_json_frame<W, T>(
    writer: &mut W,
    kind: FrameKind,
    request_id: u32,
    message: &T,
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(message)?;
    write_frame(writer, kind, request_id, &payload).await
}

pub fn encode_discovery_message<T>(message: &T) -> Result<Vec<u8>, ProtocolError>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_DISCOVERY_PAYLOAD_LEN {
        return Err(ProtocolError::DiscoveryPayloadTooLarge(payload.len()));
    }

    let mut message_bytes = Vec::with_capacity(DISCOVERY_MAGIC.len() + payload.len());
    message_bytes.extend_from_slice(&DISCOVERY_MAGIC);
    message_bytes.extend_from_slice(&payload);
    Ok(message_bytes)
}

pub fn decode_discovery_message<T>(bytes: &[u8]) -> Result<T, ProtocolError>
where
    T: DeserializeOwned,
{
    if bytes.len() < DISCOVERY_MAGIC.len() || bytes[..DISCOVERY_MAGIC.len()] != DISCOVERY_MAGIC {
        return Err(ProtocolError::InvalidDiscoveryMagic);
    }

    let payload = &bytes[DISCOVERY_MAGIC.len()..];
    if payload.len() > MAX_DISCOVERY_PAYLOAD_LEN {
        return Err(ProtocolError::DiscoveryPayloadTooLarge(payload.len()));
    }

    Ok(serde_json::from_slice(payload)?)
}

pub async fn write_stream_frame<W>(
    writer: &mut W,
    request_id: u32,
    channel: StreamChannel,
    eof: bool,
    payload: &[u8],
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    let mut flags = channel as u32;
    if eof {
        flags |= STREAM_EOF_FLAG;
    }

    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(ProtocolError::PayloadTooLarge(payload.len()));
    }

    let header = FrameHeader {
        version: PROTOCOL_VERSION,
        kind: FrameKind::Stream,
        flags,
        request_id,
        payload_len: payload.len() as u32,
    };

    writer.write_all(&header.encode()).await?;
    writer.write_all(payload).await?;
    Ok(())
}

pub fn decode_json<T>(frame: &Frame, expected_kind: FrameKind) -> Result<T, ProtocolError>
where
    T: DeserializeOwned,
{
    if frame.header.kind != expected_kind {
        return Err(ProtocolError::UnexpectedFrameKind {
            expected: expected_kind,
            actual: frame.header.kind,
        });
    }

    Ok(serde_json::from_slice(&frame.payload)?)
}

pub fn decode_stream_frame(frame: Frame) -> Result<StreamFrame, ProtocolError> {
    if frame.header.kind != FrameKind::Stream {
        return Err(ProtocolError::UnexpectedFrameKind {
            expected: FrameKind::Stream,
            actual: frame.header.kind,
        });
    }

    let channel = StreamChannel::from_u32(frame.header.flags & STREAM_CHANNEL_MASK)?;
    let eof = frame.header.flags & STREAM_EOF_FLAG != 0;
    Ok(StreamFrame {
        request_id: frame.header.request_id,
        channel,
        eof,
        payload: frame.payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[test]
    fn header_round_trip() {
        let header = FrameHeader {
            version: PROTOCOL_VERSION,
            kind: FrameKind::Request,
            flags: 7,
            request_id: 41,
            payload_len: 128,
        };

        let encoded = header.encode();
        let decoded = FrameHeader::decode(encoded).expect("header should decode");
        assert_eq!(header, decoded);
    }

    #[tokio::test]
    async fn json_frame_round_trip() {
        let (mut client, mut server) = duplex(1024);
        let request = ControlRequest::Exec {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            cwd: Some("/tmp".to_string()),
            timeout_secs: Some(5),
            stream: false,
        };

        let expected = request.clone();
        let send = tokio::spawn(async move {
            write_json_frame(&mut client, FrameKind::Request, 9, &request)
                .await
                .expect("frame should write");
        });

        let frame = read_frame(&mut server).await.expect("frame should read");
        send.await.expect("writer task should finish");
        let decoded: ControlRequest =
            decode_json(&frame, FrameKind::Request).expect("request should decode");
        assert_eq!(decoded, expected);
        assert_eq!(frame.header.request_id, 9);
    }

    #[tokio::test]
    async fn stream_frame_round_trip() {
        let (mut client, mut server) = duplex(1024);
        let expected = b"chunk-data".to_vec();

        let send = tokio::spawn(async move {
            write_stream_frame(&mut client, 5, StreamChannel::File, true, &expected)
                .await
                .expect("stream frame should write");
        });

        let frame = read_frame(&mut server).await.expect("frame should read");
        send.await.expect("writer task should finish");
        let decoded = decode_stream_frame(frame).expect("stream frame should decode");
        assert_eq!(decoded.request_id, 5);
        assert_eq!(decoded.channel, StreamChannel::File);
        assert!(decoded.eof);
        assert_eq!(decoded.payload, b"chunk-data");
    }

    #[tokio::test]
    async fn fs_request_round_trip() {
        let (mut client, mut server) = duplex(1024);
        let request = ControlRequest::FsWrite {
            path: "/tmp/example.txt".to_string(),
            content: "aGVsbG8=".to_string(),
            encoding: ContentEncoding::Base64,
            mode: Some(0o600),
            create_parent: true,
            atomic: true,
            if_missing: false,
            if_sha256: Some("deadbeef".to_string()),
        };

        let expected = request.clone();
        let send = tokio::spawn(async move {
            write_json_frame(&mut client, FrameKind::Request, 10, &request)
                .await
                .expect("frame should write");
        });

        let frame = read_frame(&mut server).await.expect("frame should read");
        send.await.expect("writer task should finish");
        let decoded: ControlRequest =
            decode_json(&frame, FrameKind::Request).expect("request should decode");
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn fs_response_round_trip() {
        let (mut client, mut server) = duplex(1024);
        let response = ControlResponse::FsStat {
            stat: AgentFsStat {
                path: "/tmp/example.txt".to_string(),
                exists: true,
                kind: Some(FsEntryKind::File),
                size: Some(5),
                mode: Some(0o644),
                mtime_unix_ms: Some(42),
                sha256: Some("abc".to_string()),
            },
        };

        let expected = response.clone();
        let send = tokio::spawn(async move {
            write_json_frame(&mut client, FrameKind::Response, 11, &response)
                .await
                .expect("frame should write");
        });

        let frame = read_frame(&mut server).await.expect("frame should read");
        send.await.expect("writer task should finish");
        let decoded: ControlResponse =
            decode_json(&frame, FrameKind::Response).expect("response should decode");
        assert_eq!(decoded, expected);
    }

    #[test]
    fn error_code_serialization_is_stable() {
        let encoded = serde_json::to_string(&ErrorCode::FsPreconditionFailed)
            .expect("error code should encode");
        assert_eq!(encoded, "\"fs_precondition_failed\"");
    }

    #[test]
    fn discovery_message_round_trip() {
        let expected = DiscoveryResponse {
            nonce: 7,
            server_id: "rsdbd-test".to_string(),
            device_name: "test-device".to_string(),
            platform: "Tizen 10.0 (aarch64)".to_string(),
            tcp_port: 27101,
            protocol_version: PROTOCOL_VERSION,
            features: vec!["discover.udp".to_string(), "shell.exec".to_string()],
        };

        let encoded = encode_discovery_message(&expected).expect("discovery should encode");
        let decoded: DiscoveryResponse =
            decode_discovery_message(&encoded).expect("discovery should decode");
        assert_eq!(decoded, expected);
    }
}
