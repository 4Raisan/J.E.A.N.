//! Window-station inspection: the filters, the bounds, and the three-way wait.
//!
//! These tests drive [`jean_tools::WindowStation`], the seam the real Win32 walk sits
//! behind, rather than the live desktop. That is not a shortcut — it is the only honest
//! option for two of the three things being asserted. The wait logic has to distinguish
//! "the process exited before it painted" from "the process is slow to paint", and the
//! only way to observe that without racing a real application is to script it. A test
//! that launched a real window would be asserting about whatever happened to be on the
//! screen, at whatever speed, on somebody else's machine; a test that killed a real
//! process to force the `PidGone` branch would be doing the one thing this crate forbids.
//!
//! What *is* asserted against the live system, in `the_test_process_...` and
//! `a_known_absent_pid_...`, is the part that needs no fixture and no third-party
//! application: this process's own pid, which owns no windows, and a pid proven absent
//! by `jean_tools::process::unused_pid`.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use jean_tools::desktop::{
    any_window_for_pid, clamp_poll, counts_as_operator_window, pid_is_running, wait_for_window,
    wait_for_window_with, windows_for_pid, windows_for_pid_with, RawWindow, SystemWindowStation,
    WindowStation, WindowWait, MAX_POLL, MAX_TITLE_CHARS, MAX_WINDOWS_REPORTED, MIN_POLL,
};
use jean_tools::{Result, ToolError};

const TARGET: u32 = 4_242;

/// A top-level window that passes every filter.
fn visible(pid: u32, hwnd: isize, title: &str) -> RawWindow {
    RawWindow {
        pid,
        hwnd,
        thread_id: 7,
        title: title.to_owned(),
        is_visible: true,
        is_minimised: false,
        is_tool_window: false,
        has_owner: false,
    }
}

/// A window station whose entire behavior is scripted.
///
/// `reveal` holds one snapshot per attempt; the last entry repeats, so a fixture that
/// wants "nothing, nothing, then a window" is three entries and a fixture that wants
/// "nothing forever" is one.
struct Fixture {
    reveal: RefCell<Vec<Vec<RawWindow>>>,
    attempts: Cell<usize>,
    refuses: bool,
    alive: bool,
}

impl Fixture {
    fn new(windows: Vec<RawWindow>) -> Self {
        Fixture {
            reveal: RefCell::new(vec![windows]),
            attempts: Cell::new(0),
            refuses: false,
            alive: true,
        }
    }

    fn script(reveals: Vec<Vec<RawWindow>>) -> Self {
        assert!(
            !reveals.is_empty(),
            "a fixture with no snapshot has no answer"
        );
        Fixture {
            reveal: RefCell::new(reveals),
            attempts: Cell::new(0),
            refuses: false,
            alive: true,
        }
    }

    /// The station itself cannot be read. Mirrors this build's real state.
    fn refusing() -> Self {
        Fixture {
            reveal: RefCell::new(vec![vec![]]),
            attempts: Cell::new(0),
            refuses: true,
            alive: true,
        }
    }

    /// The process is gone. Reaching this branch needs a process that has exited, and
    /// this crate does not kill things to make tests pass.
    fn dead() -> Self {
        Fixture {
            alive: false,
            ..Fixture::new(vec![])
        }
    }
}

impl WindowStation for Fixture {
    fn top_level_windows(&self) -> Result<Vec<RawWindow>> {
        let attempt = self.attempts.get();
        self.attempts.set(attempt + 1);
        if self.refuses {
            return Err(ToolError::ProcessQuery {
                pid: TARGET,
                reason: "the fixture station refuses to answer".to_owned(),
            });
        }
        let snapshots = self.reveal.borrow();
        let index = attempt.min(snapshots.len() - 1);
        Ok(snapshots[index].clone())
    }

    fn pid_is_running(&self, _pid: u32) -> bool {
        self.alive
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The filter.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn an_invisible_or_untitled_window_is_not_one_the_operator_can_see() {
    let hidden = RawWindow {
        is_visible: false,
        ..visible(TARGET, 1, "Hidden plumbing")
    };
    let untitled = visible(TARGET, 2, "");
    let blank_title = visible(TARGET, 3, "   ");

    assert!(!counts_as_operator_window(&hidden));
    assert!(!counts_as_operator_window(&untitled));
    assert!(!counts_as_operator_window(&blank_title));
    assert!(counts_as_operator_window(&visible(TARGET, 4, "Report.pdf")));
}

#[test]
fn an_orphan_tool_window_is_rejected_and_a_belonging_one_is_kept() {
    // The distinction the Phase-1 launch check turns on: a floating palette belongs to
    // an application, a freestanding tool window belongs to nothing.
    let orphan = RawWindow {
        is_tool_window: true,
        ..visible(TARGET, 1, "IME candidate")
    };
    let owned_palette = RawWindow {
        is_tool_window: true,
        has_owner: true,
        ..visible(TARGET, 2, "Brushes")
    };

    assert!(!counts_as_operator_window(&orphan));
    assert!(counts_as_operator_window(&owned_palette));
}

#[test]
fn a_minimised_window_is_reported_rather_than_filtered_out() {
    // A launch that comes up minimised succeeded; dropping the window here would turn
    // that into "no window appeared", which is a different and wrong sentence.
    let minimized = RawWindow {
        is_minimised: true,
        ..visible(TARGET, 1, "Notes")
    };
    assert!(counts_as_operator_window(&minimized));

    let fixture = Fixture::new(vec![minimized.clone()]);
    let report = windows_for_pid_with(&fixture, TARGET, MAX_WINDOWS_REPORTED)
        .expect("a scripted station answers");
    assert_eq!(report.windows.len(), 1);
    assert!(report.windows[0].is_minimised);
    assert!(
        report.all_minimised(),
        "the caller must be able to say 'it came up in the background'"
    );
}

#[test]
fn windows_belonging_to_other_processes_are_never_attributed_to_the_target() {
    let fixture = Fixture::new(vec![
        visible(TARGET, 1, "Ours"),
        visible(TARGET + 1, 2, "Not ours"),
        visible(1, 3, "The system"),
    ]);
    let report =
        windows_for_pid_with(&fixture, TARGET, MAX_WINDOWS_REPORTED).expect("the fixture answers");

    assert_eq!(report.windows.len(), 1);
    assert_eq!(report.windows[0].title, "Ours");
    // The walk saw three, kept one: "nothing matched" and "there was nothing to match"
    // stay different answers.
    assert_eq!(report.windows_examined, 3);
    assert_eq!(report.windows_matched, 1);
    assert!(!report.truncated);
}

// ─────────────────────────────────────────────────────────────────────────────
// The bounds.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn more_windows_than_the_row_bound_returns_the_bound_and_says_it_was_hit() {
    let mut windows = Vec::new();
    for index in 0..(MAX_WINDOWS_REPORTED + 44) {
        windows.push(visible(TARGET, index as isize, &format!("Window {index}")));
    }
    let fixture = Fixture::new(windows);
    let report =
        windows_for_pid_with(&fixture, TARGET, MAX_WINDOWS_REPORTED).expect("the fixture answers");

    assert_eq!(report.windows.len(), MAX_WINDOWS_REPORTED);
    assert_eq!(report.windows_matched, MAX_WINDOWS_REPORTED + 44);
    assert!(
        report.truncated,
        "a cap that is invisible in the type is a cap that gets silently dropped"
    );
    assert_eq!(report.max_rows, MAX_WINDOWS_REPORTED);
}

#[test]
fn a_caller_chosen_bound_truncates_without_losing_the_count() {
    let windows: Vec<RawWindow> = (0..9)
        .map(|index| visible(TARGET, index, &format!("Doc {index}")))
        .collect();
    let fixture = Fixture::new(windows);
    let report = windows_for_pid_with(&fixture, TARGET, 5).expect("the fixture answers");

    assert_eq!(report.windows.len(), 5);
    assert_eq!(report.windows_matched, 9);
    assert!(report.truncated);
}

#[test]
fn a_zero_row_bound_answers_the_yes_no_question_and_still_counts() {
    let fixture = Fixture::new(vec![visible(TARGET, 1, "Only one")]);
    let report = windows_for_pid_with(&fixture, TARGET, 0).expect("the fixture answers");

    assert!(report.windows.is_empty());
    assert_eq!(
        report.windows_matched, 1,
        "asking for no rows must not be reported as finding no window"
    );
    assert!(report.truncated);
    assert!(
        report.has_window(),
        "the yes/no answer is read from the match count, so a row bound cannot flip it"
    );
}

#[test]
fn titles_are_bounded_in_characters_and_the_cut_is_reported_separately() {
    let long = "a".repeat(MAX_TITLE_CHARS + 60);
    let fixture = Fixture::new(vec![visible(TARGET, 1, &long)]);
    let report =
        windows_for_pid_with(&fixture, TARGET, MAX_WINDOWS_REPORTED).expect("the fixture answers");
    let row = &report.windows[0];

    assert_eq!(row.title.chars().count(), MAX_TITLE_CHARS);
    assert!(row.title_truncated);

    // Non-ASCII at the boundary: the bound is in characters, so a two-byte title must
    // cut on a character boundary and must not panic. `[..n]` on a byte count would.
    let wide = "é".repeat(MAX_TITLE_CHARS + 7);
    let fixture = Fixture::new(vec![visible(TARGET, 2, &wide)]);
    let report =
        windows_for_pid_with(&fixture, TARGET, MAX_WINDOWS_REPORTED).expect("the fixture answers");
    assert_eq!(report.windows[0].title.chars().count(), MAX_TITLE_CHARS);
    assert!(report.windows[0].title_truncated);

    // A title that exactly fits is not flagged.
    let exact = "b".repeat(MAX_TITLE_CHARS);
    let fixture = Fixture::new(vec![visible(TARGET, 3, &exact)]);
    let report =
        windows_for_pid_with(&fixture, TARGET, MAX_WINDOWS_REPORTED).expect("the fixture answers");
    assert!(!report.windows[0].title_truncated);
    assert!(!report.truncated);
}

// ─────────────────────────────────────────────────────────────────────────────
// The wait: three outcomes that must not collapse into "it failed".
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_window_that_appears_on_the_third_attempt_is_found() {
    let fixture = Fixture::script(vec![
        vec![],
        vec![],
        vec![visible(TARGET, 1, "Untitled - Editor")],
    ]);
    let wait = wait_for_window_with(&fixture, TARGET, Duration::from_secs(2), MIN_POLL);

    match wait {
        WindowWait::Found(report) => {
            assert_eq!(report.pid, TARGET);
            assert_eq!(report.windows.len(), 1);
            assert_eq!(report.windows[0].title, "Untitled - Editor");
        }
        other => panic!("expected Found, got {other:?}"),
    }
    assert_eq!(fixture.attempts.get(), 3, "and it took three polls");
}

#[test]
fn a_process_that_exited_before_painting_is_reported_gone_not_slow() {
    // The single-instance case: the launched pid hands off to an existing window and
    // dies. This must arrive as `PidGone`, never as a timeout, because the honest
    // follow-up is "your existing window was used", not "try launching again".
    let fixture = Fixture::dead();
    let wait = wait_for_window_with(&fixture, TARGET, Duration::from_secs(5), MIN_POLL);

    match wait {
        WindowWait::PidGone {
            pid,
            attempts,
            waited,
        } => {
            assert_eq!(pid, TARGET);
            assert_eq!(attempts, 1);
            assert!(
                waited < Duration::from_millis(1_000),
                "a 40 ms exit has to look different from a 5 s stall: {waited:?}"
            );
        }
        other => panic!("expected PidGone, got {other:?}"),
    }
}

#[test]
fn a_live_process_with_no_window_reaches_the_deadline() {
    let fixture = Fixture::new(vec![]);
    let wait = wait_for_window_with(&fixture, TARGET, Duration::from_millis(80), MIN_POLL);

    match wait {
        WindowWait::DeadlineReached {
            pid,
            poll,
            waited,
            attempts,
        } => {
            assert_eq!(pid, TARGET);
            assert_eq!(poll, MIN_POLL);
            assert!(attempts >= 2, "the poll must have run, not just glanced");
            // The deadline is honoured, not overshot by a whole cadence.
            assert!(
                waited < Duration::from_millis(200),
                "waited {waited:?} for an 80 ms deadline"
            );
        }
        other => panic!("expected DeadlineReached, got {other:?}"),
    }
}

#[test]
fn an_exited_process_that_still_owns_a_window_is_found_not_gone() {
    // Ordering matters: enumerate before checking liveness, so a window that exists is
    // never announced as "the process is gone" because of a race between the questions.
    let fixture = Fixture {
        alive: false,
        ..Fixture::new(vec![visible(TARGET, 1, "Dying but visible")])
    };
    let wait = wait_for_window_with(&fixture, TARGET, Duration::from_secs(1), MIN_POLL);
    assert!(matches!(wait, WindowWait::Found(_)), "got {wait:?}");
}

#[test]
fn a_station_that_cannot_look_never_becomes_a_deadline_or_an_empty_list() {
    // The whole honesty of this module in one test: inability to look is its own
    // outcome, not "no windows exist".
    let fixture = Fixture::refusing();
    let wait = wait_for_window_with(&fixture, TARGET, Duration::from_secs(30), MIN_POLL);

    match &wait {
        WindowWait::ProbeUnavailable { pid, reason, .. } => {
            assert_eq!(*pid, TARGET);
            assert!(reason.contains("refuses"), "got {reason}");
        }
        other => panic!("expected ProbeUnavailable, got {other:?}"),
    }
    assert!(wait.is_unavailable());
    assert_eq!(
        fixture.attempts.get(),
        1,
        "a 30 s timeout must not be spent rediscovering a permanent refusal"
    );

    let error = windows_for_pid_with(&fixture, TARGET, MAX_WINDOWS_REPORTED)
        .expect_err("a refusing station reports, it does not return an empty list");
    assert!(error.to_string().contains(TARGET.to_string().as_str()));
}

// ─────────────────────────────────────────────────────────────────────────────
// Poll clamping: a caller passing zero must not spin a core.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_zero_poll_is_clamped_to_the_floor_and_the_used_cadence_is_echoed() {
    assert_eq!(clamp_poll(Duration::ZERO), MIN_POLL);
    assert_eq!(clamp_poll(MIN_POLL), MIN_POLL);
    assert_eq!(clamp_poll(MAX_POLL), MAX_POLL);
    assert_eq!(clamp_poll(Duration::from_secs(3_600)), MAX_POLL);
    assert!(MIN_POLL < MAX_POLL);

    let fixture = Fixture::new(vec![]);
    let wait = wait_for_window_with(&fixture, TARGET, Duration::ZERO, Duration::ZERO);
    match wait {
        WindowWait::DeadlineReached { poll, attempts, .. } => {
            assert_eq!(poll, MIN_POLL, "the report names the cadence it used");
            assert_eq!(
                attempts, 1,
                "a zero timeout is one immediate look, which is a legal question"
            );
        }
        other => panic!("expected DeadlineReached, got {other:?}"),
    }

    // A huge poll is bounded the same way, so a window that opened and closed inside
    // one sleep cannot be missed indefinitely.
    let fixture = Fixture::new(vec![]);
    let wait = wait_for_window_with(&fixture, TARGET, Duration::ZERO, Duration::from_secs(3_600));
    match wait {
        WindowWait::DeadlineReached { poll, .. } => assert_eq!(poll, MAX_POLL),
        other => panic!("expected DeadlineReached, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Against the live system: only what needs no third-party application.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn the_test_process_itself_owns_no_windows_and_that_is_an_answer_not_a_failure() {
    // The cargo test binary has no window. Whatever this build can or cannot enumerate,
    // the one thing it must never do is produce a window for a process that has none —
    // that single-instance false positive is what makes a launch check worthless.
    let pid = std::process::id();
    match windows_for_pid(pid) {
        Ok(report) => {
            assert_eq!(report.pid, pid);
            assert!(
                report.windows.is_empty(),
                "the test process owns no window; got {:?}",
                report.windows
            );
            assert_eq!(report.windows_matched, 0);
            // `windows_examined` may be large: a desktop always has top-level windows.
            // It is reported either way, so this line documents the field's meaning.
        }
        Err(error) => {
            // The other legal answer, and the one this build currently takes: it could
            // not look, and says so instead of returning the empty list above.
            let text = error.to_string();
            assert!(
                text.contains("Win32_UI_WindowsAndMessaging"),
                "an error from the window probe must name the missing feature, got: {text}"
            );
        }
    }

    // `false` here is not proof of absence — it is either "no window" or "could not
    // look", and both are consistent with the match above.
    assert!(!any_window_for_pid(pid));

    let wait = wait_for_window(pid, Duration::from_millis(60), MIN_POLL);
    assert!(
        !matches!(wait, WindowWait::Found(_)),
        "a windowless process must never be reported as Found: {wait:?}"
    );
}

#[test]
fn a_known_absent_pid_is_not_running_and_this_process_is() {
    // Liveness is the half of this module that works today, so it is tested for real.
    let absent = jean_tools::process::unused_pid().expect("a pid can be proven absent");
    assert!(
        !pid_is_running(absent),
        "pid {absent} was proven unclaimed and must not read as alive"
    );
    assert!(
        pid_is_running(std::process::id()),
        "this test is running, so its own pid is alive"
    );
    // Pid 0 is not a process, and the query is refused before the syscall rather than
    // answered by whatever the OS does with it.
    assert!(!pid_is_running(0));
}

#[test]
fn the_default_liveness_of_a_station_is_the_real_query() {
    // A station that implements only the window half inherits the production liveness
    // answer. Asserting the *default* body specifically matters: if it ever degraded to
    // `true`, every `PidGone` branch would become unreachable in production and the
    // single-instance case would silently turn back into a timeout.
    struct WindowsOnly;
    impl WindowStation for WindowsOnly {
        fn top_level_windows(&self) -> Result<Vec<RawWindow>> {
            Ok(vec![])
        }
    }

    let absent = jean_tools::process::unused_pid().expect("a pid can be proven absent");
    assert!(
        !WindowsOnly.pid_is_running(absent),
        "the inherited default must answer the real question"
    );
    assert!(
        WindowsOnly.pid_is_running(std::process::id()),
        "the inherited default must see this running process"
    );

    // And a fixture that *does* override it keeps its own answer, which is what makes
    // the `PidGone` tests above real rather than decorative.
    assert!(!Fixture::dead().pid_is_running(std::process::id()));
    assert!(Fixture::new(vec![]).pid_is_running(absent));
}

#[test]
fn the_production_station_is_read_only_and_holds_nothing() {
    // `SystemWindowStation` is a unit struct, so it cannot own a window handle, a
    // process handle, or a thread id: there is no field that could be used to act on
    // anything, and no lifetime question about when a handle closes. This test exists
    // so that adding a stored handle later is a deliberate edit rather than an accident.
    let station = SystemWindowStation;
    let copy = station;
    let _ = format!("{station:?} {copy:?}");
    assert!(
        std::mem::size_of::<SystemWindowStation>() == 0,
        "the production station must stay stateless"
    );
}
