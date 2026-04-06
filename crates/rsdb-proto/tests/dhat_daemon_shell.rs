#![cfg(feature = "dhat-heap")]

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use rsdb_proto::*;
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

struct MultiFrameReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    remaining: usize,
}

impl<'a> MultiFrameReader<'a> {
    fn new(bytes: &'a [u8], count: usize) -> Self {
        Self {
            bytes,
            offset: 0,
            remaining: count,
        }
    }
}

impl AsyncRead for MultiFrameReader<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.offset >= self.bytes.len() {
            if self.remaining == 0 {
                return Poll::Ready(Ok(()));
            }
            self.offset = 0;
            self.remaining -= 1;
        }
        let remaining = &self.bytes[self.offset..];
        let to_copy = remaining.len().min(buf.remaining());
        buf.put_slice(&remaining[..to_copy]);
        self.offset += to_copy;
        Poll::Ready(Ok(()))
    }
}

fn make_stdin_frame_bytes(payload_size: usize) -> Vec<u8> {
    let payload = vec![b'A'; payload_size];
    let header = FrameHeader {
        version: PROTOCOL_VERSION,
        kind: FrameKind::Stream,
        flags: StreamChannel::Stdin as u32,
        request_id: 1,
        payload_len: payload.len() as u32,
    };
    let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(&payload);
    bytes
}

/// OLD daemon pattern: read_frame() allocates a new Vec per frame,
/// then decode_stream_frame() to extract payload.
#[test]
fn daemon_old_read_frame_small_stdin() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Interactive stdin: small frames (64 bytes, like keystrokes)
    let frame_bytes = make_stdin_frame_bytes(64);
    let iterations = 5_000;

    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        let mut reader = MultiFrameReader::new(&frame_bytes, iterations);
        for _ in 0..iterations {
            let frame = read_frame(&mut reader).await.unwrap();
            let chunk = decode_stream_frame(frame).unwrap();
            black_box(chunk.payload.len());
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== OLD daemon: read_frame, small stdin ({iterations} iters, 64B) ===");
    eprintln!("  Total bytes:       {:>12}", stats.total_bytes);
    eprintln!("  Total blocks:      {:>12}", stats.total_blocks);
    eprintln!("  Bytes per iter:    {:>12}", stats.total_bytes / iterations as u64);
    eprintln!("  Blocks per iter:   {:>12}", stats.total_blocks / iterations as u64);
}

/// NEW daemon pattern: read_stream_frame_into() reuses buffer.
#[test]
fn daemon_new_read_frame_into_small_stdin() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_stdin_frame_bytes(64);
    let iterations = 5_000;

    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        let mut payload = Vec::with_capacity(DEFAULT_STREAM_CHUNK_SIZE);
        let mut reader = MultiFrameReader::new(&frame_bytes, iterations);
        for _ in 0..iterations {
            let header = read_stream_frame_into(&mut reader, &mut payload).await.unwrap();
            black_box((header.request_id, payload.len()));
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== NEW daemon: read_stream_frame_into, small stdin ({iterations} iters, 64B) ===");
    eprintln!("  Total bytes:       {:>12}", stats.total_bytes);
    eprintln!("  Total blocks:      {:>12}", stats.total_blocks);
    eprintln!("  Bytes per iter:    {:>12}", stats.total_bytes / iterations as u64);
    eprintln!("  Blocks per iter:   {:>12}", stats.total_blocks / iterations as u64);
}

/// OLD daemon pattern with large piped stdin (64KB chunks).
#[test]
fn daemon_old_read_frame_large_stdin() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_stdin_frame_bytes(DEFAULT_STREAM_CHUNK_SIZE);
    let iterations = 5_000;

    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        let mut reader = MultiFrameReader::new(&frame_bytes, iterations);
        for _ in 0..iterations {
            let frame = read_frame(&mut reader).await.unwrap();
            let chunk = decode_stream_frame(frame).unwrap();
            black_box(chunk.payload.len());
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== OLD daemon: read_frame, large stdin ({iterations} iters, 64KB) ===");
    eprintln!("  Total bytes:       {:>12}", stats.total_bytes);
    eprintln!("  Total blocks:      {:>12}", stats.total_blocks);
    eprintln!("  Bytes per iter:    {:>12}", stats.total_bytes / iterations as u64);
    eprintln!("  Blocks per iter:   {:>12}", stats.total_blocks / iterations as u64);
}

/// NEW daemon pattern with large piped stdin (64KB chunks).
#[test]
fn daemon_new_read_frame_into_large_stdin() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_stdin_frame_bytes(DEFAULT_STREAM_CHUNK_SIZE);
    let iterations = 5_000;

    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        let mut payload = Vec::with_capacity(DEFAULT_STREAM_CHUNK_SIZE);
        let mut reader = MultiFrameReader::new(&frame_bytes, iterations);
        for _ in 0..iterations {
            let header = read_stream_frame_into(&mut reader, &mut payload).await.unwrap();
            black_box((header.request_id, payload.len()));
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== NEW daemon: read_stream_frame_into, large stdin ({iterations} iters, 64KB) ===");
    eprintln!("  Total bytes:       {:>12}", stats.total_bytes);
    eprintln!("  Total blocks:      {:>12}", stats.total_blocks);
    eprintln!("  Bytes per iter:    {:>12}", stats.total_bytes / iterations as u64);
    eprintln!("  Blocks per iter:   {:>12}", stats.total_blocks / iterations as u64);
}
