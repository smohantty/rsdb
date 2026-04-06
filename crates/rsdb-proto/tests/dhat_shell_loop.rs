#[cfg(feature = "dhat-heap")]
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

/// Simulates the OLD shell reader loop: read_frame() per iteration,
/// allocating a new Vec<u8> payload each time.
#[test]
#[ignore]
fn shell_loop_old_read_frame() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_stream_frame_bytes(DEFAULT_STREAM_CHUNK_SIZE);
    let iterations = 5_000;

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut reader = MultiFrameReader::new(&frame_bytes, iterations);
        for _ in 0..iterations {
            let frame = read_frame(&mut reader).await.unwrap();
            // Simulate what the old shell loop did: decode + write payload
            let hdr = decode_stream_frame_header(&frame.header).unwrap();
            black_box((hdr.request_id, hdr.channel));
            black_box(frame.payload.len());
            // payload Vec dropped here — allocated fresh next iteration
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== OLD shell loop: read_frame ({iterations} iters, {}KB chunks) ===", DEFAULT_STREAM_CHUNK_SIZE / 1024);
    eprintln!("  Total bytes:       {:>12}", stats.total_bytes);
    eprintln!("  Total blocks:      {:>12}", stats.total_blocks);
    eprintln!("  Max bytes live:    {:>12}", stats.max_bytes);
    eprintln!("  Bytes per iter:    {:>12}", stats.total_bytes / iterations as u64);
    eprintln!("  Blocks per iter:   {:>12}", stats.total_blocks / iterations as u64);
}

/// Simulates the NEW shell reader loop: read_frame_into() reusing a
/// single Vec<u8> across all iterations — zero per-frame allocations.
#[test]
#[ignore]
fn shell_loop_new_read_frame_into() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let frame_bytes = make_stream_frame_bytes(DEFAULT_STREAM_CHUNK_SIZE);
    let iterations = 5_000;

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut payload = Vec::with_capacity(DEFAULT_STREAM_CHUNK_SIZE);
        let mut reader = MultiFrameReader::new(&frame_bytes, iterations);
        for _ in 0..iterations {
            let header = read_frame_into(&mut reader, &mut payload).await.unwrap();
            // Simulate what the new shell loop does: decode header + use payload inline
            let stream = decode_stream_frame_header(&header).unwrap();
            black_box((stream.request_id, stream.channel));
            black_box(payload.len());
            // payload NOT dropped — reused next iteration
        }
    });

    let stats = dhat::HeapStats::get();
    eprintln!("\n=== NEW shell loop: read_frame_into ({iterations} iters, {}KB chunks) ===", DEFAULT_STREAM_CHUNK_SIZE / 1024);
    eprintln!("  Total bytes:       {:>12}", stats.total_bytes);
    eprintln!("  Total blocks:      {:>12}", stats.total_blocks);
    eprintln!("  Max bytes live:    {:>12}", stats.max_bytes);
    eprintln!("  Bytes per iter:    {:>12}", stats.total_bytes / iterations as u64);
    eprintln!("  Blocks per iter:   {:>12}", stats.total_blocks / iterations as u64);
}
