use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rsdb_proto::{
    DEFAULT_STREAM_CHUNK_SIZE, FrameHeader, FrameKind, PROTOCOL_VERSION, StreamChannel,
    decode_stream_frame_header, read_frame, read_frame_into,
};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::runtime::Builder;

struct CountingAlloc;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[derive(Clone, Copy)]
struct BenchResult {
    elapsed: Duration,
    allocations: usize,
}

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

fn frame_bytes() -> Vec<u8> {
    let payload = vec![b'x'; DEFAULT_STREAM_CHUNK_SIZE];
    let header = FrameHeader {
        version: PROTOCOL_VERSION,
        kind: FrameKind::Stream,
        flags: StreamChannel::Stdout as u32,
        request_id: 7,
        payload_len: payload.len() as u32,
    };

    let mut bytes = Vec::with_capacity(header.encode().len() + payload.len());
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(&payload);
    bytes
}

fn reset_allocations() {
    ALLOCATIONS.store(0, Ordering::Relaxed);
}

fn allocations() -> usize {
    ALLOCATIONS.load(Ordering::Relaxed)
}

fn run_read_frame(
    runtime: &tokio::runtime::Runtime,
    frame_bytes: &[u8],
    iterations: usize,
) -> BenchResult {
    reset_allocations();
    let started = Instant::now();
    runtime.block_on(async {
        for _ in 0..iterations {
            let mut reader = SliceReader::new(frame_bytes);
            let frame = read_frame(&mut reader).await.expect("frame should decode");
            black_box(frame.payload.len());
        }
    });
    BenchResult {
        elapsed: started.elapsed(),
        allocations: allocations(),
    }
}

fn run_read_frame_into(
    runtime: &tokio::runtime::Runtime,
    frame_bytes: &[u8],
    iterations: usize,
) -> BenchResult {
    reset_allocations();
    let started = Instant::now();
    runtime.block_on(async {
        let mut payload = Vec::with_capacity(DEFAULT_STREAM_CHUNK_SIZE);
        for _ in 0..iterations {
            let mut reader = SliceReader::new(frame_bytes);
            let header = read_frame_into(&mut reader, &mut payload)
                .await
                .expect("frame should decode");
            let stream = decode_stream_frame_header(&header).expect("stream header should decode");
            black_box((stream.request_id, payload.len()));
        }
    });
    BenchResult {
        elapsed: started.elapsed(),
        allocations: allocations(),
    }
}

fn main() {
    const ITERATIONS: usize = 20_000;

    let runtime = Builder::new_current_thread()
        .build()
        .expect("runtime should build");
    let frame_bytes = frame_bytes();

    // Warm the code paths before counting allocations.
    let _ = run_read_frame(&runtime, &frame_bytes, 1_000);
    let _ = run_read_frame_into(&runtime, &frame_bytes, 1_000);

    let baseline = run_read_frame(&runtime, &frame_bytes, ITERATIONS);
    let optimized = run_read_frame_into(&runtime, &frame_bytes, ITERATIONS);

    println!(
        "read_frame      {:>8?}  {:>8} allocs",
        baseline.elapsed, baseline.allocations
    );
    println!(
        "read_frame_into {:>8?}  {:>8} allocs",
        optimized.elapsed, optimized.allocations
    );
}
