#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use rsdb_proto::*;
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

struct SliceReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> SliceReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

}

impl AsyncRead for SliceReader<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let remaining = &self.bytes[self.offset..];
        if remaining.is_empty() {
            return Poll::Ready(Ok(()));
        }
        let to_copy = remaining.len().min(buf.remaining());
        buf.put_slice(&remaining[..to_copy]);
        self.offset += to_copy;
        Poll::Ready(Ok(()))
    }
}

fn make_stream_frame_bytes(payload_size: usize) -> Vec<u8> {
    let payload = vec![b'x'; payload_size];
    let header = FrameHeader {
        version: PROTOCOL_VERSION,
        kind: FrameKind::Stream,
        flags: StreamChannel::Stdout as u32,
        request_id: 1,
        payload_len: payload.len() as u32,
    };
    let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(&payload);
    bytes
}

fn make_json_request_bytes() -> Vec<u8> {
    let request = ControlRequest::Exec {
        command: "ls".to_string(),
        args: vec!["-la".to_string(), "/tmp".to_string()],
        cwd: Some("/home".to_string()),
        timeout_secs: Some(30),
        stream: false,
    };
    let payload = serde_json::to_vec(&request).unwrap();
    let header = FrameHeader {
        version: PROTOCOL_VERSION,
        kind: FrameKind::Request,
        flags: 0,
        request_id: 1,
        payload_len: payload.len() as u32,
    };
    let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(&payload);
    bytes
}

fn make_json_response_bytes() -> Vec<u8> {
    let response = ControlResponse::FsList {
        entries: (0..50)
            .map(|i| AgentFsListEntry {
                path: format!("/tmp/file_{i}.txt"),
                kind: FsEntryKind::File,
                size: 1024 * i,
                sha256: Some(format!("deadbeef{i:04}")),
            })
            .collect(),
    };
    let payload = serde_json::to_vec(&response).unwrap();
    let header = FrameHeader {
        version: PROTOCOL_VERSION,
        kind: FrameKind::Response,
        flags: 0,
        request_id: 1,
        payload_len: payload.len() as u32,
    };
    let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(&payload);
    bytes
}

#[test]
#[ignore]
fn profile_read_frame() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_stream_frame_bytes(DEFAULT_STREAM_CHUNK_SIZE);
    let iterations = 1_000;

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        for _ in 0..iterations {
            let mut reader = SliceReader::new(&frame_bytes);
            let frame = read_frame(&mut reader).await.unwrap();
            black_box(frame.payload.len());
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== read_frame ({iterations} iterations, {}KB payload) ===", DEFAULT_STREAM_CHUNK_SIZE / 1024);
    eprintln!("  Total bytes:       {}", stats.total_bytes);
    eprintln!("  Total blocks:      {}", stats.total_blocks);
    eprintln!("  Max bytes live:    {}", stats.max_bytes);
    eprintln!("  Max blocks live:   {}", stats.max_blocks);
    eprintln!("  Bytes per iter:    {}", stats.total_bytes / iterations);
    eprintln!("  Blocks per iter:   {}", stats.total_blocks / iterations);
}

#[test]
#[ignore]
fn profile_read_frame_into() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_stream_frame_bytes(DEFAULT_STREAM_CHUNK_SIZE);
    let iterations = 1_000;

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut payload = Vec::with_capacity(DEFAULT_STREAM_CHUNK_SIZE);
        for _ in 0..iterations {
            let mut reader = SliceReader::new(&frame_bytes);
            let header = read_frame_into(&mut reader, &mut payload).await.unwrap();
            let stream = decode_stream_frame_header(&header).unwrap();
            black_box((stream.request_id, payload.len()));
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== read_frame_into ({iterations} iterations, {}KB payload) ===", DEFAULT_STREAM_CHUNK_SIZE / 1024);
    eprintln!("  Total bytes:       {}", stats.total_bytes);
    eprintln!("  Total blocks:      {}", stats.total_blocks);
    eprintln!("  Max bytes live:    {}", stats.max_bytes);
    eprintln!("  Max blocks live:   {}", stats.max_blocks);
    eprintln!("  Bytes per iter:    {}", stats.total_bytes / iterations);
    eprintln!("  Blocks per iter:   {}", stats.total_blocks / iterations);
}

#[test]
#[ignore]
fn profile_json_request_decode() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_json_request_bytes();
    let iterations = 1_000;

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        for _ in 0..iterations {
            let mut reader = SliceReader::new(&frame_bytes);
            let frame = read_frame(&mut reader).await.unwrap();
            let req: ControlRequest = decode_json(&frame, FrameKind::Request).unwrap();
            black_box(&req);
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== json request decode ({iterations} iterations) ===");
    eprintln!("  Total bytes:       {}", stats.total_bytes);
    eprintln!("  Total blocks:      {}", stats.total_blocks);
    eprintln!("  Max bytes live:    {}", stats.max_bytes);
    eprintln!("  Max blocks live:   {}", stats.max_blocks);
    eprintln!("  Bytes per iter:    {}", stats.total_bytes / iterations);
    eprintln!("  Blocks per iter:   {}", stats.total_blocks / iterations);
}

#[test]
#[ignore]
fn profile_json_response_decode() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_json_response_bytes();
    let iterations = 1_000;

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        for _ in 0..iterations {
            let mut reader = SliceReader::new(&frame_bytes);
            let frame = read_frame(&mut reader).await.unwrap();
            let resp: ControlResponse = decode_json(&frame, FrameKind::Response).unwrap();
            black_box(&resp);
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== json response decode (50-entry FsList, {iterations} iterations) ===");
    eprintln!("  Total bytes:       {}", stats.total_bytes);
    eprintln!("  Total blocks:      {}", stats.total_blocks);
    eprintln!("  Max bytes live:    {}", stats.max_bytes);
    eprintln!("  Max blocks live:   {}", stats.max_blocks);
    eprintln!("  Bytes per iter:    {}", stats.total_bytes / iterations);
    eprintln!("  Blocks per iter:   {}", stats.total_blocks / iterations);
}

#[test]
#[ignore]
fn profile_discovery_round_trip() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let iterations = 1_000;

    let response = DiscoveryResponse {
        nonce: 42,
        server_id: "rsdbd-test".to_string(),
        device_name: "test-device".to_string(),
        platform: "Tizen 10.0 (aarch64)".to_string(),
        tcp_port: 27101,
        protocol_version: PROTOCOL_VERSION,
        features: vec!["discover.udp".to_string(), "shell.exec".to_string()],
    };

    for _ in 0..iterations {
        let encoded = encode_discovery_message(&response).unwrap();
        let decoded: DiscoveryResponse = decode_discovery_message(&encoded).unwrap();
        black_box(&decoded);
    }

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== discovery round-trip ({iterations} iterations) ===");
    eprintln!("  Total bytes:       {}", stats.total_bytes);
    eprintln!("  Total blocks:      {}", stats.total_blocks);
    eprintln!("  Max bytes live:    {}", stats.max_bytes);
    eprintln!("  Max blocks live:   {}", stats.max_blocks);
    eprintln!("  Bytes per iter:    {}", stats.total_bytes / iterations);
    eprintln!("  Blocks per iter:   {}", stats.total_blocks / iterations);
}
