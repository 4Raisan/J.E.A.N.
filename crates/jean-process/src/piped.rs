//! A supervised child J.E.A.N. can **talk to**, not only capture.
//!
//! [`crate::runner`] answers a different question: start a command, collect its output inside a byte
//! ceiling, decide whether it exited or was killed. Nothing there can be handed a second instruction
//! after it starts, which is what a *worker* needs — an ACP agent waiting for a prompt on stdin, an
//! MCP server answering `tools/call`, a browser that must be told to quit. This module supplies that
//! shape and keeps every guarantee the command path already has:
//!
//! * **The same containment sequence.** Job created with kill-on-close → `CreateProcessW` with
//!   `CREATE_SUSPENDED` → [`Job::assign`] → [`win32::resume`]. Containment before execution is not an
//!   optimisation here either: a protocol peer spawns helpers (`node` under `npx`, a compiler under a
//!   shell), and a descendant created in the window before assignment would survive us.
//! * **Three pipes, one of which the child reads.** `runner` gives the child stdout and stderr only,
//!   and passes an invalid stdin on purpose so a child that reads input fails immediately. A worker is
//!   given the read end of a real pipe.
//! * **A handle set the child cannot confuse.** Only the ends handed to the child are inheritable, and
//!   the parent drops every child-side copy it still holds, so pipe EOF means "no process anywhere
//!   still holds this pipe" — the same reasoning as in `runner`, and the reason a protocol reader can
//!   tell "the agent exited" from "the agent is thinking".
//!
//! Two handles are exposed rather than a `std::process::Child` because a `Child` this crate did not
//! create cannot be put in a job before it runs. The trade is that reads and writes are raw pipe
//! operations: [`PipedChild::read_stdout`] returns bytes, and framing lines or JSON is the protocol
//! module's business, not this one's.
//!
//! Cancellation has an owner-thread shape on purpose. A blocked `ReadFile` cannot be interrupted by a
//! flag, and a polling read would burn CPU for the life of a session that may run ten minutes. The way
//! to stop a worker is [`Control::terminate`], which closes the job: the child dies, its write ends
//! close, and the blocked read returns [`PipeRead::Eof`] at once. `Control` is `Send + Sync`, so a
//! watchdog thread can hold one while the session stays on the thread that owns the pipes.

use std::sync::{Arc, Mutex};

use jean_protocol::redact::Redactor;

use crate::cmdline;
use crate::env;
use crate::error::{ProcessError, Result};
use crate::spec::LaunchSpec;
use crate::win32::{
    self, Job, OwnedHandle, PipePair, PipeRead, CANCEL_EXIT_CODE, TIMEOUT_EXIT_CODE,
};

/// What survives after [`PipedChild`] moves onto its owning thread.
///
/// Send and Sync because every field is either a kernel handle behind a mutex or an integer, and
/// `Mutex<T: Send>` is what lets a watchdog thread act on a session it does not own.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
}

impl Control {
    /// The child's process id. Windows recycles pids, so this identifies the *run*, not the process:
    /// pair it with [`Control::is_running`], or use the task id the caller recorded at spawn.
    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    /// True while the process has not exited.
    pub fn is_running(&self) -> bool {
        self.shared
            .exit_code()
            .is_none_or(|code| code == STILL_ACTIVE)
    }

    /// The exit code, if the process has already exited.
    pub fn exit_code_if_settled(&self) -> Option<i32> {
        match self.shared.exit_code() {
            Some(STILL_ACTIVE) | None => None,
            Some(code) => Some(code),
        }
    }

    /// Terminate the whole job tree.
    ///
    /// This is the cancel path and the deadline path, and it is also what unblocks a reader parked in
    /// `ReadFile`: the child dies, its write ends close, and the read returns end of stream.
    pub fn terminate(&self) -> Result<()> {
        let job =
            self.shared.job.lock().map_err(|_| {
                ProcessError::win32("TerminateJobObject", "the job lock was poisoned")
            })?;
        job.terminate(CANCEL_EXIT_CODE)
    }

    pub fn tree_empty(&self) -> Result<bool> {
        let job = self.shared.job.lock().map_err(|_| {
            ProcessError::win32("QueryInformationJobObject", "the job lock was poisoned")
        })?;
        job.is_empty()
    }

    /// Terminate because the deadline elapsed, with the exit code that says so.
    pub fn terminate_on_timeout(&self) -> Result<()> {
        let job =
            self.shared.job.lock().map_err(|_| {
                ProcessError::win32("TerminateJobObject", "the job lock was poisoned")
            })?;
        job.terminate(TIMEOUT_EXIT_CODE)
    }
}

/// ` STILL_ACTIVE`, the value `GetExitCodeProcess` keeps reporting while the process runs.
const STILL_ACTIVE: i32 = 259;

struct Shared {
    /// A mutex, not because the job needs one, but because `Job` is a raw handle wrapper and this
    /// crate deliberately declares handles `Send` only. A watchdog thread must be able to reach it.
    job: Mutex<Job>,
    process: Mutex<OwnedHandle>,
    stdin: Mutex<OwnedHandle>,
    pid: u32,
}

impl Shared {
    fn exit_code(&self) -> Option<i32> {
        let process = self.process.lock().ok()?;
        win32::exit_code(&process).ok()
    }

    fn write(&self, bytes: &[u8]) -> Result<()> {
        let stdin = self
            .stdin
            .lock()
            .map_err(|_| ProcessError::win32("WriteFile(pipe)", "the stdin lock was poisoned"))?;
        win32::write_pipe(&stdin, bytes)
    }
}

/// A worker process, inside a job, with its three pipes.
///
/// Move it to the thread that will speak the protocol. Its [`Control`] clone is what every other
/// thread uses to cancel it.
pub struct PipedChild {
    shared: Arc<Shared>,
    stdout: OwnedHandle,
    stderr: OwnedHandle,
}

impl PipedChild {
    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    /// A cancel handle for another thread.
    pub fn control(&self) -> Control {
        Control {
            shared: self.shared.clone(),
        }
    }

    /// Write to the worker's stdin. The bytes are the caller's frame; this adds no separator.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        self.shared.write(bytes)
    }

    /// One line to stdin, `\n` included: the framing both ACP and MCP use over stdio.
    pub fn write_line(&self, bytes: &[u8]) -> Result<()> {
        let mut frame = bytes.to_vec();
        frame.push(b'\n');
        self.shared.write(&frame)
    }

    /// A blocking read from the worker's stdout.
    ///
    /// Returns [`PipeRead::Eof`] once every writer has closed, which after [`Control::terminate`] is
    /// immediate. Callers that need a deadline must hold a [`Control`] elsewhere; a blocking read has
    /// no timeout of its own.
    pub fn read_stdout(&self, buffer: &mut [u8]) -> PipeRead {
        win32::read_pipe(&self.stdout, buffer)
    }

    /// A blocking read from the worker's stderr, which is **diagnostics only**.
    ///
    /// Never protocol: a peer that writes garbage to stdout has already broken the session, and the
    /// reason the two streams are separate handles is so a noisy tool cannot be mistaken for a frame.
    pub fn read_stderr(&self, buffer: &mut [u8]) -> PipeRead {
        win32::read_pipe(&self.stderr, buffer)
    }

    /// The worker's stdin as a `File`, for a protocol library that needs a std stream.
    ///
    /// A *duplicate*: `PipedChild` keeps its own end, so dropping the `File` closes one writer, not
    /// the pipe, and a peer never sees EOF because a caller scoped a handle too tightly.
    pub fn stdin_file(&self) -> Result<std::fs::File> {
        let stdin = self.shared.stdin.lock().map_err(|_| {
            ProcessError::win32("DuplicateHandle(stdin)", "the stdin lock was poisoned")
        })?;
        as_file(&stdin)
    }

    /// The worker's stdout as a `File` — the protocol stream, duplicated (see [`Self::stdin_file`]).
    pub fn stdout_file(&self) -> Result<std::fs::File> {
        as_file(&self.stdout)
    }

    /// The worker's stderr as a `File` — diagnostics, duplicated.
    pub fn stderr_file(&self) -> Result<std::fs::File> {
        as_file(&self.stderr)
    }

    /// Close our end of the worker's stdin so it sees end-of-file.
    ///
    /// A CLI worker (`codex exec -`) treats an open stdin as "the caller still has instructions to
    /// give", so a session that never closes it waits forever after the prompt it already read. The
    /// slot keeps a value — an invalid handle, which `Drop` skips — so a second call is a no-op rather
    /// than a panic, and any duplicate handed out by [`Self::stdin_file`] is unaffected: that is a
    /// separate kernel handle, and closing one writer on a pipe does not close the pipe.
    pub fn close_stdin(&self) -> Result<()> {
        let mut stdin = self.shared.stdin.lock().map_err(|_| {
            ProcessError::win32("CloseHandle(stdin)", "the stdin lock was poisoned")
        })?;
        let previous = std::mem::replace(&mut *stdin, OwnedHandle::invalid());
        // Dropping `previous` is the close. Nothing else holds it, because every other end this crate
        // gave away came from `try_clone`.
        drop(previous);
        Ok(())
    }
}

/// Duplicate an owned handle into a `std::fs::File` the caller owns outright.
///
/// This is the only place a raw handle crosses into the standard library, and it crosses by copy:
/// the duplicate's ownership moves to the `File`, which closes it exactly once.
fn as_file(handle: &OwnedHandle) -> Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;

    let copy = handle.try_clone()?;
    // `into_raw` deliberately does not close, which is what makes the next line correct: ownership of
    // this duplicate moves to the `File`, and the `File` closes it exactly once.
    let raw = copy.into_raw();
    // SAFETY: `try_clone` returned a fresh, valid handle for this pipe, and nothing else owns it.
    Ok(unsafe { std::fs::File::from_raw_handle(raw.0) })
}

/// Start a worker. The child exists only if this returns `Ok`.
pub fn spawn(spec: &LaunchSpec) -> Result<PipedChild> {
    spec.validate()?;

    // Infrastructure first: nothing below this point has created a process, so any failure here is a
    // clean return and the pipes are closed by their own drops.
    let stdin_pipe = child_side_pipe()?;
    let stdout_pipe = win32::create_capture_pipe()?;
    let stderr_pipe = win32::create_capture_pipe()?;
    // `create_capture_pipe` marks both ends inheritable because a capturing child receives the write
    // end. Our own read ends must not travel with it: a grandchild that ends up holding the read half
    // of its parent's output pipe keeps a *reader* alive, and the parent's `read_stdout` then waits
    // for an EOF that the whole point of this module — "no process anywhere still holds this pipe" —
    // says will arrive.
    win32::set_inheritable(&stdout_pipe.read, false)?;
    win32::set_inheritable(&stderr_pipe.read, false)?;
    let job = Job::create_kill_on_close()?;

    let command_line = cmdline::build_command_line_units(spec.executable.as_os_str(), &spec.args)?;
    let environment_block = env::encode(&spec.env)?;

    let child = match win32::create_suspended(
        &spec.executable,
        &command_line,
        spec.working_dir.as_deref(),
        &environment_block,
        Some(&stdin_pipe.read),
        &stdout_pipe.write,
        &stderr_pipe.write,
    ) {
        Ok(child) => child,
        Err(e) => {
            // No child exists. Dropping the pipes and the job is the entire cleanup.
            drop(stdin_pipe);
            drop(stdout_pipe);
            drop(stderr_pipe);
            return Err(e);
        }
    };

    let pid = child.pid;
    if let Err(e) = job.assign(&child.process) {
        // The child exists and has run nothing. Terminating the job is the only correct response:
        // an uncontained worker is exactly what this crate exists to prevent.
        let _ = job.terminate(CANCEL_EXIT_CODE);
        drop(child.thread);
        drop(stdin_pipe);
        drop(stdout_pipe);
        drop(stderr_pipe);
        return Err(ProcessError::JobAssignment {
            pid,
            reason: e.to_string(),
        });
    }

    if let Err(e) = win32::resume(&child.thread) {
        let _ = job.terminate(CANCEL_EXIT_CODE);
        drop(child.thread);
        drop(stdin_pipe);
        drop(stdout_pipe);
        drop(stderr_pipe);
        return Err(ProcessError::Supervision {
            executable: spec.executable.clone(),
            reason: format!("ResumeThread: {e}"),
        });
    }
    drop(child.thread);

    // The parent keeps exactly the ends it uses. Holding the child's ends would make the worker's
    // stdout never report EOF, because a writer we are not reading from is still a writer.
    drop(stdin_pipe.read);
    drop(stdout_pipe.write);
    drop(stderr_pipe.write);

    Ok(PipedChild {
        shared: Arc::new(Shared {
            job: Mutex::new(job),
            process: Mutex::new(child.process),
            stdin: Mutex::new(stdin_pipe.write),
            pid,
        }),
        stdout: stdout_pipe.read,
        stderr: stderr_pipe.read,
    })
}

/// A pipe whose **read** end is the one handed to the child.
///
/// `create_capture_pipe` marks both ends inheritable, which is right for an output pipe and wrong
/// here: the parent's copy of a read end the child owns would let a write succeed with no reader
/// left alive. Only the read end may be inherited, so the write end is set non-inheritable.
fn child_side_pipe() -> Result<PipePair> {
    let pair = win32::create_capture_pipe()?;
    win32::set_inheritable(&pair.write, false)?;
    Ok(pair)
}

/// Redact a worker's command line for the audit trail and the terminal.
///
/// A delegation prompt can carry a credential the agent was told to use, and the arguments are where
/// it would appear; the same [`Redactor`] the process path already uses is applied here rather than a
/// second rule that can drift.
pub fn describe(spec: &LaunchSpec, redactor: &Redactor) -> String {
    redactor.redact(&spec.describe())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use super::*;

    fn system32(program: &str) -> std::path::PathBuf {
        let root = system_root();
        std::path::Path::new(&root).join("System32").join(program)
    }

    fn system_root() -> String {
        std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:\\Windows"))
    }

    /// The minimum a Windows console binary needs in its environment in order to start at all.
    fn standard_environment() -> BTreeMap<String, String> {
        let root = system_root();
        let mut env = BTreeMap::new();
        env.insert("SystemRoot".to_owned(), root.clone());
        env.insert("COMSPEC".to_owned(), format!("{root}\\System32\\cmd.exe"));
        env.insert("PATH".to_owned(), format!("{root}\\System32;{root}"));
        env.insert("PATHEXT".to_owned(), ".COM;.EXE;.BAT;.CMD".to_owned());
        env
    }

    fn spec(program: &str, args: &[&str]) -> LaunchSpec {
        spec_with(program, args, standard_environment())
    }

    fn spec_with(program: &str, args: &[&str], env: BTreeMap<String, String>) -> LaunchSpec {
        LaunchSpec {
            executable: system32(program),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            working_dir: None,
            env,
            timeout: Some(Duration::from_secs(20)),
            output_cap_bytes: 64 * 1024,
            stdout_path: None,
            stderr_path: None,
            terminal: crate::spec::TerminalMode::Pipes,
        }
    }

    /// A worker's stdin is a real pipe, and what we write reaches the child.
    ///
    /// `findstr` is the child: it copies its stdin to stdout, so the assertion is about bytes that came
    /// back rather than about a handle that looks right. `cmd.exe /C` would be the obvious choice and is
    /// the wrong one — it never reads stdin at all.
    #[test]
    fn a_worker_child_starts_with_a_usable_stdin_and_a_usable_environment() {
        let child = spawn(&spec("findstr.exe", &["/R", "."])).expect("a worker must start");
        child
            .write_line(b"from-stdin\r\n")
            .expect("the worker's stdin accepts a line");
        child
            .close_stdin()
            .expect("closing stdin is how a prompt ends");

        let mut reader = LineCollector::new(&child);
        let got = reader.until_eof(Duration::from_secs(15));
        assert!(got.contains("from-stdin"), "stdout was {got:?}");
        assert_eq!(child.control().exit_code_if_settled(), Some(0));
    }

    /// A second `close_stdin` closes nothing twice.
    ///
    /// The prompt writer and the connector can both decide the worker has all it was told; the second
    /// call must be a no-op rather than a `CloseHandle` on a recycled value.
    #[test]
    fn closing_the_workers_stdin_twice_is_a_no_op() {
        let child = spawn(&spec("findstr.exe", &["/R", "."])).expect("a worker must start");
        child.close_stdin().expect("first close");
        child.close_stdin().expect("second close");
        let _ = child.control().terminate();
    }

    /// Whether Windows refuses a `%SystemRoot%`-less `cmd.exe` is the loader's decision, and this test
    /// binary answers it both ways for reasons this batch did not establish: the full 117-test library
    /// starts the child, while the same binary with one unrelated test appended — which sorts *after*
    /// this one, so nothing that runs before it differs — refuses it, and no subset bisect found a
    /// module, a pair, or a count that predicted either answer. So the assertion here is the one that
    /// actually protects a delegation: the worker is either refused outright or it starts with the block
    /// J.E.A.N. handed it, and it never quietly ends up with J.E.A.N.'s own environment because the
    /// allowlist decided on nothing.
    ///
    /// The reason the allowlist has to carry `SystemRoot`, `COMSPEC` and `PATH` in the first place
    /// is recorded by [`a_worker_sees_the_environment_it_was_given_and_no_other`].
    #[test]
    fn a_stripped_environment_is_refused_or_hermetic_but_never_the_parents() {
        let empty = BTreeMap::new();
        let started = spawn(&spec_with("cmd.exe", &["/C", "set"], empty));
        match started {
            Ok(child) => {
                let mut reader = LineCollector::new(&child);
                let got = reader.until_eof(Duration::from_secs(15));
                let _ = child.control().terminate();
                let lowered = got.to_lowercase();
                for leaked in ["systemroot=", "userprofile=", "temp="] {
                    assert!(
                        !lowered.contains(leaked),
                        "the worker was given an empty environment and still reports {leaked:?}: \
                         {got:?}"
                    );
                }
            }
            Err(e) => {
                let text = e.to_string();
                assert!(
                    text.contains("parameter is incorrect"),
                    "the spawn failure should carry the Win32 reason, not a bare code: {text}"
                );
            }
        }
    }

    /// The child's own testimony that the environment block is the one it was given.
    ///
    /// `cmd.exe /C set` prints its environment, so nothing here is read back from a structure
    /// J.E.A.N. built for itself. The five names have to appear and the three that belong to the
    /// process doing the launching must not. `PROMPT` is cmd's own default rather than an inherited
    /// value, which is why it is not asserted either way.
    #[test]
    fn a_worker_sees_the_environment_it_was_given_and_no_other() {
        let mut env = standard_environment();
        env.insert("JEAN_CANARY".to_owned(), "hermetic".to_owned());
        let child = spawn(&spec_with("cmd.exe", &["/C", "set"], env))
            .expect("a worker with SystemRoot must start");

        let mut reader = LineCollector::new(&child);
        let got = reader.until_eof(Duration::from_secs(15));
        let _ = child.control().terminate();
        let lowered = got.to_lowercase();

        for expected in [
            "systemroot=",
            "comspec=",
            "path=",
            "pathext=",
            "jean_canary=",
        ] {
            assert!(
                lowered.contains(expected),
                "the worker's environment is missing {expected:?}: {got:?}"
            );
        }
        for leaked in ["userprofile=", "temp=", "appdata="] {
            assert!(
                !lowered.contains(leaked),
                "the worker inherited {leaked:?} from J.E.A.N.'s own environment: {got:?}"
            );
        }
    }

    /// The job, not just the process.
    ///
    /// `cmd.exe /C ping -n 30 …` makes `ping` a **descendant** of the worker. Anonymous pipes are
    /// inherited, so `ping` holds the same stdout write end `cmd` does, and the parent's read cannot
    /// report end-of-stream until every holder is gone. EOF arriving within a second of
    /// [`Control::terminate`] is therefore direct evidence that the whole tree was closed — the
    /// guarantee a worker protocol depends on, because a cancelled delegation with a live `ping`, `node`
    /// or compiler behind it is a delegation that is still running.
    #[test]
    fn terminating_the_job_takes_the_workers_descendants_with_it() {
        let child = spawn(&spec("cmd.exe", &["/C", "ping -n 30 127.0.0.1"]))
            .expect("cmd.exe must start as a worker");
        let control = child.control();
        assert!(control.is_running());

        // Let the descendant appear and take hold of the pipe before cancelling.
        std::thread::sleep(Duration::from_millis(400));
        control.terminate().expect("terminate");

        let mut reader = LineCollector::new(&child);
        let started = Instant::now();
        let elapsed_budget = Duration::from_secs(5);
        let reached_eof = reader.wait_eof(elapsed_budget);
        let took = started.elapsed();
        assert!(
            reached_eof,
            "stdout never reported EOF after {}ms: a descendant still holds the pipe",
            took.as_millis()
        );
        assert!(
            took < Duration::from_secs(4),
            "cancelling a worker took {}ms — the tree is not being closed promptly",
            took.as_millis()
        );
        assert_eq!(
            control.exit_code_if_settled(),
            Some(crate::win32::CANCEL_EXIT_CODE as i32),
            "the worker must report the code J.E.A.N. wrote into the job"
        );
    }

    /// Reads the worker's stdout without a framing opinion, for the tests in this module only.
    struct LineCollector<'a> {
        child: &'a PipedChild,
        buffer: Vec<u8>,
        done: bool,
    }

    impl<'a> LineCollector<'a> {
        fn new(child: &'a PipedChild) -> Self {
            Self {
                child,
                buffer: Vec::new(),
                done: false,
            }
        }

        fn until_eof(&mut self, budget: Duration) -> String {
            let _ = self.pump(budget);
            String::from_utf8_lossy(&self.buffer).into_owned()
        }

        /// Read until the pipe reports end-of-stream, or the budget runs out.
        fn wait_eof(&mut self, budget: Duration) -> bool {
            self.pump(budget)
        }

        fn pump(&mut self, budget: Duration) -> bool {
            let mut scratch = [0u8; 4096];
            let until = Instant::now() + budget;
            while Instant::now() < until {
                match self.child.read_stdout(&mut scratch) {
                    PipeRead::Data(n) => self.buffer.extend_from_slice(&scratch[..n]),
                    PipeRead::Eof => {
                        self.done = true;
                        return true;
                    }
                    PipeRead::Failed(reason) => panic!("read failed: {reason}"),
                }
            }
            self.done
        }
    }
}
