//! Bounded output capture.
//!
//! ## The two rules
//!
//! **1. The cap is a hard ceiling on what is retained, not on what is read.**
//! A child that writes 10 GB must not fill the disk, but it also must not
//! deadlock. If the supervisor stopped reading once the cap was reached, the
//! child would block on a full pipe forever and the task would hang until its
//! timeout — a hang caused by J.E.A.N.'s own bookkeeping. So the reader keeps
//! draining the pipe to EOF and simply discards everything past the cap.
//!
//! **2. Losing output must be visible.** Silent truncation is indistinguishable
//! from a child that genuinely produced little output. When output is dropped,
//! a marker is written at the cut point and a second marker at the end carries
//! the true byte count, so a reader of the artifact can tell that the tail is
//! missing and how much.
//!
//! ## What the cap counts
//!
//! The documented per-stream cap (10 MB by default) bounds the *artifact file*,
//! markers included. Payload retention stops early enough that both markers
//! still fit. That makes the invariant checkable with one assertion —
//! `file size <= cap` — instead of a fuzzy "approximately".

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use jean_protocol::StreamSummary;

use crate::error::{ProcessError, Result};

/// Bytes held back for the two truncation markers.
///
/// Generous on purpose: the markers are formatted with byte counts that can be
/// up to 20 digits, and running out of room mid-marker would break the one
/// invariant this module promises.
const MARKER_RESERVE: u64 = 512;

/// Written at the cut point.
fn cut_marker(cap: u64) -> String {
    format!(
        "\n[JEAN: output truncated here. This stream's cap is {cap} bytes; \
         everything the child wrote past this point was drained and discarded.]\n"
    )
}

/// Appended at the end, once the true total is known.
fn total_marker(produced: u64, retained: u64) -> String {
    format!(
        "[JEAN: the child produced {produced} bytes on this stream; \
         {retained} were retained before the cap.]\n"
    )
}

/// Where captured bytes go.
#[derive(Debug)]
enum Sink {
    /// Retain nothing, count everything. Used when `process.capture_output` is
    /// off or when a caller wants the byte counts without the disk cost.
    Discard,
    /// Append to a file, stopping at the payload budget.
    File { file: File, path: PathBuf },
}

/// Collects one stream under a byte cap.
///
/// One instance per stream, owned by that stream's reader thread. Not `Sync`:
/// the design deliberately gives each pipe a single owner so no locking is
/// needed on the hot path.
#[derive(Debug)]
pub struct BoundedCollector {
    stream: &'static str,
    cap: u64,
    /// Bytes the child actually produced, retained or not.
    produced: u64,
    /// Bytes written to the sink, markers included.
    retained: u64,
    truncated: bool,
    sink: Sink,
    /// Set when a write to the sink failed. Capture is best-effort: a full disk
    /// must not kill the child, so the error is recorded and reported in the
    /// outcome instead of aborting the read loop.
    error: Option<String>,
}

impl BoundedCollector {
    /// Retain nothing; count everything.
    pub fn discard(stream: &'static str, cap: u64) -> Self {
        Self {
            stream,
            cap,
            produced: 0,
            retained: 0,
            truncated: false,
            sink: Sink::Discard,
            error: None,
        }
    }

    /// Capture to `path`, creating parent directories and truncating any
    /// previous content.
    pub fn to_file(stream: &'static str, path: &Path, cap: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| capture_error(stream, path, e))?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| capture_error(stream, path, e))?;
        Ok(Self {
            stream,
            cap,
            produced: 0,
            retained: 0,
            truncated: false,
            sink: Sink::File {
                file,
                path: path.to_path_buf(),
            },
            error: None,
        })
    }

    /// Bytes of payload that may be retained before the markers must fit.
    fn payload_budget(&self) -> u64 {
        // `cap / 2` keeps a tiny cap usable instead of leaving a zero budget:
        // with cap = 64 the reserve alone would consume everything, and a test
        // that wants to observe truncation needs *some* retained prefix.
        self.cap
            .saturating_sub(2 * MARKER_RESERVE)
            .max(self.cap / 2)
    }

    /// Feed a chunk read from the pipe.
    ///
    /// Never fails. A sink error is captured and reported through
    /// [`BoundedCollector::finish`] so that a disk problem cannot turn into a
    /// hung or leaked child.
    pub fn push(&mut self, bytes: &[u8]) {
        self.produced += bytes.len() as u64;

        if matches!(self.sink, Sink::Discard) {
            return;
        }

        if self.retained >= self.payload_budget() {
            if !self.truncated {
                self.truncated = true;
                self.write_marker(cut_marker(self.cap));
            }
            // Keep counting; the caller keeps draining. Nothing is retained.
            return;
        }

        let room = self.payload_budget() - self.retained;
        if (bytes.len() as u64) <= room {
            self.write(bytes);
            return;
        }

        // This chunk straddles the cap: keep the prefix that fits, then cut.
        let split = room as usize;
        self.write(&bytes[..split]);
        self.truncated = true;
        self.write_marker(cut_marker(self.cap));
    }

    fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // The failure is carried out of the match instead of being reported
        // inside it: `record_error` takes `&mut self`, and the borrow of
        // `self.sink` held by the match arm would still be live there.
        let mut written = 0usize;
        let mut failure: Option<(PathBuf, std::io::Error)> = None;
        match &mut self.sink {
            Sink::Discard => {}
            Sink::File { file, path } => match file.write_all(bytes) {
                Ok(()) => written = bytes.len(),
                Err(e) => failure = Some((path.clone(), e)),
            },
        }
        self.retained += written as u64;
        if let Some((path, e)) = failure {
            self.record_error(&path, e);
        }
    }

    fn write_marker(&mut self, marker: String) {
        let len = marker.len() as u64;
        // The cap bounds the artifact file, markers included. If the cap is
        // smaller than a marker, skip the marker rather than break that
        // invariant: `truncated` and the true byte count in the task record
        // still report the loss, so nothing is silently dropped. This only bites
        // for a pathologically small configured cap.
        if self.retained + len > self.cap {
            tracing::warn!(
                stream = self.stream,
                cap = self.cap,
                marker_bytes = len,
                "the output cap is too small to fit a truncation marker; \
                 the artifact is unmarked but the task record still reports truncation"
            );
            return;
        }
        let mut written = 0usize;
        let mut failure: Option<(PathBuf, std::io::Error)> = None;
        match &mut self.sink {
            Sink::Discard => {}
            Sink::File { file, path } => match file.write_all(marker.as_bytes()) {
                Ok(()) => written = marker.len(),
                Err(e) => failure = Some((path.clone(), e)),
            },
        }
        self.retained += written as u64;
        if let Some((path, e)) = failure {
            self.record_error(&path, e);
        }
    }

    fn record_error(&mut self, path: &Path, e: std::io::Error) {
        // Only the first error is kept: a full disk reports the same failure on
        // every subsequent write, and a log full of repeats hides the real one.
        if self.error.is_none() {
            self.error = Some(capture_error(self.stream, path, e).to_string());
        }
        // Stop trying. Switching to Discard keeps `produced` accurate while
        // ending the flood of failing writes.
        self.sink = Sink::Discard;
    }

    /// Record a failure that happened while *reading* the stream.
    ///
    /// Kept separate from a sink failure because the causes and fixes differ:
    /// this one means the pipe could not be read, so the byte count is a lower
    /// bound rather than a total.
    pub fn note_error(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }

    /// Close the artifact and report what happened.
    pub fn finish(mut self) -> (StreamSummary, Option<String>) {
        if self.truncated {
            let marker = total_marker(self.produced, self.retained);
            self.write_marker(marker);
        }

        let (path, error) = match self.sink {
            Sink::File { mut file, path } => {
                let flush_error = file
                    .flush()
                    .err()
                    .map(|e| capture_error(self.stream, &path, e));
                (Some(path.display().to_string()), flush_error)
            }
            Sink::Discard => (None, None),
        };

        // A sink failure outranks a flush failure: the sink error is the one that
        // explains why bytes are missing.
        let error = self.error.or(error.map(|e| e.to_string()));
        if let Some(text) = &error {
            tracing::warn!(stream = self.stream, error = %text, "output capture failed partway");
        }

        (
            StreamSummary {
                path,
                bytes: self.produced,
                truncated: self.truncated,
            },
            error,
        )
    }

    /// Bytes the child produced, retained or not.
    pub fn produced(&self) -> u64 {
        self.produced
    }

    /// Bytes written to the artifact so far, markers included.
    pub fn retained(&self) -> u64 {
        self.retained
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn stream(&self) -> &'static str {
        self.stream
    }
}

fn capture_error(stream: &'static str, path: &Path, e: std::io::Error) -> ProcessError {
    ProcessError::Capture {
        stream,
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "jean-output-test-{}-{}-{}.log",
            tag,
            std::process::id(),
            n
        ))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// Feed `bytes` in `chunk`-sized pieces, the way a pipe reader would.
    fn feed(collector: &mut BoundedCollector, bytes: &[u8], chunk: usize) {
        for c in bytes.chunks(chunk.max(1)) {
            collector.push(c);
        }
    }

    #[test]
    fn output_under_the_cap_is_kept_whole() {
        let path = temp_path("under");
        let mut c = BoundedCollector::to_file("stdout", &path, 10_000).unwrap();
        feed(&mut c, b"hello world", 4);
        let (summary, error) = c.finish();

        assert!(error.is_none(), "{error:?}");
        assert!(!summary.truncated);
        assert_eq!(summary.bytes, 11);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        cleanup(&path);
    }

    #[test]
    fn output_over_the_cap_is_truncated_with_a_marker() {
        let path = temp_path("over");
        let cap = 4_096u64;
        let payload: Vec<u8> = (0..cap * 3).map(|i| b'a' + (i % 26) as u8).collect();

        let mut c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
        feed(&mut c, &payload, 512);
        let (summary, error) = c.finish();

        assert!(error.is_none(), "{error:?}");
        assert!(summary.truncated);
        assert_eq!(
            summary.bytes,
            payload.len() as u64,
            "the byte count must report what the child produced, not what was kept"
        );

        let kept = std::fs::read(&path).unwrap();
        assert!(
            (kept.len() as u64) <= cap,
            "the artifact must fit inside the documented cap: {} > {cap}",
            kept.len()
        );
        let text = String::from_utf8_lossy(&kept);
        assert!(
            text.contains("output truncated here"),
            "the cut must be announced at the cut point: {text}"
        );
        assert!(
            text.contains(&format!("the child produced {} bytes", payload.len())),
            "the tail must report the true total: {text}"
        );
        cleanup(&path);
    }

    #[test]
    fn the_retained_prefix_is_the_head_of_the_output() {
        let path = temp_path("prefix");
        let cap = 2_048u64;
        let payload: Vec<u8> = (0..cap * 2).map(|_| b'x').collect();

        let mut c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
        feed(&mut c, &payload, 128);
        let _ = c.finish();

        let kept = std::fs::read(&path).unwrap();
        let head_len = kept.len().min(64);
        assert_eq!(&kept[..head_len], &payload[..head_len]);
        cleanup(&path);
    }

    #[test]
    fn excess_output_is_still_drained_after_the_cut() {
        // The failure this guards against: stopping the read at the cap would
        // fill the pipe, block the child on its own write, and hang the task
        // until its timeout. Draining is what keeps a chatty child cheap.
        let path = temp_path("drain");
        let cap = 1_024u64;
        let payload = vec![b'z'; (cap * 200) as usize];

        let mut c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
        feed(&mut c, &payload, 4_096);
        let (summary, _) = c.finish();

        assert_eq!(
            summary.bytes,
            payload.len() as u64,
            "every byte was counted"
        );
        assert!(summary.truncated);
        assert!((std::fs::metadata(&path).unwrap().len()) <= cap);
        cleanup(&path);
    }

    #[test]
    fn a_chunk_that_straddles_the_cap_is_split_not_dropped() {
        let path = temp_path("straddle");
        let cap = 2_048u64;
        // One huge chunk: nothing before it, so the split must happen inside it.
        let payload = vec![b'q'; (cap * 4) as usize];

        let mut c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
        c.push(&payload);
        let (summary, _) = c.finish();

        assert!(summary.truncated);
        assert_eq!(summary.bytes, payload.len() as u64);
        let kept = std::fs::read(&path).unwrap();
        assert!(kept.starts_with(&[b'q'; 64]));
        assert!((kept.len() as u64) <= cap);
        cleanup(&path);
    }

    #[test]
    fn output_exactly_at_the_cap_is_not_marked_truncated() {
        let path = temp_path("exact");
        let cap = 4_096u64;
        let budget = {
            let c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
            c.payload_budget()
        };
        let payload = vec![b'e'; budget as usize];

        let mut c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
        feed(&mut c, &payload, 100);
        let (summary, _) = c.finish();

        assert!(
            !summary.truncated,
            "filling the payload budget exactly is not truncation"
        );
        assert_eq!(summary.bytes, payload.len() as u64);
        cleanup(&path);
    }

    #[test]
    fn one_byte_past_the_budget_triggers_the_cut() {
        let path = temp_path("oneover");
        let cap = 4_096u64;
        let budget = {
            let c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
            c.payload_budget()
        };

        let mut c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
        feed(&mut c, &vec![b'e'; (budget as usize) + 1], 1);
        let (summary, _) = c.finish();
        assert!(summary.truncated);
        assert_eq!(summary.bytes, budget + 1);
        cleanup(&path);
    }

    #[test]
    fn a_tiny_cap_still_retains_something_and_still_fits() {
        // `payload_budget` floors at `cap / 2` precisely so a small configured
        // cap degrades instead of retaining nothing at all.
        let path = temp_path("tiny");
        let cap = 96u64;
        let payload = vec![b't'; 4_000];

        let mut c = BoundedCollector::to_file("stdout", &path, cap).unwrap();
        feed(&mut c, &payload, 7);
        let (summary, _) = c.finish();

        assert!(summary.truncated);
        let kept = std::fs::read(&path).unwrap();
        assert!((kept.len() as u64) <= cap, "{} > {cap}", kept.len());
        assert!(kept.starts_with(b"tttt"), "the head was retained: {kept:?}");
        cleanup(&path);
    }

    #[test]
    fn an_empty_stream_produces_an_empty_artifact() {
        let path = temp_path("empty");
        let c = BoundedCollector::to_file("stderr", &path, 1_024).unwrap();
        let (summary, error) = c.finish();
        assert!(error.is_none());
        assert_eq!(summary.bytes, 0);
        assert!(!summary.truncated);
        assert!(std::fs::read(&path).unwrap().is_empty());
        cleanup(&path);
    }

    #[test]
    fn discard_mode_counts_without_writing() {
        let mut c = BoundedCollector::discard("stdout", 16);
        feed(&mut c, &vec![b'd'; 10_000], 64);
        let (summary, error) = c.finish();

        assert!(error.is_none());
        assert_eq!(summary.bytes, 10_000, "counts are still exact");
        assert!(
            !summary.truncated,
            "nothing was retained, so nothing was truncated"
        );
        assert!(summary.path.is_none(), "no artifact was created");
    }

    #[test]
    fn parent_directories_are_created() {
        let base = temp_path("nested");
        let nested = base.join("a").join("b").join("stdout.log");
        let mut c = BoundedCollector::to_file("stdout", &nested, 1_024)
            .unwrap_or_else(|e| panic!("creating {nested:?} failed: {e}"));
        c.push(b"nested");
        let (summary, error) = c.finish();
        assert!(error.is_none());
        assert_eq!(summary.bytes, 6);
        assert_eq!(std::fs::read(&nested).unwrap(), b"nested");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reopening_a_path_truncates_the_previous_run() {
        let path = temp_path("reuse");
        std::fs::write(&path, vec![b'x'; 5_000]).unwrap();
        let mut c = BoundedCollector::to_file("stdout", &path, 4_096).unwrap();
        c.push(b"new");
        let _ = c.finish();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        cleanup(&path);
    }

    #[test]
    fn a_write_failure_is_reported_instead_of_aborting_the_read() {
        // Point the sink at a directory: opening it for writing fails on every
        // supported platform, which is the cheapest way to exercise the error
        // path without a full disk.
        let dir = temp_path("dir-as-file");
        std::fs::create_dir_all(&dir).unwrap();

        let err = BoundedCollector::to_file("stdout", &dir, 1_024).unwrap_err();
        match err {
            ProcessError::Capture {
                stream,
                path,
                reason,
            } => {
                assert_eq!(stream, "stdout");
                assert_eq!(path, dir);
                assert!(!reason.is_empty());
            }
            other => panic!("expected Capture, got {other:?}"),
        }
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn an_io_failure_mid_stream_degrades_to_counting() {
        // Simulated by dropping the sink into Discard the way `record_error`
        // does, then confirming the counters stay honest. This is the behaviour
        // contract: capture is best-effort, supervision is not.
        let path = temp_path("midstream");
        let mut c = BoundedCollector::to_file("stdout", &path, 1_024).unwrap();
        c.push(b"before");
        c.sink = Sink::Discard;
        c.error = Some("disk full".to_owned());
        c.push(b"after");
        let (summary, error) = c.finish();

        assert_eq!(summary.bytes, 11, "both pushes were counted");
        assert_eq!(error.as_deref(), Some("disk full"));
        cleanup(&path);
    }

    #[test]
    fn a_read_failure_is_reported_and_only_the_first_one_is_kept() {
        let mut c = BoundedCollector::discard("stdout", 1_024);
        c.push(b"some output");
        c.note_error("The pipe has been ended. (os error 109)".to_owned());
        c.note_error("a second, less interesting failure".to_owned());
        let (summary, error) = c.finish();

        assert_eq!(summary.bytes, 11, "what was read is still counted exactly");
        assert_eq!(
            error.as_deref(),
            Some("The pipe has been ended. (os error 109)"),
            "a repeated failure must not overwrite the first one"
        );
    }

    #[test]
    fn the_stream_name_is_available_for_events() {
        let c = BoundedCollector::discard("stderr", 8);
        assert_eq!(c.stream(), "stderr");
        assert_eq!(c.produced(), 0);
        assert_eq!(c.retained(), 0);
    }
}
