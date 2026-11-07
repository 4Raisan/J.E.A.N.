//! Running a blocking Win32 call with a bound that actually holds.
//!
//! This is the mechanism [`crate::worker`] replaced for UI Automation and capture: those two now run in a
//! child process that can be terminated, because a `SendMessage` into a provider that is not pumping
//! cannot be cancelled and a parked thread therefore has to be *killed* rather than walked away from.
//!
//! What is left here is [`crate::input`], and it stays for a reason that is measurable rather than
//! convenient: `SendInput` posts to the system input queue, and the verification around it reads the
//! window manager's own records — no call in that path crosses into another application's window
//! procedure, so there is no provider that can hold it open indefinitely. Running an *action* in a process
//! that can be killed mid-action would make the no-replay rule (D-050) harder to honour, not easier.
//!
//! The honesty of the design is unchanged, and it is worth keeping in one sentence: a timed-out call here
//! is *abandoned*, not cancelled, and the thread is left parked on purpose. [`WorkerBudget`] caps how many
//! of those the process will carry at once, after which the work is refused rather than accumulated. The
//! waiting itself is sliced ([`POLL_SLICE`]) and checked against the cancellation flag as well as the
//! deadline, which is the same shape `jean_model::http` and `jean_acp::watchdog` use: a cancel raised
//! during a blocked desktop call has to be *responsive*, not merely eventual.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the caller sleeps between checks while the worker is running.
///
/// Small enough that a cancel is seen promptly and a deadline is not overshot by a whole interval, large
/// enough that a fast call costs nothing measurable. Same reasoning and same number as
/// `crates/jean-model/src/http.rs`.
const POLL_SLICE: Duration = Duration::from_millis(25);

/// How many bounded calls are running on their own threads right now.
///
/// Counted for the process, because the honest limit is a resource limit: each outstanding call holds a
/// thread and its stack. Since D-051 the only caller is [`crate::input`], so this counts injected-input
/// calls rather than the UIA and capture calls it used to hold — and the leak it describes is a real one,
/// which is why the number says so: a call the caller abandoned is still counted until its thread actually
/// returns. Incremented before the spawn and decremented by the worker itself, so an answered call can
/// never leave the count high, and an abandoned thread is not forgotten when it later finishes.
///
/// Desktop *provider* work is no longer counted here: `jean_core::desktop::gate_workers` bounds it through
/// `worker::live_workers()` against `[desktop] max_concurrent_workers`, which is a count of child processes
/// that are alive, and those processes are terminated rather than carried.
static IN_FLIGHT: AtomicU32 = AtomicU32::new(0);

/// What a bounded call came back with.
#[derive(Debug)]
pub enum Outcome<T> {
    /// The worker answered inside the bound.
    Completed(T),
    /// The bound expired. The worker thread is still parked and is deliberately abandoned.
    TimedOut { waited: Duration },
    /// Cancellation was raised while waiting. The worker is still abandoned — the call itself cannot be
    /// retracted, and the caller must treat whatever it was doing as uncertain.
    Cancelled { waited: Duration },
    /// The worker panicked. Reported as its own case, because "the provider never answered" and "our code
    /// fell over" are different facts and a timeout would blame the wrong party.
    Panicked,
    /// No worker thread could be created at all, so nothing was ever attempted.
    ///
    /// Distinct from [`Outcome::Panicked`]: that one means a call was made and our own code died during
    /// it, whose side effects are then unknown. Here they are known to be none.
    NotStarted,
}

impl<T> Outcome<T> {
    /// The value, or the failure this outcome describes.
    ///
    /// `f` turns the waiting failures into the caller's error type, because a timeout means something
    /// different for a read than for an action and only the caller knows which.
    pub fn unwrap_or_else<E, F>(self, budget: Duration, f: F) -> Result<T, E>
    where
        F: FnOnce(Abandoned, Duration) -> E,
    {
        match self {
            Outcome::Completed(value) => Ok(value),
            Outcome::TimedOut { waited } => Err(f(Abandoned::TimedOut { waited }, budget)),
            Outcome::Cancelled { waited } => Err(f(Abandoned::Cancelled { waited }, budget)),
            Outcome::Panicked => Err(f(Abandoned::Panicked, budget)),
            Outcome::NotStarted => Err(f(Abandoned::NotStarted, budget)),
        }
    }
}

/// A waiting failure, before it is given the caller's vocabulary.
#[derive(Debug)]
pub enum Abandoned {
    TimedOut { waited: Duration },
    Cancelled { waited: Duration },
    Panicked,
    NotStarted,
}

/// How many desktop calls may be outstanding at the same time.
#[derive(Debug, Clone, Copy)]
pub struct WorkerBudget {
    limit: u32,
}

impl WorkerBudget {
    pub fn new(limit: u32) -> Self {
        Self {
            limit: limit.max(1),
        }
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Whether another call may be risked at all.
    ///
    /// Checked *before* spawning: the failure mode being managed is accumulation, and a spawn that has
    /// already happened is a thread that has already leaked.
    pub fn allows(&self) -> bool {
        in_flight() < self.limit
    }
}

/// Calls currently on their own threads, answered or abandoned alike.
pub fn in_flight() -> u32 {
    IN_FLIGHT.load(Ordering::SeqCst)
}

/// Run `work` on a fresh thread and wait for it for at most `budget`.
///
/// `cancel` is polled on the same slice as the deadline, so a cancellation raised while a provider is
/// thinking ends the *wait* — it does not end the call, which nothing can do, and the worker stays
/// counted against [`in_flight`] either way.
pub fn spawn_bounded<T, F>(
    label: &'static str,
    budget: Duration,
    cancel: Option<&AtomicBool>,
    work: F,
) -> Outcome<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    let spawn = {
        // Claimed before the thread exists, so a caller that gives up between the claim and the spawn
        // still owes nothing, and every caller that does get a worker has its decrement ahead of it.
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        std::thread::Builder::new()
            .name(format!("jean-desktop-{label}"))
            .spawn(move || {
                // A panic here must not look like a hung provider: `catch_unwind` turns it into a message
                // on the channel, and the count still comes down, because the thread has finished.
                let value = catch_unwind(AssertUnwindSafe(work));
                let _ = tx.send(value.ok());
                IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            })
    };
    let handle = match spawn {
        Ok(handle) => handle,
        Err(_) => {
            // No thread means no worker and nothing parked. Saying so beats reporting a call that never
            // started as one that hung.
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            return Outcome::NotStarted;
        }
    };

    loop {
        match rx.recv_timeout(POLL_SLICE) {
            Ok(Some(value)) => return Outcome::Completed(value),
            // The worker sent `None`: it panicked inside `work`. The thread has already finished.
            Ok(None) => return Outcome::Panicked,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The sender dropped without sending anything — the thread died during unwinding or was
            // killed. Either way, no answer is coming.
            Err(mpsc::RecvTimeoutError::Disconnected) => return Outcome::Panicked,
        }

        if let Some(flag) = cancel {
            if flag.load(Ordering::SeqCst) {
                drop(handle);
                return Outcome::Cancelled {
                    waited: started.elapsed(),
                };
            }
        }

        let waited = started.elapsed();
        if waited >= budget {
            drop(handle);
            return Outcome::TimedOut { waited };
        }
    }
}

/// The window into which one call's bound was already spent.
///
/// Used where one operation makes several bounded calls against a single overall deadline, so the second
/// call does not silently get the first one's budget again.
pub fn remaining(budget: Duration, spent: Duration) -> Duration {
    budget.saturating_sub(spent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The count is process-wide, so the tests that assert exact numbers must not overlap.
    ///
    /// Without this, a worker abandoned by one test is still parked while another reads [`in_flight`], and
    /// the assertion becomes a coin flip — the same class of flake this tree refuses to carry in
    /// `jean-process`.
    static SERIALISE: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        SERIALISE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wait for the abandoned worker above to be released, and fail loudly if it never is.
    ///
    /// The count is what the next call's budget depends on, so asserting on a number that has not settled
    /// would be asserting a race.
    fn settle(before: u32) {
        let mut settled = false;
        for _ in 0..80 {
            if in_flight() == before {
                settled = true;
                break;
            }
            std::thread::sleep(POLL_SLICE);
        }
        assert!(
            settled,
            "a released worker never came off the count: still {}",
            in_flight()
        );
    }

    #[test]
    fn a_call_that_answers_is_not_left_counted() {
        let _guard = lock();
        let before = in_flight();
        let outcome = spawn_bounded("answering", Duration::from_secs(5), None, || 7u8);
        assert!(matches!(outcome, Outcome::Completed(7)));
        settle(before);
    }

    /// The bound has to hold against something that genuinely never returns.
    ///
    /// A parked `recv` on a channel nobody writes to is the same shape as a provider that never answers,
    /// and unlike the real thing it can be created here without hanging the machine.
    #[test]
    fn a_call_that_never_answers_is_abandoned_and_still_counted() {
        let _guard = lock();
        let before = in_flight();
        let (tx, rx) = mpsc::channel::<()>();
        let outcome: Outcome<u8> =
            spawn_bounded("silent", Duration::from_millis(80), None, move || {
                // Blocking on a receiver kept alive by `tx` in this frame: nothing will ever arrive, which is
                // exactly the hung-provider condition.
                let _ = rx.recv();
                1u8
            });
        assert!(
            matches!(outcome, Outcome::TimedOut { .. }),
            "a silent worker must report the bound, not an answer"
        );
        assert_eq!(
            before + 1,
            in_flight(),
            "an abandoned worker is still parked, and must remain visible"
        );
        // Releasing it the way destroying the provider's window would: close the channel.
        drop(tx);
        settle(before);
    }

    #[test]
    fn a_panicking_worker_is_reported_as_a_panicked_worker() {
        let _guard = lock();
        let before = in_flight();
        let outcome = spawn_bounded::<u8, _>("panicking", Duration::from_secs(5), None, || {
            panic!("the provider script fell over")
        });
        assert!(matches!(outcome, Outcome::Panicked));
        settle(before);
    }

    #[test]
    fn cancellation_ends_the_wait_even_though_the_call_continues() {
        let _guard = lock();
        let before = in_flight();
        let flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<()>();
        let setter = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            setter.store(true, Ordering::SeqCst);
        });
        let outcome: Outcome<u8> =
            spawn_bounded("slow", Duration::from_secs(30), Some(&flag), move || {
                let _ = rx.recv();
                2u8
            });
        assert!(
            matches!(outcome, Outcome::Cancelled { .. }),
            "a cancel must end the wait long before the 30-second bound"
        );
        assert_eq!(
            before + 1,
            in_flight(),
            "the abandoned call is still running"
        );
        drop(tx);
        settle(before);
    }

    #[test]
    fn the_budget_refuses_before_spawning_another_leak() {
        let _guard = lock();
        let before = in_flight();
        let budget = WorkerBudget::new(1);
        assert!(
            budget.allows(),
            "one slot of headroom before anything is risked"
        );
        let (tx, rx) = mpsc::channel::<()>();
        let _ = spawn_bounded("one", Duration::from_millis(60), None, move || {
            let _ = rx.recv();
        });
        assert!(
            !budget.allows(),
            "the cap is reached while the worker is parked"
        );
        drop(tx);
        settle(before);
        assert!(budget.allows());
    }

    #[test]
    fn remaining_time_never_goes_negative() {
        assert_eq!(
            Duration::ZERO,
            remaining(Duration::from_millis(50), Duration::from_millis(90))
        );
    }
}
