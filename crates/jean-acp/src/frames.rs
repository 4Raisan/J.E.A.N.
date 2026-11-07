//! Frame bounds — the ceiling between an agent's output and this process's memory.
//!
//! Both transports this crate speaks are newline-delimited JSON. Neither of the two readers available
//! to us enforces a maximum line length: the ACP SDK wraps the incoming stream in
//! `BufReader::lines()` and will buffer a line of any size, and the CLI fallback reads the pipe
//! itself. A worker that emits a 2 GiB line — by bug or by design — is a denial of service against
//! the operator's machine, so `Bounds::max_frame_bytes` is enforced in this module at the byte
//! level, *under* whatever framing sits on top of it.
//!
//! Two shapes, because the two connectors are driven differently:
//!
//! * [`BoundedRead`] / [`BoundedWrite`] implement `futures`' async traits and wrap the pipes handed
//!   to the ACP SDK. The cap is per frame: the counter resets at every `\n`, so a long *session* of
//!   many normal frames never trips it, and one oversized frame fails immediately — the SDK reports
//!   that as a broken connection, which is the correct outcome, rather than hanging.
//! * [`LineReader`] is synchronous, over [`PipedChild::read_stdout`], for the CLI connector, which
//!   never needs an executor: it reads a line, acts on it, reads the next.
//!
//! Both count every byte they pass into a shared [`FrameCount`], because the report needs "how much
//! did this agent actually say", and a worker truncated at the cap must not look like one that ran
//! quietly.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use futures::ready;
use futures::{AsyncRead, AsyncWrite};

use jean_process::piped::PipedChild;
use jean_process::win32::PipeRead;

/// Bytes passed through the readers, shared with whoever reports the session.
#[derive(Clone, Debug)]
pub struct FrameCount(Arc<AtomicU64>);

impl Default for FrameCount {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameCount {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    pub fn add(&self, bytes: u64) {
        self.0.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Which side of the pipe a traced frame crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Bytes the worker sent.
    In,
    /// Bytes J.E.A.N. sent.
    Out,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}

/// How many leading bytes of a frame the trace is allowed to hold in memory.
///
/// A JSON-RPC frame puts `method` and `id` first, so the interesting part is always near the front,
/// and "near the front" is a bound a reader can check. The buffer is dropped when the line ends.
const TRACE_PREFIX_BYTES: usize = 120;

/// Frames a session traces before going quiet.
///
/// Instrumentation without a ceiling is a second bug — a chatty agent would fill the log faster than
/// the frame cap fills memory — so the count is enforced here rather than left to a log filter.
const TRACE_FRAMES: usize = 512;

/// A bounded, redacted record of the frames that crossed a transport.
///
/// This exists because the failure it describes was invisible: a session that never settles is either
/// waiting for a reply it never sent or ignoring one it never received, and no log level short of raw
/// traffic distinguishes them. So the trace reports *shape* — direction, byte length, and the
/// protocol's own `method` and `id`, which are wire vocabulary rather than payload — and never the
/// body. A file's contents, a prompt, a credential and a tool's arguments are all in the part that is
/// not recorded.
///
/// Opt-in through `JEAN_ACP_TRACE=1`, off by default: a delegation's traffic is the operator's, and
/// nothing writes it out because a connector was compiled.
#[derive(Debug)]
pub struct WireTrace {
    frames: std::sync::atomic::AtomicUsize,
    limit: usize,
    loud: bool,
}

impl WireTrace {
    /// The trace the environment asks for, if any. Shared by both directions of one session, so the
    /// cap is per session rather than per half.
    ///
    /// `JEAN_ACP_TRACE=1` goes through `tracing`, which is where an operator's log sees it. `=2` also
    /// writes to stderr, for the one situation where the log layer is the thing under test: a session
    /// that never returns has no process left to flush a buffered appender with, and `--nocapture` on a
    /// test is the only witness available.
    pub fn from_env() -> Option<Arc<Self>> {
        let loud = match std::env::var("JEAN_ACP_TRACE").ok().as_deref() {
            Some("1") => false,
            Some("2") => true,
            _ => return None,
        };
        Some(Arc::new(Self {
            frames: std::sync::atomic::AtomicUsize::new(0),
            limit: TRACE_FRAMES,
            loud,
        }))
    }

    /// A point in the session's own progress, recorded the same way as a frame: bounded, and no
    /// payload. This is what distinguishes "the loop never saw the dispatch" from "the handler ran and
    /// never came back", which is the question a stalled session actually asks.
    pub fn note(&self, stage: &str) {
        let seen = self.frames.fetch_add(1, Ordering::Relaxed);
        if seen >= self.limit {
            return;
        }
        if self.loud {
            eprintln!("[acp trace] stage={stage} frame={}", seen + 1);
        }
        tracing::debug!(stage, frame = seen + 1, "ACP session stage");
    }

    /// One frame, described and discarded. Called once per complete line, never per chunk.
    fn record(&self, direction: Direction, bytes: usize, prefix: &[u8]) {
        let seen = self.frames.fetch_add(1, Ordering::Relaxed);
        if seen >= self.limit {
            // Announce the cap once, so a silent trace is distinguishable from a quiet worker.
            if seen == self.limit {
                tracing::warn!(
                    frames = self.limit,
                    direction = direction.as_str(),
                    "ACP frame trace capped; later frames are counted but not recorded"
                );
            }
            return;
        }
        let text = String::from_utf8_lossy(prefix);
        if self.loud {
            // The same fields, to stderr: `tracing` needs a subscriber, and a session killed at its
            // deadline usually does not have a live one.
            eprintln!(
                "[acp trace] dir={} frame={} bytes={} method={:?} id={:?}",
                direction.as_str(),
                seen + 1,
                bytes,
                field_of(&text, "method"),
                field_of(&text, "id")
            );
        }
        tracing::debug!(
            direction = direction.as_str(),
            frame = seen + 1,
            bytes,
            method = field_of(&text, "method"),
            id = field_of(&text, "id"),
            "ACP frame"
        );
    }
}

/// The value of a top-level JSON-RPC field, as far as it appears in a traced prefix.
///
/// Deliberately a scan rather than a parse: this runs on the transport's hot path, the payload may be
/// anything, and the answer is only ever a label in a log line. `None` for a value beyond the prefix,
/// which is honest — it says "not visible", not "absent".
fn field_of(line: &str, key: &str) -> Option<String> {
    let at = line.find(&format!("\"{key}\""))?;
    let colon = line[at..].find(':')?;
    let value = line[at + colon + 1..].trim_start();
    let value = if let Some(rest) = value.strip_prefix('"') {
        let end = rest.find('"').unwrap_or(rest.len()).min(48);
        &rest[..end]
    } else {
        // Unquoted: a number, `null`, or the start of an object — keep the leading token only.
        value
            .find([',', '}', ' '])
            .map(|end| &value[..end])
            .unwrap_or(value)
    };
    let label: String = value.chars().take(48).collect();
    (!label.is_empty()).then_some(label)
}

/// The leading bytes of the frame currently being assembled, for [`WireTrace`].
#[derive(Debug, Default)]
struct TraceWindow {
    bytes: usize,
    prefix: Vec<u8>,
}

impl TraceWindow {
    /// Feed one chunk; return every frame completed by it, in order.
    fn push(&mut self, data: &[u8]) -> Vec<(usize, Vec<u8>)> {
        let mut done = Vec::new();
        for byte in data {
            if *byte == b'\n' {
                done.push((self.bytes, std::mem::take(&mut self.prefix)));
                self.bytes = 0;
            } else {
                self.bytes += 1;
                if self.prefix.len() < TRACE_PREFIX_BYTES {
                    self.prefix.push(*byte);
                }
            }
        }
        done
    }
}

fn over_cap(max_frame_bytes: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("protocol frame exceeded {max_frame_bytes} bytes"),
    )
}

/// Length of every frame in `data`, checked one byte at a time.
///
/// Byte-wise rather than "find the first newline" because a single read can carry several frames, and
/// a cap that only measures the first of them is a cap the peer can step over.
fn check_frames(data: &[u8], partial: &mut usize, max_frame_bytes: usize) -> Option<io::Error> {
    for byte in data.iter().copied() {
        if byte == b'\n' {
            if *partial > max_frame_bytes {
                return Some(over_cap(max_frame_bytes));
            }
            *partial = 0;
        } else {
            *partial += 1;
        }
    }
    if *partial > max_frame_bytes {
        return Some(over_cap(max_frame_bytes));
    }
    None
}

/// What a session owes the worker on its way out.
///
/// A `blocking::Unblock` **joins its helper thread when it drops**, and the read half's thread is parked
/// in `ReadFile` on the worker's stdout. So a transport dropped while the worker is alive-and-silent
/// joins a thread that only the very process it is waiting on can wake — and the join happens on the
/// runtime thread, inside `block_on`, where no deadline can be observed: `tokio::time::timeout_at`
/// cannot fire on a thread that is not polling. That is §38's "the session never settles": every frame
/// crossed, the agent answered its own request with `end_turn`, the loop built the report, and
/// `block_on` never returned.
///
/// A transport half that owns its handle behind this shared cell can be dropped by the SDK, by the
/// session's `select!`, or by a runtime being torn down, and joining nothing means each of those is
/// safe. The *cell* outlives the session and is dropped by [`crate::acp::run`] only once the worker it
/// was reading is provably gone, which is when the join stops being a wait.
///
/// `Mutex`, not `RefCell`: the handle has to cross into the runtime as `Send`, and on this
/// current-thread runtime exactly one task ever touches it, so the lock is never contended.
pub struct SharedPipe<R>(Arc<Mutex<R>>);

impl<R> SharedPipe<R> {
    /// The half to hand to the transport, and the keep-alive that decides when the real handle drops.
    pub fn share(inner: R) -> (Self, Arc<Mutex<R>>) {
        let cell = Arc::new(Mutex::new(inner));
        (Self(cell.clone()), cell)
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for SharedPipe<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // A poisoned lock means a previous poll panicked while holding the handle. Ending the stream is
        // the honest answer: the connection breaks, the session reports it, nothing blocks.
        let mut guard = self
            .0
            .lock()
            .map_err(|_| io::Error::other("the shared transport half was poisoned"))?;
        Pin::new(&mut *guard).poll_read(cx, buf)
    }
}

impl<R: AsyncWrite + Unpin> AsyncWrite for SharedPipe<R> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| io::Error::other("the shared transport half was poisoned"))?;
        Pin::new(&mut *guard).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| io::Error::other("the shared transport half was poisoned"))?;
        Pin::new(&mut *guard).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| io::Error::other("the shared transport half was poisoned"))?;
        Pin::new(&mut *guard).poll_close(cx)
    }
}

/// An `AsyncRead` that refuses a single line longer than `max_frame_bytes`.
pub struct BoundedRead<R> {
    inner: R,
    max_frame_bytes: usize,
    /// Bytes of the current, unfinished frame.
    partial: usize,
    counted: FrameCount,
    /// Set by the ACP path, whose reader is the SDK's: without a beat here, a worker streaming
    /// continuously would still look silent to the watchdog and be killed mid-sentence.
    heartbeat: Option<Heartbeat>,
    /// Frames traced out to `tracing`, when the operator asked for them.
    trace: Option<Arc<WireTrace>>,
    window: TraceWindow,
}

impl<R> BoundedRead<R> {
    pub fn new(inner: R, max_frame_bytes: usize, counted: FrameCount) -> Self {
        Self {
            inner,
            max_frame_bytes,
            partial: 0,
            counted,
            heartbeat: None,
            trace: None,
            window: TraceWindow::default(),
        }
    }

    #[must_use]
    pub fn with_heartbeat(mut self, heartbeat: Heartbeat) -> Self {
        self.heartbeat = Some(heartbeat);
        self
    }

    #[must_use]
    pub fn with_trace(mut self, trace: Arc<WireTrace>) -> Self {
        self.trace = Some(trace);
        self
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let read = ready!(Pin::new(&mut self.inner).poll_read(cx, buf))?;
        let cap = self.max_frame_bytes;
        self.counted.add(read as u64);
        if read > 0 {
            if let Some(heartbeat) = &self.heartbeat {
                heartbeat.beat();
            }
        }
        // Traced before the cap is checked, so a trace shows the frame that broke the session rather
        // than stopping silently at it. The bytes themselves never leave `window`.
        if let Some(trace) = self.trace.clone() {
            for (bytes, prefix) in self.window.push(&buf[..read]) {
                trace.record(Direction::In, bytes, &prefix);
            }
        }
        match check_frames(&buf[..read], &mut self.partial, cap) {
            Some(error) => Poll::Ready(Err(error)),
            None => Poll::Ready(Ok(read)),
        }
    }
}

/// An `AsyncWrite` that refuses to send a single line longer than `max_frame_bytes`.
///
/// Outgoing frames are built by the SDK from our own requests, so this is the less likely direction —
/// but the prompt is operator text of arbitrary length, and a cap that only guarded reads would let
/// one enormous prompt be written without ever being counted.
pub struct BoundedWrite<W> {
    inner: W,
    max_frame_bytes: usize,
    partial: usize,
    trace: Option<Arc<WireTrace>>,
    window: TraceWindow,
}

impl<W> BoundedWrite<W> {
    pub fn new(inner: W, max_frame_bytes: usize) -> Self {
        Self {
            inner,
            max_frame_bytes,
            partial: 0,
            trace: None,
            window: TraceWindow::default(),
        }
    }

    #[must_use]
    pub fn with_trace(mut self, trace: Arc<WireTrace>) -> Self {
        self.trace = Some(trace);
        self
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for BoundedWrite<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let written = ready!(Pin::new(&mut self.inner).poll_write(cx, buf))?;
        let cap = self.max_frame_bytes;
        if let Some(trace) = self.trace.clone() {
            for (bytes, prefix) in self.window.push(&buf[..written]) {
                trace.record(Direction::Out, bytes, &prefix);
            }
        }
        // `buf` is the caller's, `self` is ours: the slice and the counter never borrow the same thing.
        let overflow = check_frames(&buf[..written], &mut self.partial, cap);
        match overflow {
            Some(error) => Poll::Ready(Err(error)),
            None => Poll::Ready(Ok(written)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// Synchronous NDJSON reading for the CLI connector.
pub struct LineReader<'a> {
    child: &'a PipedChild,
    buffer: Vec<u8>,
    scratch: Vec<u8>,
    max_frame_bytes: usize,
    counted: FrameCount,
    heartbeat: Heartbeat,
}

impl<'a> LineReader<'a> {
    pub fn new(
        child: &'a PipedChild,
        max_frame_bytes: usize,
        counted: FrameCount,
        heartbeat: Heartbeat,
    ) -> Self {
        Self {
            child,
            buffer: Vec::with_capacity(8 * 1024),
            scratch: vec![0u8; jean_process::win32::READ_CHUNK],
            max_frame_bytes,
            counted,
            heartbeat,
        }
    }

    /// The next complete line, or `None` at end of stream.
    ///
    /// A peer that dies mid-frame leaves an unterminated tail, which is returned as a line: a
    /// one-way stream cannot say "this was complete", and the caller decides whether the partial
    /// record means anything. The ACP path does not use this function.
    pub fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            if let Some(at) = self.buffer.iter().position(|b| *b == b'\n') {
                let line = self.buffer.drain(..=at).collect::<Vec<u8>>();
                if line.len() - 1 > self.max_frame_bytes {
                    return Err(over_cap(self.max_frame_bytes));
                }
                return Ok(Some(self.decode(trim_end(&line))?));
            }
            match self.child.read_stdout(&mut self.scratch) {
                PipeRead::Data(n) => {
                    self.counted.add(n as u64);
                    self.heartbeat.beat();
                    self.buffer.extend_from_slice(&self.scratch[..n]);
                    // Only an unfinished frame may not exceed the cap. A buffer full of complete
                    // frames is a fast agent, and the loop above drains them one at a time.
                    if self.buffer.len() > self.max_frame_bytes && !self.buffer.contains(&b'\n') {
                        return Err(over_cap(self.max_frame_bytes));
                    }
                }
                PipeRead::Eof => {
                    if self.buffer.is_empty() {
                        return Ok(None);
                    }
                    let rest = std::mem::take(&mut self.buffer);
                    return Ok(Some(self.decode(trim_end(&rest))?));
                }
                PipeRead::Failed(reason) => {
                    return Err(io::Error::other(reason));
                }
            }
        }
    }

    fn decode(&self, bytes: Vec<u8>) -> io::Result<String> {
        String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol frame was not valid UTF-8",
            )
        })
    }
}

fn trim_end(bytes: &[u8]) -> Vec<u8> {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\r' | b'\n') {
        end -= 1;
    }
    bytes[..end].to_vec()
}

/// Seconds of silence, measured from one clock.
///
/// A worker that writes nothing is indistinguishable from a worker that is thinking, and the reader
/// cannot notice because it is parked in a blocking pipe read. So the reader reports activity *here*
/// and the watchdog reads it: no lock, no waker, no second timer to keep in step with the first.
#[derive(Clone, Debug)]
pub struct Heartbeat {
    started: Arc<(Instant, AtomicU64)>,
}

impl Heartbeat {
    pub fn new() -> Self {
        Self {
            started: Arc::new((Instant::now(), AtomicU64::new(0))),
        }
    }

    /// Called by the reader whenever bytes arrive.
    pub fn beat(&self) {
        let elapsed = self.started.0.elapsed().as_millis() as u64;
        self.started.1.store(elapsed, Ordering::Relaxed);
    }

    /// How long the worker has said nothing.
    ///
    /// Milliseconds, not seconds: an idle timeout is configured in seconds, but a test and a
    /// sub-second grace both need the measurement to be finer than the setting.
    pub fn silent_for(&self) -> std::time::Duration {
        let now = self.started.0.elapsed().as_millis() as u64;
        let last = self.started.1.load(Ordering::Relaxed);
        std::time::Duration::from_millis(now.saturating_sub(last))
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::io::Cursor;
    use futures::AsyncReadExt as _;

    /// Read `want` bytes through the *extension* method, which is what a `BufReader` would call.
    fn read_all<R: AsyncRead + Unpin>(reader: &mut R, want: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 16];
        while out.len() < want {
            match block_on(reader.read(&mut chunk)) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
            }
        }
        out
    }

    #[test]
    fn a_frame_under_the_cap_passes_through_untouched() {
        let counted = FrameCount::new();
        let mut reader = BoundedRead::new(
            Cursor::new(b"{\"a\":1}\n{\"b\":2}\n".to_vec()),
            64,
            counted.clone(),
        );
        let got = read_all(&mut reader, 16);
        assert_eq!(&got, b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(counted.get(), 16);
    }

    #[test]
    fn a_frame_over_the_cap_fails_rather_than_buffering() {
        let counted = FrameCount::new();
        let payload = vec![b'x'; 4096];
        let mut reader = BoundedRead::new(Cursor::new(payload), 1024, counted.clone());
        let mut chunk = [0u8; 4096];
        let error = block_on(reader.read(&mut chunk)).expect_err("over-cap must error");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("1024"), "{error}");
        // The bytes still count: a truncated stream is traffic, and the report says how much.
        assert!(counted.get() >= 1024);
    }

    #[test]
    fn the_counter_resets_at_every_newline_so_a_long_session_is_fine() {
        let counted = FrameCount::new();
        let mut stream = Vec::new();
        for _ in 0..500 {
            stream.extend_from_slice(b"{\"kind\":\"token\",\"text\":\"hello\"}\n");
        }
        let mut reader = BoundedRead::new(Cursor::new(stream), 128, counted.clone());
        let total = 500 * 32;
        let got = read_all(&mut reader, total);
        assert_eq!(got.len(), total);
        assert_eq!(counted.get(), total as u64);
    }

    #[test]
    fn every_frame_in_a_multi_frame_read_is_measured_not_just_the_first() {
        // A reader that hands over three frames at once must not let a huge second frame step over
        // the cap that the first one satisfied.
        let data = [
            vec![b'a'; 8],
            b"\n".to_vec(),
            vec![b'b'; 200],
            b"\n".to_vec(),
        ]
        .concat();
        let mut partial = 0usize;
        let error = check_frames(&data, &mut partial, 64).expect("second frame is over cap");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_write_over_the_cap_is_refused_too() {
        use futures::AsyncWriteExt as _;
        let mut writer = BoundedWrite::new(Vec::<u8>::new(), 8);
        let big = vec![b'y'; 40];
        let error = block_on(writer.write_all(&big)).expect_err("over-cap write");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn trimmed_lines_drop_their_framing_and_a_carriage_return_too() {
        assert_eq!(trim_end(b"{\"a\":1}\r\n"), b"{\"a\":1}");
        assert_eq!(trim_end(b"plain"), b"plain");
    }

    /// The window holds a label, never a payload: a 5 KiB body contributes the first 120 bytes, which
    /// is where `method` and `id` live and where nothing of the agent's work is.
    #[test]
    fn the_trace_window_stops_at_the_prefix_and_holds_nothing_after_it() {
        let mut window = TraceWindow::default();
        let mut frame =
            br#"{"jsonrpc":"2.0","id":7,"method":"fs/write_text_file","params":{"content":""#
                .to_vec();
        frame.extend_from_slice(&vec![b'x'; 5 * 1024]);
        frame.extend_from_slice(b"the-agent's-file-body");
        frame.push(b'"');
        frame.push(b'\n');
        let done = window.push(&frame);
        assert_eq!(done.len(), 1, "one frame, terminated by one newline");
        let (bytes, prefix) = &done[0];
        assert_eq!(
            *bytes,
            frame.len() - 1,
            "the length is reported; the bytes are not"
        );
        assert_eq!(prefix.len(), TRACE_PREFIX_BYTES);
        assert_eq!(
            field_of(&String::from_utf8_lossy(prefix), "method").as_deref(),
            Some("fs/write_text_file")
        );
        assert!(
            !String::from_utf8_lossy(prefix).contains("the-agent"),
            "the body leaked into the trace window"
        );
    }

    #[test]
    fn a_field_outside_the_traced_prefix_reports_nothing_rather_than_guessing() {
        let line = "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"a\":1,\"id\":";
        assert_eq!(field_of(line, "method").as_deref(), Some("session/update"));
        assert_eq!(field_of(line, "id"), None, "no value has arrived yet");
        assert_eq!(field_of("{\"id\":42}", "id").as_deref(), Some("42"));
    }

    #[test]
    fn a_heartbeat_measures_silence_from_the_first_beat_onward() {
        let beat = Heartbeat::new();
        assert!(beat.silent_for().as_secs() < 2);
        beat.beat();
        assert_eq!(beat.silent_for(), std::time::Duration::from_secs(0));
    }

    /// The §38 deadlock, with real pipes and no agent.
    ///
    /// A duplex stream cannot reproduce it — dropping one joins nothing, which is exactly why the
    /// scripted tests passed while the live fixture hung for 221 s. This uses an OS pipe, whose read
    /// parks a thread the way `ReadFile` on a worker's stdout does, and asserts the two things the fix
    /// guarantees: the transport half can be dropped while the peer is alive, and only the owner's
    /// keep-alive may ever wait for that thread.
    #[test]
    fn dropping_a_transport_half_does_not_join_a_thread_parked_on_a_live_pipe() {
        use futures::AsyncReadExt as _;
        use std::time::Duration;

        let (reader, writer): (std::io::PipeReader, std::io::PipeWriter) =
            std::io::pipe().expect("an anonymous pipe");
        let (mut half, keep_alive) = SharedPipe::share(blocking::Unblock::new(reader));
        let (ended_tx, ended_rx) = std::sync::mpsc::channel();

        // The session's thread: park a read on a peer that never answers, then end the transport the way
        // a settled — or abandoned — session ends it, from inside the runtime.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("a current-thread runtime");
            runtime.block_on(async move {
                let mut buf = [0u8; 8];
                let _ = tokio::time::timeout(Duration::from_millis(300), half.read(&mut buf)).await;
                drop(half);
            });
            // The runtime goes too, in case a task still holds a half of the transport.
            drop(runtime);
            let _ = ended_tx.send(());
        });

        // Before `SharedPipe` this is where the process stopped: the drop joined the parked thread, the
        // join needed the peer to close, and no deadline could fire on a thread that was not polling.
        ended_rx.recv_timeout(Duration::from_secs(10)).expect(
            "the session thread never returned: dropping a transport half joined a live read",
        );

        // The owner keeps the handle until the worker is gone. Closing the peer is what the terminate
        // does in production; here it is the same fact, that nothing is writing any more.
        drop(writer);
        let joined = std::time::Instant::now();
        drop(keep_alive);
        assert!(
            joined.elapsed() < Duration::from_secs(10),
            "the keep-alive's join took {}",
            joined.elapsed().as_secs_f32()
        );
    }
}
