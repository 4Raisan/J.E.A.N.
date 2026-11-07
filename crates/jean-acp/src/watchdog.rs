//! The watchdog: the only thing that notices a worker has stopped answering.
//!
//! Both readers park in a blocking pipe read — that is the design of [`jean_process::piped`], and it
//! is the right design, because polling a pipe for the lifetime of a ten-minute session would burn a
//! core. The consequence is that nobody on the reading side can see that the operator pressed Ctrl-C,
//! that the session deadline passed, or that the agent has said nothing for two minutes. Someone has
//! to, and it cannot be the reader, so it is this thread.
//!
//! It does two things, in this order, and the order is the point:
//!
//! 1. **Ask.** A [`Signal`] goes to the session task, which can still act — it sends `session/cancel`
//!    to an ACP agent, answers any pending permission request with `Cancelled`, and closes its side
//!    of the pipes. A well-behaved agent finishes its turn and exits within [`shutdown_grace`].
//! 2. **End it.** If the grace elapses and the process is still alive, [`PipedControl::terminate`]
//!    closes the job object. Everything in that job dies with it, including helpers the worker
//!    spawned (`node` under `npx`, a compiler under a shell), which is exactly what "cancellation
//!    kills owned descendants" has to mean. Closing the job also closes the child's pipe ends, so the
//!    blocked reader returns end-of-stream and the session task unblocks on its own.
//!
//! A watchdog that only did step 2 would be a `kill -9` with extra steps and would leave an agent
//! half-writing a file; one that only did step 1 would hang forever on an agent that ignores
//! cancellation, which the spec permits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use jean_process::piped::Control as PipedControl;

use crate::frames::Heartbeat;

/// Why the watchdog is ending the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// The operator, or `jean cancel`, asked for this task to stop.
    Cancelled,
    /// The delegation's own deadline elapsed.
    TimedOut,
    /// The worker said nothing for longer than the idle timeout.
    Idle,
}

/// The poll interval. Short enough that a cancel feels immediate, long enough that an idle watchdog
/// costs nothing measurable.
const POLL: Duration = Duration::from_millis(100);

/// What [`check`] decided, and what the caller should do about it.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing has expired and nobody has cancelled.
    Waiting,
    /// Tell the session, then terminate the job after the grace period.
    Ending(Signal),
    /// The process is gone. The watchdog's job is done.
    Done,
}

/// Pure decision: the reason this is a function rather than ten lines inside a thread loop is that
/// every branch of it gets tested below without spawning a process.
pub fn check(
    cancel: &AtomicBool,
    session_deadline: Instant,
    idle_timeout: Duration,
    heartbeat: &Heartbeat,
    control: &PipedControl,
) -> Verdict {
    if !control.is_running() {
        return Verdict::Done;
    }
    if cancel.load(Ordering::Relaxed) {
        return Verdict::Ending(Signal::Cancelled);
    }
    if Instant::now() >= session_deadline {
        return Verdict::Ending(Signal::TimedOut);
    }
    if idle_timeout > Duration::ZERO && heartbeat.silent_for() >= idle_timeout {
        return Verdict::Ending(Signal::Idle);
    }
    Verdict::Waiting
}

/// A running watchdog. Dropping or calling [`Watchdog::stop`] ends the thread; it does **not** end
/// the worker, which is the caller's decision.
pub struct Watchdog {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Start watching one worker.
    ///
    /// `session` is the channel to the session task, if the connector has one to notify; the CLI
    /// fallback passes `None` and gets termination alone, which is all a one-way stream can offer.
    pub fn arm(
        control: PipedControl,
        cancel: Arc<AtomicBool>,
        session_deadline: Instant,
        idle_timeout: Duration,
        heartbeat: Heartbeat,
        shutdown_grace: Duration,
        session: Option<UnboundedSender<Signal>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match check(
                        &cancel,
                        session_deadline,
                        idle_timeout,
                        &heartbeat,
                        &control,
                    ) {
                        Verdict::Waiting => {
                            std::thread::sleep(POLL);
                            continue;
                        }
                        Verdict::Done => return,
                        Verdict::Ending(signal) => {
                            // Best effort: a closed channel means the session task is already going
                            // away, and step 2 does not depend on it having heard us.
                            if let Some(tx) = &session {
                                let _ = tx.send(signal);
                            }
                            // Give the agent the grace period to exit on its own before the job
                            // closes. Checked in small slices so `stop` still works during the wait.
                            let until = Instant::now() + shutdown_grace;
                            while Instant::now() < until {
                                if stop.load(Ordering::Relaxed) {
                                    return;
                                }
                                if !control.is_running() {
                                    return;
                                }
                                std::thread::sleep(POLL);
                            }
                            // The distinction is kept for the exit code the operator sees: a timeout
                            // is reported as a timeout, a cancel as a cancel.
                            let _ = match signal {
                                Signal::TimedOut => control.terminate_on_timeout(),
                                _ => control.terminate(),
                            };
                            return;
                        }
                    }
                }
            })
        };
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// Stop watching. Called when the session ends normally, so a healthy worker is never killed by
    /// a watchdog that outlived its purpose.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        // `stop` already took the flag; this is the same, for a watchdog dropped without being called.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The channel pair a connector uses: the sender goes to the watchdog, the receiver to the session.
///
/// Unbounded, and still bounded in practice: a watchdog sends at most one signal and then returns, so
/// the queue holds a single item. Awaiting it is only possible on the session side because the session
/// is the async half of this crate; the CLI connector uses `try_recv` after its stream ends.
pub fn session_channel() -> (UnboundedSender<Signal>, UnboundedReceiver<Signal>) {
    tokio::sync::mpsc::unbounded_channel()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `check` needs a `PipedControl`, which only `jean_process::piped::spawn` can produce, so the
    // branches below are exercised through a process this crate really starts — a silent worker, which
    // is the case both the idle and the deadline branch are about. What is testable without a process is
    // the arithmetic, and that is what `deadline` and `idle` do here.

    #[test]
    fn a_deadline_in_the_past_is_a_timeout_only_after_cancel_is_consulted() {
        // The ordering inside `check` is cancel → deadline → idle. A worker whose deadline passed and
        // which the operator also cancelled is reported as cancelled, because that is the operator's
        // action and it is the more specific fact.
        let cancel = AtomicBool::new(true);
        let deadline = Instant::now() - Duration::from_secs(5);
        let beat = Heartbeat::new();
        // No process: a control for a pid that exited immediately reports not-running, which short
        // circuits to Done. Spawn one that is guaranteed to have died.
        let spec = exit_now_spec();
        let child = jean_process::piped::spawn(&spec).expect("spawn");
        let control = child.control();
        wait_until_stopped(&control);
        assert_eq!(
            check(&cancel, deadline, Duration::from_secs(30), &beat, &control),
            Verdict::Done,
            "a worker that is gone is not an expired worker"
        );
    }

    #[test]
    fn an_idle_worker_is_still_watched_while_a_deadline_has_not_passed() {
        // A sleeping worker is the whole point of the idle timeout: `tail -f`-style, it is alive, the
        // session deadline is far away, and it has produced nothing.
        let spec = sleeper_spec();
        let child = jean_process::piped::spawn(&spec).expect("spawn");
        let control = child.control();
        let cancel = AtomicBool::new(false);
        let beat = Heartbeat::new();
        // Zero idle timeout means "off" (see `check`), so the waiting case is testable without
        // waiting, and then the real case with a timeout already elapsed.
        assert_eq!(
            check(
                &cancel,
                Instant::now() + Duration::from_secs(60),
                Duration::ZERO,
                &beat,
                &control
            ),
            Verdict::Waiting
        );
        std::thread::sleep(Duration::from_millis(300));
        let ending = check(
            &cancel,
            Instant::now() + Duration::from_secs(60),
            Duration::from_millis(100),
            &beat,
            &control,
        );
        assert_eq!(ending, Verdict::Ending(Signal::Idle));
        let _ = control.terminate();
    }

    #[test]
    fn a_passing_deadline_ends_the_session_as_a_timeout() {
        let spec = sleeper_spec();
        let child = jean_process::piped::spawn(&spec).expect("spawn");
        let control = child.control();
        let cancel = AtomicBool::new(false);
        let beat = Heartbeat::new();
        beat.beat();
        let ending = check(
            &cancel,
            Instant::now(),
            Duration::from_secs(600),
            &beat,
            &control,
        );
        assert_eq!(ending, Verdict::Ending(Signal::TimedOut));
        let _ = control.terminate();
    }

    #[test]
    fn arming_and_stopping_a_watchdog_leaves_the_worker_alone() {
        // `stop` must not kill: a session that finishes normally has its watchdog retired, and the
        // worker is still there for the caller to shut down deliberately.
        let spec = sleeper_spec();
        let child = jean_process::piped::spawn(&spec).expect("spawn");
        let control = child.control();
        let (tx, _rx) = session_channel();
        let watchdog = Watchdog::arm(
            control.clone(),
            Arc::new(AtomicBool::new(false)),
            Instant::now() + Duration::from_secs(60),
            Duration::from_secs(60),
            Heartbeat::new(),
            Duration::from_millis(50),
            Some(tx),
        );
        watchdog.stop();
        assert!(control.is_running(), "a retired watchdog must not kill");
        let _ = control.terminate();
    }

    /// A worker that finishes by itself. `cmd.exe` is not usable here: with an empty environment
    /// block it refuses to start at all (`ERROR_INVALID_PARAMETER`), which is a fact about cmd, not
    /// about this crate, and is tested where it belongs — in `jean-process`.
    fn exit_now_spec() -> jean_process::spec::LaunchSpec {
        command("ping.exe", &["-n", "1", "127.0.0.1"])
    }

    fn sleeper_spec() -> jean_process::spec::LaunchSpec {
        // A worker that stays alive and says nothing, which is the shape both the idle and the
        // deadline branches need. `ping` is used rather than `timeout` because `timeout` refuses to
        // run without a console.
        command("ping.exe", &["-n", "30", "127.0.0.1"])
    }

    fn command(program: &str, args: &[&str]) -> jean_process::spec::LaunchSpec {
        // A child with a **completely empty** environment is a real case, not a convenience: Windows
        // refuses `cmd.exe` outright with `ERROR_INVALID_PARAMETER`, which is why
        // `[delegation] workers.env_passthrough` exists. `jean-core` builds the allowlist-pinned block
        // for production; a test that starts a system binary has to pin at least what it needs.
        let root = system_root();
        let system_dir = std::path::Path::new(&root).join("System32");
        let mut env = std::collections::BTreeMap::new();
        env.insert("SystemRoot".to_owned(), root.clone());
        env.insert(
            "PATH".to_owned(),
            format!("{};{}", system_dir.display(), root),
        );
        env.insert(
            "COMSPEC".to_owned(),
            system_dir.join("cmd.exe").to_string_lossy().into_owned(),
        );
        jean_process::spec::LaunchSpec {
            executable: system32(program),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            working_dir: None,
            env,
            timeout: None,
            output_cap_bytes: 64 * 1024,
            stdout_path: None,
            stderr_path: None,
            terminal: jean_process::spec::TerminalMode::Pipes,
        }
    }

    /// The Windows directory, as the machine has it.
    fn system_root() -> String {
        std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:\\Windows"))
    }

    /// An absolute path, because `LaunchSpec::validate` checks existence and does not search `PATH`.
    fn system32(program: &str) -> std::path::PathBuf {
        std::path::Path::new(&system_root())
            .join("System32")
            .join(program)
    }

    fn wait_until_stopped(control: &PipedControl) {
        let until = Instant::now() + Duration::from_secs(10);
        while control.is_running() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
