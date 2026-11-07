//! Supervised execution: the sequence that makes cancellation trustworthy.
//!
//! ## The ordering is the design
//!
//! ```text
//! create pipes ─┐
//! create job ───┤
//!               ▼
//! CreateProcessW(CREATE_SUSPENDED)      ← child exists but has run nothing
//!               ▼
//! AssignProcessToJobObject              ← containment established
//!               ▼                       if this fails: TerminateProcess, report,
//!               ▼                            never resume (R-14)
//! ResumeThread                          ← only now can the child do anything
//!               ▼
//! close the parent's pipe write ends    ← so EOF means "the whole tree is gone"
//!               ▼
//! reader threads drain both pipes       ← concurrently, and past the cap
//!               ▼
//! WaitForMultipleObjects(child, cancel, deadline)
//!               ▼
//! TerminateJobObject on timeout/cancel  ← kills descendants, not just the child
//! ```
//!
//! Resuming before assignment would leave a window in which the child could
//! spawn a descendant that is never in the job, and that descendant would
//! survive every cancellation J.E.A.N. attempts. There is no way to close the
//! window after the fact, which is why `CREATE_SUSPENDED` is not an optimisation
//! here but a requirement.
//!
//! ## What this module returns
//!
//! [`supervise`] returns `Ok` for every outcome that involved a decision about a
//! child — including launch failure, timeout and cancellation — because the
//! caller must persist all of them. `Err` is reserved for infrastructure
//! failures where no child could be supervised at all.
//!
//! ## The same sequence, run twice
//!
//! A spec gets either two anonymous pipes or one pseudo console; see
//! [`TerminalMode`]. Both branches perform the sequence above with the same
//! objects and the same guarantees: one kill-on-close job, creation suspended,
//! assignment before resume, one wait that decides whether the child exited or was
//! cancelled or missed its deadline, and the same bounded collectors writing the
//! same artifacts. Only the byte plumbing differs, and with it what the outcome
//! can honestly claim about stdout and stderr — [`supervise_in_pty`] documents
//! that.
//!
//! ## What it does not decide
//!
//! Nothing here judges success. `exit_code == 0` means the child said it was
//! happy. Whether that proves the user's goal was met is the verification step,
//! which lives in `jean-core`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jean_protocol::{StreamSummary, Termination};

use crate::cmdline;
use crate::conpty::{self, Pty};
use crate::env;
use crate::error::{ProcessError, Result};
use crate::output::BoundedCollector;
use crate::spec::{ChildOutcome, LaunchSpec, TerminalMode};
use crate::win32::{
    self, CancelEvent, Job, OwnedHandle, PipeRead, SuspendedChild, WaitOutcome, CANCEL_EXIT_CODE,
    READ_CHUNK, TIMEOUT_EXIT_CODE,
};

/// A cancellation signal that several threads can hold.
///
/// Cloning is cheap and shares one kernel event. The supervisor waits on it; the
/// IPC thread in `jean-core` sets it when a second `jean cancel` connects; a
/// Ctrl-C handler can set it too. All three are independent, which is what makes
/// "cancel from a second CLI invocation while the task is running" work without
/// the two processes sharing memory.
#[derive(Clone)]
pub struct CancelToken {
    inner: Arc<TokenInner>,
}

struct TokenInner {
    event: CancelEvent,
    cancelled: AtomicBool,
}

// `CancelEvent` holds a kernel handle. `SetEvent` and `WaitForSingleObject` on
// the same event from different threads is explicitly supported by Win32, and
// the handle is closed exactly once by the `Arc`'s last owner, so sharing the
// reference is sound. Without this, `Arc<TokenInner>` would not be `Sync` and
// could not be handed to the IPC thread.
unsafe impl Sync for TokenInner {}

impl CancelToken {
    /// Create an armed-but-untriggered token.
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: Arc::new(TokenInner {
                event: CancelEvent::new()?,
                cancelled: AtomicBool::new(false),
            }),
        })
    }

    /// Request cancellation. Safe to call more than once and from any thread.
    pub fn cancel(&self) -> Result<()> {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.event.cancel()
    }

    /// True once [`cancel`](Self::cancel) has been called, whether or not the
    /// supervisor has observed it yet.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// The kernel event the supervisor waits on.
    pub(crate) fn handle(&self) -> &OwnedHandle {
        self.inner.event.handle()
    }
}

/// A reader thread's result.
pub(crate) type ReaderResult = (StreamSummary, Option<String>);

/// A process that was started and then released.
///
/// `created_ms` is the process's creation time in milliseconds since the Unix epoch,
/// read while the handle was still held. It is part of the outcome because a pid on
/// its own is not identity: every caller that later asks "is the thing I started still
/// there" must compare both, and `None` means the query failed, which the caller reports
/// rather than treating as "not running".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetachedOutcome {
    pub pid: u32,
    pub created_ms: Option<i64>,
}

/// Start an application and keep it running after this process exits.
///
/// The mirror of [`supervise`] for the launch intent, and deliberately much smaller:
/// nothing is captured, nothing is waited for, and **no Job Object is used**, because a
/// job would take the application down when the CLI finishes. That asymmetry is the
/// whole point — see [`win32::create_detached`] — and it is why this function does not
/// take a cancellation token or a timeout: there is no contract here to cancel, and the
/// caller's job is to *verify* that the application opened, not to own it.
///
/// `spec.timeout`, `spec.output_cap_bytes` and the two artifact paths are ignored. A
/// pseudo console is refused: a detached launch gets a real console of its own, and
/// asking for a pseudo console while also asking to be left alone is a contradiction the
/// caller should hear about rather than get silently redirected.
pub fn launch_detached(spec: &LaunchSpec) -> Result<DetachedOutcome> {
    spec.validate()?;
    if spec.terminal.is_pty() {
        return Err(ProcessError::Invalid {
            field: "terminal",
            reason: "a detached launch creates the application's own console; a pseudo console \
                     only exists for a process this supervisor is reading"
                .to_owned(),
        });
    }

    let command_line = cmdline::build_command_line_units(spec.executable.as_os_str(), &spec.args)?;
    let environment_block = env::encode(&spec.env)?;

    let (pid, created_ms) = win32::create_detached(
        &spec.executable,
        &command_line,
        spec.working_dir.as_deref(),
        &environment_block,
    )?;
    Ok(DetachedOutcome { pid, created_ms })
}

/// The console geometry for a run that wants a terminal but has no window size to
/// pass on.
///
/// 24x80 rather than the pseudo console's own default: a TUI that queries the
/// console size then sees the same geometry on every machine, which is what makes
/// a captured screen comparable across runs.
pub const DEFAULT_PTY_ROWS: u16 = 24;
pub const DEFAULT_PTY_COLS: u16 = 80;

/// Run one child to completion, cancellation or timeout.
///
/// `on_spawn` is called with the child's pid immediately after the process is
/// resumed and before any waiting begins. Persisting the pid there is what makes
/// crash recovery possible: a supervisor killed a moment later leaves a record
/// that names the process it owned.
pub fn supervise<F>(spec: &LaunchSpec, cancel: &CancelToken, on_spawn: F) -> Result<ChildOutcome>
where
    F: FnOnce(u32),
{
    spec.validate()?;

    // A cancel that arrived before the child existed must not be silently
    // ignored: the caller asked for this task to stop.
    if cancel.is_cancelled() {
        return Ok(ChildOutcome {
            pid: 0,
            exit_code: None,
            termination: Termination::Cancelled,
            stdout: StreamSummary::default(),
            stderr: StreamSummary::default(),
            duration: Duration::ZERO,
            error: Some("cancelled before the child was started".to_owned()),
        });
    }

    let command_line = cmdline::build_command_line_units(spec.executable.as_os_str(), &spec.args)?;
    let environment_block = env::encode(&spec.env)?;

    // The branch decides only how bytes travel. The containment, the wait, the
    // classification and the caps are the same code on both paths.
    match spec.terminal {
        TerminalMode::Pipes => {
            supervise_with_pipes(spec, cancel, &command_line, &environment_block, on_spawn)
        }
        TerminalMode::Pty { rows, cols } => supervise_in_pty(
            spec,
            cancel,
            rows,
            cols,
            &command_line,
            &environment_block,
            on_spawn,
        ),
    }
}

/// The Phase-0 path: two anonymous pipes, each capped and retained on its own.
fn supervise_with_pipes<F>(
    spec: &LaunchSpec,
    cancel: &CancelToken,
    command_line: &[u16],
    environment_block: &[u8],
    on_spawn: F,
) -> Result<ChildOutcome>
where
    F: FnOnce(u32),
{
    // Infrastructure first: if any of these fail, no child has been created and
    // dropping the handles is the whole cleanup.
    let stdout_pipe = win32::create_capture_pipe()?;
    let stderr_pipe = win32::create_capture_pipe()?;
    let job = Job::create_kill_on_close()?;

    let child = match win32::create_suspended(
        &spec.executable,
        command_line,
        spec.working_dir.as_deref(),
        environment_block,
        // A supervised command is not handed an input pipe; `piped` does that for a worker.
        None,
        &stdout_pipe.write,
        &stderr_pipe.write,
    ) {
        Ok(child) => child,
        // No child exists. The pipes and the job drop here, which is all the
        // cleanup there is to do.
        Err(e) => {
            return Ok(ChildOutcome::launch_failed(
                &e.to_string(),
                StreamSummary::default(),
                StreamSummary::default(),
            ))
        }
    };

    // Containment before execution. See the module docs for why this cannot be
    // reordered or retried later.
    if let Err(e) = job.assign(&child.process) {
        return Ok(outcome_from_failed_assignment(&child, &e));
    }

    if let Err(e) = win32::resume(&child.thread) {
        // Running and inside the job: terminate through the job, so nothing that
        // managed to start is left behind.
        let mut notes: Vec<String> = Vec::new();
        kill_tree(&job, &child.process, CANCEL_EXIT_CODE, &mut notes);
        // The pipes were never handed to readers, so dropping them here is what
        // makes the child's writes fail rather than block.
        drop(stdout_pipe);
        drop(stderr_pipe);
        return Err(resume_failure(spec, &e, &notes));
    }
    // The thread handle has no further use. Dropping it closes it; the thread
    // itself keeps running.
    drop(child.thread);

    // Closing the parent's copies of the write ends is what makes pipe EOF mean
    // "no process anywhere still holds this pipe".
    drop(stdout_pipe.write);
    drop(stderr_pipe.write);

    // Recorded before any waiting, so a crash from here on leaves a pid behind.
    on_spawn(child.pid);

    let started = Instant::now();
    let mut notes: Vec<String> = Vec::new();

    let readers = match spawn_readers(spec, stdout_pipe.read, stderr_pipe.read) {
        Ok(r) => r,
        Err(e) => {
            // The child is running and contained. Kill it rather than leaving an
            // unsupervised process behind while we report the failure.
            kill_tree(&job, &child.process, CANCEL_EXIT_CODE, &mut notes);
            return Err(ProcessError::Supervision {
                executable: spec.executable.clone(),
                reason: format!("could not start the output reader threads: {e}"),
            });
        }
    };

    let (termination, wait_notes) =
        await_child(&child.process, &job, cancel, started, spec.timeout);
    notes.extend(wait_notes);

    let exit_code = observed_exit_code(&child.process, &mut notes);

    let (stdout, stdout_error) = join_reader(readers.0, "stdout", &mut notes);
    let (stderr, stderr_error) = join_reader(readers.1, "stderr", &mut notes);

    if stdout.truncated {
        notes.push(bytes_note("stdout", spec.output_cap_bytes, stdout.bytes));
    }
    if stderr.truncated {
        notes.push(bytes_note("stderr", spec.output_cap_bytes, stderr.bytes));
    }
    if let Some(e) = stdout_error {
        notes.push(format!("stdout capture failed partway: {e}"));
    }
    if let Some(e) = stderr_error {
        notes.push(format!("stderr capture failed partway: {e}"));
    }

    // The job handle is dropped here. Kill-on-close means that even on the paths
    // above that did not call terminate explicitly, nothing in the tree can
    // outlive this function.
    drop(job);

    Ok(ChildOutcome {
        pid: child.pid,
        exit_code,
        termination,
        stdout,
        stderr,
        duration: started.elapsed(),
        error: if notes.is_empty() {
            None
        } else {
            Some(notes.join("; "))
        },
    })
}

/// Run one child under a pseudo console, with the same guarantees as the pipe path.
///
/// ## What the reported streams mean, and why they cannot mean more
///
/// A console is one surface: a write to stdout and a write to stderr both land on
/// the same screen buffer, in whatever order the child produced them, together with
/// the cursor control a terminal needs. There is therefore exactly one stream to
/// capture, and it goes to the **stdout** artifact. `stderr` in the returned outcome
/// is the empty default, and that empty default does *not* mean the child printed
/// no diagnostics — read naively it looks like a clean run whose errors are in fact
/// sitting inside the stdout file. So [`note_console_stream`] puts the reason in
/// every pty outcome's notes, and the spec refuses a `stderr_path` alongside a
/// console rather than delivering an empty file that asks to be misread.
///
/// ## What the console adds, and what it does not change
///
/// The job, the suspended create, assignment before resume, [`await_child`], the
/// exit-code read, the byte caps and the artifact files are the pipe path's code,
/// reached through the same calls. The console adds a keystroke relay, and the only
/// thing it takes away is the second stream.
#[allow(clippy::too_many_arguments)]
fn supervise_in_pty<F>(
    spec: &LaunchSpec,
    cancel: &CancelToken,
    rows: u16,
    cols: u16,
    command_line: &[u16],
    environment_block: &[u8],
    on_spawn: F,
) -> Result<ChildOutcome>
where
    F: FnOnce(u32),
{
    // Infrastructure first, as in the pipe path: nothing exists to clean up beyond
    // the handles these two own, and both close on drop.
    let mut pty = Pty::create(rows, cols)?;
    let job = Job::create_kill_on_close()?;

    let child = match conpty::create_suspended_in_pty(
        &spec.executable,
        command_line,
        spec.working_dir.as_deref(),
        environment_block,
        &pty,
    ) {
        Ok(child) => child,
        Err(e) => {
            // The console never got a child, so dropping it here is correct: the
            // rule against closing a console under a live process is about a process
            // that exists.
            return Ok(ChildOutcome::launch_failed(
                &e.to_string(),
                StreamSummary::default(),
                StreamSummary::default(),
            ));
        }
    };

    // Containment before execution — the same window that `CREATE_SUSPENDED` closes
    // for a piped child. A pseudo console child that escapes its job would keep a
    // screen alive that nobody can see or cancel.
    if let Err(e) = job.assign(&child.process) {
        return Ok(outcome_from_failed_assignment(&child, &e));
    }

    if let Err(e) = win32::resume(&child.thread) {
        let mut notes: Vec<String> = Vec::new();
        kill_tree(&job, &child.process, CANCEL_EXIT_CODE, &mut notes);
        // Nothing has been handed to a reader or a relay yet, so the console is not
        // in use and closing it here is safe.
        drop(child.thread);
        drop(pty);
        return Err(resume_failure(spec, &e, &notes));
    }
    drop(child.thread);

    on_spawn(child.pid);

    let started = Instant::now();
    let mut notes: Vec<String> = Vec::new();

    // The console duplicated the two ends it needed. Keeping ours is what makes the
    // reader wait forever on a child that has already exited, so they go now —
    // before the reader starts, which is the order `Pty::close_handed` asks for.
    pty.close_handed();

    // The relay is armed before the reader starts and stopped before the console is
    // closed. Both matter: the raw-mode guard below must outlive the thread reading
    // standard input, and the guard is restored when this frame leaves — a thread
    // left behind by an early return would still be reading a terminal that had
    // already been handed back to the operator's shell.
    let raw_guard = conpty::RawInputGuard::enable();
    let mut relay = KeystrokeRelay::start(&pty, raw_guard.is_console(), &mut notes);

    let console_read = match pty.take_output_read() {
        Some(handle) => handle,
        // Impossible for a fresh `Pty`, and still not allowed to become an
        // `expect`: the child is running and contained, so it must be killed before
        // anything is reported.
        None => {
            kill_tree(&job, &child.process, CANCEL_EXIT_CODE, &mut notes);
            return Err(ProcessError::Supervision {
                executable: spec.executable.clone(),
                reason: "the pseudo console's output had already been taken".to_owned(),
            });
        }
    };
    let reader = match spawn_console_reader(spec, console_read) {
        Ok(handle) => handle,
        Err(e) => {
            relay.stop();
            kill_tree(&job, &child.process, CANCEL_EXIT_CODE, &mut notes);
            return Err(ProcessError::Supervision {
                executable: spec.executable.clone(),
                reason: format!("could not start the console reader thread: {e}"),
            });
        }
    };

    let (termination, wait_notes) =
        await_child(&child.process, &job, cancel, started, spec.timeout);
    notes.extend(wait_notes);

    let exit_code = observed_exit_code(&child.process, &mut notes);

    // Nothing may be typed into a console that is going away, so the relay is told
    // to stop first; it leaves within one bounded poll.
    relay.stop();

    // Then the console is closed and the reader is joined.
    //
    // A short pause comes first because the console host turns the child's writes
    // into the output stream as it renders them, and a host that is going away can
    // still have bytes queued. Then the close: measured here, the output pipe's EOF
    // does not arrive when the child dies (the supervisor has no console of its own
    // in that case, and nothing else closes the host's end), so `ClosePseudoConsole`
    // is the event that ends the reader's loop. Joining before it would wait forever.
    std::thread::sleep(CONSOLE_DRAIN_GRACE);
    drop(pty);

    let (stdout, console_error) = join_reader(reader, "console output", &mut notes);

    // One merged stream, reported as stdout because that is where the bytes are. The
    // stderr summary stays empty because a console has no second stream — not because
    // the child wrote no diagnostics — and `note_console_stream` puts that in the
    // outcome so the zeroes cannot be read as a clean run.
    let stderr = StreamSummary::default();
    note_console_stream(&stdout, &mut notes);

    if stdout.truncated {
        notes.push(bytes_note(
            "console output",
            spec.output_cap_bytes,
            stdout.bytes,
        ));
    }
    if let Some(e) = console_error {
        notes.push(format!("console capture failed partway: {e}"));
    }
    // Joining here rather than leaving it to `Drop` is what keeps the relay's own
    // report in this run's notes; `Drop` is the safety net for an early exit.
    if let Some(problem) = relay.shutdown() {
        notes.push(problem);
    }
    drop(job);

    Ok(ChildOutcome {
        pid: child.pid,
        exit_code,
        termination,
        stdout,
        stderr,
        duration: started.elapsed(),
        error: if notes.is_empty() {
            None
        } else {
            Some(notes.join("; "))
        },
    })
}

/// Say, in the outcome, what the console did with the streams.
///
/// Recorded on every pty run, including the quiet ones, because a missing stderr
/// artifact is otherwise indistinguishable from a child that wrote no diagnostics —
/// and that is the difference between "this failed cleanly" and "this failed loudly
/// and the message is in the other file". The byte count and the artifact path are
/// repeated so the note names the stream that was actually filled.
fn note_console_stream(stdout: &StreamSummary, notes: &mut Vec<String>) {
    notes.push(format!(
        "a pseudo console has one merged output stream: the console screen, {} bytes, was \
         captured as stdout{}, so the empty stderr summary means this run had no separate \
         stderr stream and not that the child wrote no diagnostics",
        stdout.bytes,
        match &stdout.path {
            Some(path) => format!(", to {path}"),
            None => " (counted, not retained)".to_owned(),
        }
    ));
}

/// The byte accounting sentence, shared by the two capture paths so a capped pipe
/// and a capped console report the loss in the same words.
fn bytes_note(stream: &str, cap: u64, produced: u64) -> String {
    format!(
        "{stream} exceeded its {cap}-byte cap; {produced} bytes were produced and the excess \
         was drained"
    )
}

/// The console reader: one thread, one merged stream, into the stdout artifact.
///
/// The collector's stream label is "console", not "stdout", so a capture failure
/// says which surface it could not write even though the bytes are reported under
/// the stdout field of the outcome.
fn spawn_console_reader(
    spec: &LaunchSpec,
    read: OwnedHandle,
) -> std::io::Result<JoinHandle<ReaderResult>> {
    let collector = collector_for("console", &spec.stdout_path, spec.output_cap_bytes);
    std::thread::Builder::new()
        .name("jean-console-reader".to_owned())
        .spawn(move || drain(read, collector))
}

/// Wait for the child, its deadline or a cancellation, and terminate the tree if
/// either of the latter two fired.
///
/// Both paths use this so that "the run was cancelled" and "the deadline elapsed"
/// cannot mean different things depending on how the bytes were plumbed. The
/// notes it returns are the ones that explain the classification.
fn await_child(
    process: &OwnedHandle,
    job: &Job,
    cancel: &CancelToken,
    started: Instant,
    timeout: Option<Duration>,
) -> (Termination, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();
    let deadline = timeout.map(|t| started + t);

    // One wait decides everything, so this is a `match` rather than a loop: the
    // three outcomes are terminal, and a second iteration would mean a wake-up
    // that neither finished, cancelled, nor timed out.
    let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let termination = match win32::wait_for_child_or_cancel(process, cancel.handle(), remaining) {
        Ok(WaitOutcome::ProcessExited) => Termination::Exited,
        Ok(WaitOutcome::CancelRequested) => {
            notes.push("cancel requested; the whole job was terminated".to_owned());
            kill_tree(job, process, CANCEL_EXIT_CODE, &mut notes);
            Termination::Cancelled
        }
        Ok(WaitOutcome::TimedOut) => {
            // A timeout can only be reported when a deadline was set, but
            // the message is built without assuming it: an `expect` here
            // would turn a logic slip into a crash in the supervisor.
            let elapsed = render_duration(match timeout {
                Some(t) => t,
                None => started.elapsed(),
            });
            notes.push(format!(
                "the {elapsed} deadline elapsed; the whole job was terminated"
            ));
            kill_tree(job, process, TIMEOUT_EXIT_CODE, &mut notes);
            Termination::Timeout
        }
        Err(e) => {
            // We cannot observe the child any more, so we must not keep it.
            notes.push(format!("the wait failed ({e}); the job was terminated"));
            kill_tree(job, process, CANCEL_EXIT_CODE, &mut notes);
            // Cancelled is the honest label: the child was stopped by J.E.A.N.
            // rather than by its own exit, and the note says the wait itself
            // failed.
            Termination::Cancelled
        }
    };
    (termination, notes)
}

/// The child's exit code, or the reason there is not one.
fn observed_exit_code(process: &OwnedHandle, notes: &mut Vec<String>) -> Option<i32> {
    match win32::exit_code(process) {
        Ok(code) => Some(code),
        Err(e) => {
            notes.push(format!("the exit code could not be read: {e}"));
            None
        }
    }
}

/// Report a child that was created but never contained, after killing it.
///
/// Shared by both paths because the failure is the same shape whatever the byte
/// plumbing: the process exists, is suspended, is outside the job, and therefore
/// cannot be reached by kill-on-close. `TerminateProcess` is the only way, and it
/// is safe because nothing this process did not create is in scope.
fn outcome_from_failed_assignment(child: &SuspendedChild, error: &ProcessError) -> ChildOutcome {
    if let Err(te) = win32::terminate_process(&child.process, CANCEL_EXIT_CODE) {
        tracing::error!(pid = child.pid, error = %te,
            "could not terminate a child that failed job assignment; it is suspended, so it has run nothing");
    }
    let exited = win32::wait_for_child(&child.process, win32::post_kill_grace());
    ChildOutcome {
        pid: child.pid,
        exit_code: win32::exit_code(&child.process).ok(),
        termination: Termination::JobAssignmentFailed,
        stdout: StreamSummary::default(),
        stderr: StreamSummary::default(),
        duration: Duration::ZERO,
        error: Some(format!(
            "AssignProcessToJobObject failed ({error}); the child was terminated \
             before it was resumed{grace}",
            grace = if exited {
                ""
            } else {
                " and did not exit within the grace period"
            }
        )),
    }
}

/// The error for a child that could not be resumed, after the job was terminated.
fn resume_failure(spec: &LaunchSpec, error: &ProcessError, notes: &[String]) -> ProcessError {
    ProcessError::Supervision {
        executable: spec.executable.clone(),
        reason: format!(
            "ResumeThread failed: {error}; the job was terminated{}",
            if notes.is_empty() {
                String::new()
            } else {
                format!(". {}", notes.join("; "))
            }
        ),
    }
}

/// The keystroke relay: this process's standard input into the console's input.
///
/// ## Why it owns its own handles
///
/// The relay lives on a second thread, and two threads closing one `OwnedHandle` is
/// a double `CloseHandle` — which, on a recycled value, closes an unrelated handle
/// somewhere else in this process. So the write end is duplicated with
/// [`OwnedHandle::try_clone`] and the read end is a [`conpty::StdinConsole`], itself a
/// duplicate of the process's console handle. Dropping the relay closes only what the
/// relay owned, and the pseudo console keeps the original for its own teardown.
///
/// ## Why it polls instead of blocking
///
/// A thread parked in a read on the console input handle does not wake because the
/// child exited; it wakes when someone types. By then the run has returned, the raw
/// mode has been restored and the console is closed, so the thread would be reading
/// a terminal that belongs to the operator's shell again. Every wait here is
/// therefore bounded and the stop flag is re-checked on each wake, which is also what
/// lets [`Drop`](Self::drop) join in bounded time after an early return or a panic.
///
/// ## Why a run without a console still runs
///
/// A supervisor started as a service, a scheduled task, or over a redirected stdin
/// has no console input, so nobody could type. Refusing such a run would be worse
/// than reporting it: the child may need no input at all, and its screen is still
/// captured either way. The relay does not start, and the outcome says the run was
/// not attached to a console — which is what distinguishes "this tool waited for an
/// answer and hit its timeout" from an unexplained hang.
struct KeystrokeRelay {
    /// Set to stop the thread; the thread checks it between waits.
    stop: Arc<AtomicBool>,
    /// `None` once the thread has been joined, which is what keeps `Drop` from
    /// joining a thread that is already gone.
    thread: Option<JoinHandle<(u64, Option<String>)>>,
}

impl KeystrokeRelay {
    /// Start forwarding if this process can, and record in `notes` which of the two
    /// happened.
    fn start(pty: &Pty, console_available: bool, notes: &mut Vec<String>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        if !console_available {
            return Self::unwired(&stop, "standard input is not a console", notes);
        }
        let Some(stdin) = conpty::StdinConsole::open() else {
            return Self::unwired(&stop, "this process has no console input handle", notes);
        };
        let input_write = match pty.input_write().try_clone() {
            Ok(clone) => clone,
            Err(e) => return Self::unwired(&stop, &e.to_string(), notes),
        };
        let thread_stop = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("jean-keystroke-relay".to_owned())
            .spawn(move || relay_keystrokes(stdin, input_write, &thread_stop));
        match spawned {
            Ok(thread) => {
                notes.push(
                    "keystrokes from this process's standard input were forwarded to the console"
                        .to_owned(),
                );
                Self {
                    stop,
                    thread: Some(thread),
                }
            }
            Err(e) => Self::unwired(
                &stop,
                &format!("could not start the relay thread: {e}"),
                notes,
            ),
        }
    }

    /// The relay that forwards nothing, with the reason stated where a later reader
    /// of the run will see it.
    fn unwired(stop: &Arc<AtomicBool>, reason: &str, notes: &mut Vec<String>) -> Self {
        notes.push(format!(
            "the run was not attached to a console, so keystrokes were not forwarded ({reason}); \
             a child that waits for input will end on its timeout rather than on an answer"
        ));
        Self {
            stop: Arc::clone(stop),
            thread: None,
        }
    }

    /// Tell the thread to stop. Safe to call repeatedly, and cheap enough to call on
    /// every exit path.
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Stop, join, and report what the thread has to say.
    fn shutdown(&mut self) -> Option<String> {
        self.stop();
        let thread = self.thread.take()?;
        match thread.join() {
            Ok((_, Some(problem))) => Some(problem),
            Ok((_, None)) => None,
            // A panicked relay means the byte counts are unknown, not zero: saying
            // nothing was forwarded would be a fabrication.
            Err(_) => Some(
                "the keystroke relay thread panicked; how many bytes it forwarded is unknown"
                    .to_owned(),
            ),
        }
    }
}

impl Drop for KeystrokeRelay {
    fn drop(&mut self) {
        let Some(problem) = self.shutdown() else {
            return;
        };
        // `Drop` has nowhere to put a note, and the run's report must not be lost for
        // a message: the log carries it. Reached by an early return or a panic in the
        // run, because the normal path joins explicitly.
        tracing::warn!(problem = %problem, "the keystroke relay ended with something to say");
    }
}

/// Forward bytes until the run ends, standard input closes, or the console stops
/// accepting them.
///
/// Returns how many bytes were typed into the console and, if the relay stopped for
/// a reason an operator would want to know, that reason.
fn relay_keystrokes(
    stdin: conpty::StdinConsole,
    input_write: OwnedHandle,
    stop: &AtomicBool,
) -> (u64, Option<String>) {
    let mut buffer = vec![0u8; READ_CHUNK.min(8 * 1024)];
    let mut forwarded = 0u64;
    loop {
        // The wait is bounded, so a run with no typing still ends: the child exits,
        // `stop` is set, and this thread leaves on its next pass.
        if !stdin.has_input(KEYSTROKE_POLL) {
            if stop.load(Ordering::SeqCst) {
                return (forwarded, None);
            }
            continue;
        }
        if stop.load(Ordering::SeqCst) {
            return (forwarded, None);
        }
        match stdin.read_pending(&mut buffer) {
            // The operator's own input ended (a closed stdin, or the equivalent).
            // Nothing more can be typed, and that is not a supervision failure.
            PipeRead::Eof | PipeRead::Data(0) => return (forwarded, None),
            PipeRead::Failed(message) => {
                return (
                    forwarded,
                    Some(format!("standard input could not be read: {message}")),
                )
            }
            PipeRead::Data(count) => {
                let bytes = &buffer[..count];
                match conpty::write_input(&input_write, bytes) {
                    Ok(written) => forwarded += written as u64,
                    Err(e) => {
                        if stop.load(Ordering::SeqCst) {
                            // The child exited while this write was in flight. A
                            // closed input pipe is how an interactive session ends,
                            // not a failure of the run.
                            return (forwarded, None);
                        }
                        return (
                            forwarded,
                            Some(format!(
                                "keystrokes could not be typed into the console: {e}"
                            )),
                        );
                    }
                }
            }
        }
    }
}

/// How long the relay waits for a keystroke before checking whether it should stop.
const KEYSTROKE_POLL: Duration = Duration::from_millis(100);

/// How long the console is kept open after the child has gone, so the host's last
/// rendered bytes reach the reader before it is torn down.
///
/// A pause, not a poll: the reader cannot finish during it, because the output
/// pipe's EOF arrives only with `ClosePseudoConsole`.
const CONSOLE_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// would make the report worse, not better.
pub(crate) fn kill_tree(job: &Job, process: &OwnedHandle, code: u32, notes: &mut Vec<String>) {
    if let Err(e) = job.terminate(code) {
        notes.push(format!("TerminateJobObject failed: {e}"));
    }
    if !win32::wait_for_child(process, win32::post_kill_grace()) {
        notes.push(format!(
            "the child had not exited {} after the job was terminated; its pid is recorded so it can be inspected",
            render_duration(win32::post_kill_grace())
        ));
    }
}

/// Start one reader thread per stream.
pub(crate) fn spawn_readers(
    spec: &LaunchSpec,
    stdout_read: OwnedHandle,
    stderr_read: OwnedHandle,
) -> std::io::Result<(JoinHandle<ReaderResult>, JoinHandle<ReaderResult>)> {
    let stdout_collector = collector_for("stdout", &spec.stdout_path, spec.output_cap_bytes);
    let stderr_collector = collector_for("stderr", &spec.stderr_path, spec.output_cap_bytes);

    let out = std::thread::Builder::new()
        .name("jean-stdout-reader".to_owned())
        .spawn(move || drain(stdout_read, stdout_collector))?;
    let err = std::thread::Builder::new()
        .name("jean-stderr-reader".to_owned())
        .spawn(move || drain(stderr_read, stderr_collector))?;
    Ok((out, err))
}

/// Build a collector, degrading to counting-only if the artifact cannot be
/// opened.
///
/// A capture path that cannot be written must not stop a task from running: the
/// work the user asked for is more important than the log of it. The problem is
/// reported through the outcome instead.
pub(crate) fn collector_for(
    stream: &'static str,
    path: &Option<PathBuf>,
    cap: u64,
) -> BoundedCollector {
    match path {
        None => BoundedCollector::discard(stream, cap),
        Some(p) => match BoundedCollector::to_file(stream, p, cap) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(stream, path = %p.display(), error = %e,
                    "could not open the capture file; output will be counted but not retained");
                BoundedCollector::discard(stream, cap)
            }
        },
    }
}

/// Read a pipe to EOF, feeding everything to the collector.
///
/// Runs on its own thread, concurrently with the other stream and with the wait
/// loop. One thread per stream is what prevents the classic deadlock: a child
/// that fills its stderr pipe while the supervisor is blocked reading stdout
/// would otherwise hang forever, and no timeout would fire until the deadline.
pub(crate) fn drain(read: OwnedHandle, mut collector: BoundedCollector) -> ReaderResult {
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        match win32::read_pipe(&read, &mut buffer) {
            // A zero-byte read on a blocking pipe means the same thing as a
            // broken pipe: nothing more is coming.
            PipeRead::Data(0) | PipeRead::Eof => break,
            PipeRead::Data(n) => collector.push(&buffer[..n]),
            PipeRead::Failed(message) => {
                collector.note_error(message);
                break;
            }
        }
    }
    collector.finish()
}

/// Collect a reader thread's result, tolerating a panic in it.
///
/// A panicked reader means the byte counts are unknown, not zero. Reporting them
/// as zero would claim the child produced nothing, which is a fabrication.
pub(crate) fn join_reader(
    handle: JoinHandle<ReaderResult>,
    stream: &'static str,
    notes: &mut Vec<String>,
) -> ReaderResult {
    match handle.join() {
        Ok(result) => result,
        Err(_) => {
            notes.push(format!(
                "the {stream} reader thread panicked; the byte count for this stream is unknown"
            ));
            (
                StreamSummary {
                    path: None,
                    bytes: 0,
                    truncated: false,
                },
                Some(format!("{stream} reader panicked; counts are unknown")),
            )
        }
    }
}

pub(crate) fn render_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{secs}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn a_fresh_cancel_token_is_not_cancelled() {
        let token = CancelToken::new().expect("the cancel event must be creatable");
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancelling_a_token_is_visible_through_every_clone() {
        let token = CancelToken::new().unwrap();
        let clone = token.clone();
        clone.cancel().expect("SetEvent must succeed");

        assert!(token.is_cancelled());
        assert!(clone.is_cancelled());
        // Repeated cancellation must not fail, because more than one source may
        // request it (a CLI cancel and a Ctrl-C in the same second).
        token.cancel().unwrap();
        assert!(token.is_cancelled());
    }

    #[test]
    fn a_token_can_be_shared_across_threads() {
        // This is the shape jean-core uses: the IPC thread holds a clone and
        // signals the supervisor. If `CancelToken` were not Send + Sync the
        // cancellation requirement could not be met at all.
        let token = CancelToken::new().unwrap();
        let shared = token.clone();
        let handle = std::thread::spawn(move || {
            shared.cancel().unwrap();
            shared.is_cancelled()
        });
        assert!(handle.join().unwrap());
        assert!(token.is_cancelled());
    }

    #[test]
    fn a_token_outlives_its_last_clone() {
        // The kernel event must be closed exactly once, when the final Arc drops.
        for _ in 0..100 {
            let token = CancelToken::new().unwrap();
            let a = token.clone();
            let b = token.clone();
            drop(a);
            drop(token);
            drop(b);
        }
    }

    #[test]
    fn render_duration_stays_readable() {
        assert_eq!(render_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(render_duration(Duration::from_secs(30)), "30000ms");
        assert_eq!(render_duration(Duration::from_secs(60)), "60s");
        assert_eq!(render_duration(Duration::from_secs(3600)), "3600s");
    }

    #[test]
    fn a_collector_with_no_path_counts_without_writing() {
        let c = collector_for("stdout", &None, 1_024);
        let (summary, error) = c.finish();
        assert!(summary.path.is_none());
        assert!(error.is_none());
    }

    #[test]
    fn a_collector_with_an_unwritable_path_degrades_instead_of_failing() {
        // A directory cannot be opened as a capture file. The supervisor must
        // still run the child, because losing the log is not a reason to lose
        // the work.
        let dir = std::env::temp_dir().join(format!(
            "jean-collector-dir-{}-{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let c = collector_for("stderr", &Some(dir.clone()), 1_024);
        let (summary, error) = c.finish();
        assert!(summary.path.is_none(), "no artifact was created");
        assert!(error.is_none(), "the degradation is logged, not returned");

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn a_console_outcome_names_the_stream_it_filled_and_denies_the_one_it_did_not() {
        // The two sentences a reader of a pty run has to be given: where the bytes
        // are, and that an empty stderr is not a clean stderr. Asserted here as well
        // as in the integration tests because the wording is the contract.
        let mut notes = Vec::new();
        let stdout = StreamSummary {
            path: Some(r"C:\runs\1\console.log".to_owned()),
            bytes: 4_096,
            truncated: false,
        };
        note_console_stream(&stdout, &mut notes);
        let text = notes.join("; ");
        assert!(text.contains("one merged output stream"), "{text}");
        assert!(text.contains("4096 bytes"), "{text}");
        assert!(text.contains(r"C:\runs\1\console.log"), "{text}");
        assert!(
            text.contains("not that the child wrote no diagnostics"),
            "the empty stderr summary must be explained, not just left there: {text}"
        );

        let mut counted = Vec::new();
        note_console_stream(
            &StreamSummary {
                path: None,
                bytes: 12,
                truncated: false,
            },
            &mut counted,
        );
        assert!(
            counted.join("; ").contains("counted, not retained"),
            "a console run without an artifact has to say so: {counted:?}"
        );
    }

    #[test]
    fn the_cap_note_is_the_same_sentence_for_a_pipe_and_for_a_console() {
        let pipe = bytes_note("stdout", 1_024, 9_000);
        let console = bytes_note("console output", 1_024, 9_000);
        assert!(pipe.contains("stdout exceeded its 1024-byte cap"), "{pipe}");
        assert!(
            console.contains("9000 bytes were produced"),
            "the true total must be in the note: {console}"
        );
        assert_ne!(pipe, console);
    }

    #[test]
    fn a_relay_without_a_console_reports_itself_as_not_forwarding() {
        // The outcome must not be silent about the degradation. A pty run started
        // from a service, or from `cargo test`, has no console input: the relay does
        // not start, and the notes carry the reason.
        let mut notes: Vec<String> = Vec::new();
        let Ok(pty) = Pty::create(24, 80) else {
            return; // No ConPTY here; the branch under test cannot be reached.
        };
        let guard = conpty::RawInputGuard::enable();
        let mut relay = KeystrokeRelay::start(&pty, guard.is_console(), &mut notes);
        let text = notes.join("; ");
        if guard.is_console() {
            assert!(text.contains("were forwarded"), "{text}");
        } else {
            assert!(
                text.contains("the run was not attached to a console"),
                "the degradation must be stated: {text}"
            );
            assert!(
                text.contains("timeout"),
                "and it must say what the consequence is: {text}"
            );
        }
        assert!(relay.shutdown().is_none(), "no thread, nothing to report");
    }
}
