//! The structured CLI connector: `codex exec --json`.
//!
//! ## Why this exists next to a real protocol
//!
//! Codex 0.160.1, the version installed here, has no `acp` subcommand (`codex --help` lists `exec`,
//! `review`, `login`, `mcp`, …), and the ACP adapter for it is a separate npm package that is not
//! installed. So the structured interface this build can actually speak is its machine-readable event
//! stream: `--json` prints one JSON object per line on stdout, `-` reads the prompt from stdin, and
//! `-o FILE` writes the agent's final message where JEAN can find it. That is a *fallback*, and it is
//! explicit about being one.
//!
//! ## What a CLI connector cannot do
//!
//! * **Ask permission.** Nobody is listening: the stream is one-way. What the agent may do is decided
//!   *before* it starts — by the sandbox JEAN passes (`--sandbox read-only` or `workspace-write`, the
//!   only two values `[delegation]` accepts) and by the working root JEAN gave it. This connector
//!   therefore never emits [`WorkerEvent::PermissionAsked`]: an event it could not answer would be a
//!   lie about a control that does not exist.
//! * **Be cancelled politely.** There is no `session/cancel`. The watchdog closes the job, the job
//!   tree dies with it, and the exit code says so.
//! * **Report a stop reason it means.** The turn's end is inferred from `turn.completed` /
//!   `turn.failed`; a stream that simply stops is a failure, not an end turn.
//!
//! ## What it does guarantee
//!
//! Frames are capped and counted, diagnostics go to a file rather than into the protocol stream, an
//! unparsable line is reported instead of silently dropped, and the agent's own final message is
//! stored as a *claim* that no part of this build reads as a verdict.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::contract::{
    EventSink, RunSpec, WorkerEvent, WorkerOutcome, STOP_CANCELLED, STOP_END_TURN, STOP_FAILED,
    STOP_TIMEOUT,
};
use crate::error::{Result, WorkerError};
use crate::frames::{FrameCount, Heartbeat, LineReader};
use crate::watchdog::{self, Signal, Watchdog};

/// How long to wait for the process to leave after its stdout reached end-of-stream.
///
/// A child that closes its own output and then runs for another hour is a bug in the child, but the
/// exit code is worth a short wait: `codex exec` flushes its events and exits within milliseconds.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Run one `codex exec --json` worker to settlement.
pub fn run(spec: &RunSpec<'_>, sink: &mut dyn EventSink) -> Result<WorkerOutcome> {
    let bounds = spec.bounds;
    let counted = FrameCount::new();
    let heartbeat = Heartbeat::new();

    let mut launch = spec.launch.clone();
    launch.args = protocol_args(&launch.args, bounds.output_dir.as_deref());
    let child = jean_process::piped::spawn(&launch).map_err(WorkerError::Process)?;
    let control = child.control();

    let started = WorkerEvent::Started {
        worker: spec.worker.name.clone(),
        kind: spec.worker.kind.clone(),
        pid: control.pid(),
    };
    let mut events = 1usize;
    if !sink.emit(started) {
        let _ = control.terminate();
        return Ok(settled(
            STOP_FAILED.to_owned(),
            None,
            None,
            0,
            &counted,
            bounds.output_dir.clone(),
            Some("the event sink closed before the worker started".to_owned()),
        ));
    }

    // Diagnostics are drained on their own thread into a file, never merged into the protocol
    // stream: a worker that floods stderr must not be able to look like a frame.
    let stderr = stderr_capture(&child, bounds)?;

    let (tx, mut rx) = watchdog::session_channel();
    let watchdog = Watchdog::arm(
        control.clone(),
        spec.cancel.clone(),
        bounds.session_deadline,
        bounds.idle_timeout,
        heartbeat.clone(),
        bounds.shutdown_grace,
        Some(tx),
    );

    // The prompt is stdin, and closing stdin is what tells `codex exec -` that it has all of it.
    let mut failure: Option<String> = None;
    if let Err(e) = child
        .write_line(spec.request.prompt.as_bytes())
        .and_then(|()| child.close_stdin())
    {
        failure = Some(format!("could not hand the prompt to the worker: {e}"));
        let _ = control.terminate();
    }

    let mut reader = LineReader::new(&child, bounds.max_frame_bytes, counted.clone(), heartbeat);
    let mut stream = StreamState::default();

    while failure.is_none() {
        if spec.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = control.terminate();
            break;
        }
        match reader.next_line() {
            Ok(None) => break,
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(record) => {
                        stream.observe(&record, spec.worker.name.as_str());
                        for (source, text) in stream.drain() {
                            events += 1;
                            let event = WorkerEvent::Progress {
                                seq: events as u64,
                                source,
                                text,
                            };
                            if !sink.emit(event) {
                                failure =
                                    Some("the event queue was full, so reading stopped".to_owned());
                                let _ = control.terminate();
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        // An unparsable line is reported, not fatal. `codex exec` prints human
                        // banners before its first event when the model is not configured, and a
                        // connector that treated a banner as a protocol breach would hide the actual
                        // reason the run produced nothing.
                        stream.malformed += 1;
                        events += 1;
                        let _ = sink.emit(WorkerEvent::Progress {
                            seq: events as u64,
                            source: "malformed".to_owned(),
                            text: format!("unparsable event line ({e}): {}", clip(&line, 200)),
                        });
                    }
                }
            }
            Err(e) => {
                failure = Some(format!("worker stream failed: {e}"));
                let _ = control.terminate();
            }
        }
    }

    let exit_code = wait_for_exit(&control);
    watchdog.stop();
    join_capture(stderr);

    let cancelled = spec.cancel.load(std::sync::atomic::Ordering::Relaxed);
    // The watchdog knows why it ended the session and the stream usually does not, so its signal is
    // the one reported: "cancelled", "timed out" and "went silent" are three different facts for the
    // operator and none of them can be recovered once the process is gone.
    let stop_reason = match (cancelled, rx.try_recv().ok()) {
        (true, _) => STOP_CANCELLED.to_owned(),
        (false, Some(Signal::Cancelled)) => STOP_CANCELLED.to_owned(),
        (false, Some(Signal::TimedOut)) => STOP_TIMEOUT.to_owned(),
        (false, Some(Signal::Idle)) => {
            failure
                .get_or_insert_with(|| "the worker went silent past its idle timeout".to_owned());
            STOP_FAILED.to_owned()
        }
        (false, None) => stream.stop_reason.clone().unwrap_or_else(|| {
            failure.get_or_insert_with(|| {
                let malformed = if stream.malformed > 0 {
                    format!(", {} line(s) unparsable", stream.malformed)
                } else {
                    String::new()
                };
                format!("the event stream ended without turn.completed or turn.failed{malformed}")
            });
            STOP_FAILED.to_owned()
        }),
    };

    let claim = read_claim_file(bounds.output_dir.as_deref()).or_else(|| stream.claim.clone());

    Ok(settled(
        stop_reason,
        claim,
        exit_code,
        events,
        &counted,
        bounds.output_dir.clone(),
        failure,
    ))
}

/// Flags the connector itself requires, appended to the operator's configured arguments.
///
/// Only protocol flags, and only when absent, so an operator who already wrote `--json` into
/// `[delegation] workers.args` does not get it twice. Everything that carries authority — sandbox,
/// model, working root — stays in the configuration, which the worker cannot rewrite.
fn protocol_args(configured: &[String], output_dir: Option<&std::path::Path>) -> Vec<String> {
    // Decided up front: borrowing `args` to look for a flag and pushing into it in the same statement
    // is the borrow checker's complaint, and reading the list once is also the clearer code.
    let mut args: Vec<String> = configured.to_vec();
    let has_json = args.iter().any(|a| a == "--json");
    let has_claim_file = args
        .iter()
        .any(|a| a == "-o" || a == "--output-last-message");
    let has_stdin_prompt = args.iter().any(|a| a == "-");
    if !has_json {
        args.push("--json".to_owned());
    }
    if let Some(dir) = output_dir {
        if !has_claim_file {
            let path = dir.join("last-message.txt");
            args.push("-o".to_owned());
            args.push(path.to_string_lossy().into_owned());
        }
    }
    // `-` is codex's own spelling for "read the instructions from stdin".
    if !has_stdin_prompt {
        args.push("-".to_owned());
    }
    args
}

#[allow(clippy::too_many_arguments)]
fn settled(
    stop_reason: String,
    claim: Option<String>,
    exit_code: Option<i32>,
    events: usize,
    counted: &FrameCount,
    output_dir: Option<PathBuf>,
    failure: Option<String>,
) -> WorkerOutcome {
    WorkerOutcome {
        stop_reason,
        claim,
        exit_code,
        events,
        stdout_bytes: counted.get(),
        output_dir,
        failure,
        failure_kind: None,
        remaining_work: None,
    }
}

fn wait_for_exit(control: &jean_process::piped::Control) -> Option<i32> {
    let until = Instant::now() + EXIT_GRACE;
    loop {
        if let Some(code) = control.exit_code_if_settled() {
            return Some(code);
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The agent's own final message, if codex wrote one. Recorded as a claim, never believed.
fn read_claim_file(output_dir: Option<&std::path::Path>) -> Option<String> {
    let path = output_dir?.join("last-message.txt");
    let text = std::fs::read_to_string(&path).ok()?;
    let text = text.trim().to_owned();
    if text.is_empty() {
        None
    } else {
        Some(clip(&text, 4000))
    }
}

/// What the stream said, accumulated as it goes.
#[derive(Default)]
struct StreamState {
    stop_reason: Option<String>,
    claim: Option<String>,
    pending: Vec<(String, String)>,
    malformed: usize,
}

impl StreamState {
    /// One JSONL record, in codex's vocabulary.
    ///
    /// Unknown event and item types are reported rather than dropped: a connector that silently
    /// ignores an event it has never seen makes a newer codex look like a quiet worker.
    fn observe(&mut self, record: &Value, worker: &str) {
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        let item = record.get("item");
        let item_type = item
            .and_then(|i| i.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");

        match kind {
            "thread.started" => {
                let id = record
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                self.push("status", format!("{worker} thread {id}"));
            }
            "turn.started" => self.push("status", "turn started".to_owned()),
            "turn.completed" => {
                self.stop_reason = Some(STOP_END_TURN.to_owned());
                self.push("status", "turn completed".to_owned());
            }
            "turn.failed" => {
                self.stop_reason = Some(STOP_FAILED.to_owned());
                let message = record
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message given");
                self.push("error", message.to_owned());
            }
            // A bare `error` is codex's stream-level warning: the run may continue, but the operator
            // sees it either way.
            "error" => {
                let message = record
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message given");
                self.push("error", message.to_owned());
            }
            "item.started" | "item.updated" | "item.completed" => {
                self.observe_item(item, item_type, kind);
            }
            other => {
                let name = if other.is_empty() { "unnamed" } else { other };
                self.push("unknown", format!("event {name}"));
            }
        }
    }

    fn observe_item(&mut self, item: Option<&Value>, item_type: &str, event: &str) {
        let Some(item) = item else {
            self.push("unknown", format!("{event} with no item"));
            return;
        };
        let field = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| item.get(*n).and_then(Value::as_str))
                .unwrap_or("")
                .to_owned()
        };
        match item_type {
            "agent_message" => {
                let text = field(&["text"]);
                if event == "item.completed" && !text.trim().is_empty() {
                    // The last completed message is the claim. Codex's own `-o` file is preferred,
                    // and this is the fallback for a run configured without one.
                    self.claim = Some(clip(&text, 4000));
                }
                if !text.trim().is_empty() {
                    self.push("agent_message", clip(&text, 600));
                }
            }
            "reasoning" | "thinking" => {
                let text = field(&["text", "summary"]);
                if !text.trim().is_empty() {
                    self.push("thought", clip(&text, 300));
                }
            }
            "command_execution" => {
                let command = field(&["command"]);
                let status = field(&["status"]);
                let exit = item
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .map(|c| format!(" exit {c}"))
                    .unwrap_or_default();
                self.push("tool_call", format!("$ {command} [{status}]{exit}"));
            }
            "file_change" => {
                let paths = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| r.get("path").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                self.push(
                    "tool_call",
                    format!("changed {}", if paths.is_empty() { "?" } else { &paths }),
                );
            }
            "web_search" => self.push("tool_call", format!("search {}", field(&["query"]))),
            other => self.push("tool_call", format!("{other} ({event})")),
        }
    }

    fn push(&mut self, source: &str, text: String) {
        self.pending.push((source.to_owned(), text));
    }

    fn drain(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.pending)
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}… (+{} chars)", count - max_chars)
}

/// A thread draining the worker's stderr, with the capture already open or deliberately discarded.
pub(crate) struct Capture {
    thread: Option<std::thread::JoinHandle<()>>,
}

pub(crate) fn stderr_capture(
    child: &jean_process::piped::PipedChild,
    bounds: &crate::contract::Bounds,
) -> Result<Option<Capture>> {
    // Always drained. A pipe nobody reads fills up (64 KiB by default) and then *blocks the worker*
    // on its next write, which is a hang that looks like a slow agent — so `output_dir = none`
    // discards bytes, it does not stop reading them.
    let reader = child.stderr_file().map_err(|e| WorkerError::Output {
        path: "worker stderr pipe".to_owned(),
        reason: e.to_string(),
    })?;
    let writer = match &bounds.output_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).map_err(|e| WorkerError::Output {
                path: dir.display().to_string(),
                reason: e.to_string(),
            })?;
            let path = dir.join("stderr.log");
            Some(
                std::fs::File::create(&path).map_err(|e| WorkerError::Output {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?,
            )
        }
        None => None,
    };
    let limit = bounds.max_output_bytes;
    let thread = std::thread::spawn(move || drain(reader, writer, limit));
    Ok(Some(Capture {
        thread: Some(thread),
    }))
}

fn drain(mut reader: std::fs::File, mut writer: Option<std::fs::File>, limit: u64) {
    let mut chunk = [0u8; 8 * 1024];
    let mut written = 0u64;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if written >= limit {
                    // Past the ceiling the bytes are still read — the worker must never block on us —
                    // they are just not retained.
                    continue;
                }
                let keep = (limit - written).min(n as u64) as usize;
                if let Some(file) = &mut writer {
                    if file.write_all(&chunk[..keep]).is_err() {
                        return;
                    }
                    let _ = file.flush();
                }
                written += keep as u64;
            }
        }
    }
}

pub(crate) fn join_capture(capture: Option<Capture>) {
    if let Some(capture) = capture {
        if let Some(thread) = capture.thread {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(json: &str) -> Value {
        serde_json::from_str(json).expect("test fixture is valid JSON")
    }

    #[test]
    fn a_completed_turn_is_end_turn_and_its_message_is_a_claim() {
        let mut stream = StreamState::default();
        stream.observe(
            &record(r#"{"type":"thread.started","thread_id":"t-1"}"#),
            "codex",
        );
        stream.observe(
            &record(
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"done, all green"}}"#,
            ),
            "codex",
        );
        stream.observe(&record(r#"{"type":"turn.completed","usage":{}}"#), "codex");
        assert_eq!(stream.stop_reason.as_deref(), Some(STOP_END_TURN));
        assert_eq!(stream.claim.as_deref(), Some("done, all green"));
        // The claim is kept; nothing here turns it into a verdict.
        assert!(!stream.drain().iter().any(|(source, _)| source == "verdict"));
    }

    #[test]
    fn a_failed_turn_reports_failed_and_keeps_the_reason() {
        let mut stream = StreamState::default();
        stream.observe(
            &record(r#"{"type":"turn.failed","error":{"message":"model rejected the plan"}}"#),
            "codex",
        );
        assert_eq!(stream.stop_reason.as_deref(), Some(STOP_FAILED));
        let drained = stream.drain();
        assert!(drained
            .iter()
            .any(|(_, text)| text == "model rejected the plan"));
    }

    #[test]
    fn an_event_type_this_build_has_never_seen_is_reported_not_swallowed() {
        // Version drift is the failure mode of a CLI fallback: a new codex must not go silent.
        let mut stream = StreamState::default();
        stream.observe(&record(r#"{"type":"review.planned"}"#), "codex");
        stream.observe(
            &record(r#"{"type":"item.completed","item":{"type":"mystery_widget","id":"x"}}"#),
            "codex",
        );
        let drained = stream.drain();
        assert!(drained
            .iter()
            .any(|(s, t)| s == "unknown" && t == "event review.planned"));
        assert!(drained.iter().any(|(_, t)| t.contains("mystery_widget")));
    }

    #[test]
    fn a_command_execution_is_described_from_its_argv_not_a_shell_string() {
        let mut stream = StreamState::default();
        stream.observe(
            &record(
                r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo test","status":"completed","exit_code":101}}"#,
            ),
            "codex",
        );
        let (_, text) = &stream.drain()[0];
        assert_eq!(text, "$ cargo test [completed] exit 101");
    }

    #[test]
    fn protocol_flags_are_added_once_and_never_replace_what_the_operator_wrote() {
        let configured = vec![
            "exec".to_owned(),
            "--json".to_owned(),
            "-s".to_owned(),
            "read-only".to_owned(),
        ];
        let args = protocol_args(&configured, None);
        assert_eq!(
            args,
            ["exec", "--json", "-s", "read-only", "-"].map(String::from)
        );
        // The configured sandbox survives untouched, and `--json` was not doubled.
        assert_eq!(args.iter().filter(|a| *a == "--json").count(), 1);
        assert!(args.contains(&"read-only".to_owned()));
    }

    #[test]
    fn a_claim_longer_than_the_report_ceiling_is_clipped_with_the_remainder_named() {
        let long = "x".repeat(5000);
        let clipped = clip(&long, 4000);
        assert!(clipped.starts_with(&"x".repeat(4000)));
        assert!(clipped.contains("(+1000 chars)"), "{clipped}");
        assert_eq!(clip("short", 4000), "short");
    }
}
