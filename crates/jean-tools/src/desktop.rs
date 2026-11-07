//! Window-station inspection: which process owns a window an operator can see.
//!
//! ## What this is for
//!
//! Phase 1's `jean launch` has to be able to say *it worked*, and "I called
//! `CreateProcess` and it returned" is not that. The verifiable claim is **a window
//! belonging to the expected process id exists within N seconds**. This module owns
//! exactly that question — and the half nobody thinks about until it goes wrong:
//! single-instance applications. Re-launching a browser or Outlook hands back a new
//! pid that exits within milliseconds while an already-running process's window takes
//! focus. So "no window appeared" and "the process you launched is already gone" are
//! different facts, and collapsing them produces a wrong diagnosis every time.
//! [`WindowWait`] keeps them apart.
//!
//! ## The contract, applied here
//!
//! **This module is read-only against the window station.** There is no
//! `SetForegroundWindow`, no `AttachThreadInput`, no `AllowSetForegroundWindow`, no
//! `ShowWindow`, no `SetWindowPos`, no `CloseWindow`, no `PostMessage` — not because
//! they were not thought of, but because they were considered and refused.
//!
//! The foreground lock is a documented Windows behavior: a process that is not the
//! foreground process cannot simply take it, and the usual ways of *forcing* it
//! (attaching thread input to the foreground thread, simulating an Alt keystroke,
//! `AllowSetForegroundWindow` dances) are exactly the techniques malware uses, and
//! `jean-policy` has no rule that can tell the two apart. A probe that grabbed
//! foreground would be an actuator wearing a disguise, and the Phase-4 actuator is the
//! only place that belongs. If you are about to add one of those calls here: don't.
//! It is a design change, not a feature.
//!
//! Two further refusals, both load-bearing:
//!
//! * **Minimising, restoring, closing or enumerating-then-touching a window is not
//!   done, ever, including to "help" the operator.** Reading `IsIconic` is inspection;
//!   clearing it is an act.
//! * **A window's *title* is untrusted text**, and it is reported as such. Another
//!   process wrote those characters; they are bounded and lossy-decoded, never
//!   interpreted, never used as an identity key by this module.
//!
//! ## What counts as "a window the operator can see"
//!
//! `EnumWindows` walks *every* top-level window in the window station, and the
//! overwhelming majority are not something a human would call a window of an
//! application: the shell's taskbar button host, IME candidate windows, cross-process
//! drag helpers, per-thread message windows, and a long tail of zero-size invisible
//! plumbing. Left in, they make `any_window_for_pid` true for processes the operator
//! would say have no window at all, which turns a launch verification into a tautology.
//! So [`counts_as_operator_window`] requires three things, each with a reason:
//!
//! | Filter | Why |
//! |---|---|
//! | `IsWindowVisible` | A window with the visible style bit *and* every ancestor visible. Without this, hidden plumbing and minimized-to-nothing helper windows count. Note this is not "shown"; a window can be visible and fully off-screen. |
//! | title is not empty after trimming | The only cheap signal that a window is a document/app frame rather than a message-only or owner-less helper. Windows that deliberately carry `" "` instead of `""` are included by the trim, and they are the classic invisible-but-visible trick. |
//! | not a tool window, **unless** it has an owner | `WS_EX_TOOLWINDOW` marks a window that is not meant to appear in Alt+Tab. A floating palette or a modeless dialog *is* a tool window and *is* part of an application the operator sees — but only when it belongs to a real window, so `has_owner` lets those through and drops the freestanding ones. This is the same rule the shell uses to decide Alt+Tab membership; deviating from it makes this probe disagree with what Alt+Tab shows. |
//!
//! Minimised windows **pass** the filter. A launched app that opens into the
//! background is a successful launch with a caveat, not a failed one, so
//! [`WindowInfo::is_minimised`] is reported instead of filtering on it — and the caller
//! gets to say "it launched, but minimized" rather than being silently handed an empty
//! list.
//!
//! ## Bounds, reported rather than hidden
//!
//! [`MAX_WINDOWS_REPORTED`] caps the rows a single report carries: a pathological or
//! hostile process can own an enormous number of top-level windows, and there is no
//! version of a million-row report that is useful. Hitting the cap sets
//! [`WindowReport::truncated`] and the rows still come back — the same shape
//! [`crate::process::ProcessListing`] uses. Titles are capped at [`MAX_TITLE_CHARS`]
//! with [`WindowInfo::title_truncated`] saying so. The walk itself is bounded by
//! [`WindowReport::windows_examined`], so "no windows" and "I looked at 4 000 windows
//! and none were yours" stay distinguishable.
//!
//! ## Liveness
//!
//! [`pid_is_running`] answers "is this pid currently executing" with
//! `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess`, mirroring
//! `jean-process`'s `process_is_running`: **a process that has exited but is still
//! referenced by an open handle is not running**, and `OpenProcess` succeeds against
//! it, so a handle-only check would report an app that already died as alive. `STILL_ACTIVE`
//! is the only code that means alive; a query that fails is treated as dead, because
//! "cannot tell" must never be reported as "it is fine".
//!
//! Not sysinfo: the crate-internal helper in [`crate::process`] answers presence by
//! refreshing the *entire* process table with `exe` resolution, which is a handle query
//! per process and far too expensive inside a 50 ms poll loop. `sysinfo` remains the
//! right tool for the rich reports that module produces.
//!
//! **One caveat to carry, not to hide:** a pid is recycled. `pid_is_running(4210)` says
//! *some* process owns 4210, not that the process you launched does. `jean-process`
//! pairs a pid with a creation time to get identity
//! (`is_same_process_alive`); this probe is handed a pid and nothing else, so it cannot.
//! Callers that need identity must compare against the creation time they recorded at
//! launch, which is what the single-instance case above actually turns on.
//!
//! ## What is real, and what is bounded
//!
//! The walk is the live `EnumWindows` path: [`SystemWindowStation`] enumerates the
//! session's top-level windows, reading the owning process and thread, the extended
//! style, the owner link, visibility, iconic state and title for each. It needs the
//! `Win32_UI_WindowsAndMessaging` feature of the `windows` crate, which is declared at
//! the workspace root; if that ever stops being true the walk must fail **loudly**, the
//! way it did before the feature was added:
//!
//! * `top_level_windows` returns `Err`, never an empty list — an empty list is the
//!   answer "the process has no window", and claiming that from a probe that could not
//!   look is precisely the failure this crate's docs are written against.
//! * Consequently [`any_window_for_pid`] reports `false` (see its doc: `false` is not
//!   proof of absence) and [`wait_for_window`] returns [`WindowWait::ProbeUnavailable`]
//!   on the first attempt rather than spending the caller's whole timeout arriving at a
//!   known answer.
//!
//! Two bounds the caller has to be able to see, both on [`WindowReport`]:
//!
//! * [`WindowReport::truncated`] — more windows matched `pid` than the caller's row
//!   bound could carry.
//! * [`WindowReport::walk_capped`] — the *station* was not fully examined, because
//!   [`MAX_WINDOWS_EXAMINED`] stopped the walk. `windows_matched == 0` on such a report
//!   means "not seen", not "absent".
//!
//! [`pid_is_running`] is fully functional; liveness and window lookup are separate
//! questions and only the second one can be capped.

use std::time::{Duration, Instant};

use crate::error::{Result, ToolError};

/// Upper bound on the windows one report carries.
///
/// Chosen well above any real application (a heavy IDE might own a few dozen top-level
/// windows) and far below anything a terminal can render, so hitting it means the
/// process is doing something pathological rather than something unusual.
pub const MAX_WINDOWS_REPORTED: usize = 256;

/// Upper bound on a reported title, in `char`s.
///
/// Titles are attacker-controlled text written by another process. 512 is generous
/// against a real document title and keeps a log line one line. Counted in `char`s,
/// not bytes, so truncation can never split a UTF-8 sequence.
pub const MAX_TITLE_CHARS: usize = 512;

/// Floor for a poll interval.
///
/// A caller passing `Duration::ZERO` would otherwise spin a core for the whole timeout
/// and hammer the window station — `EnumWindows` is not free, and it locks nothing but
/// does cost every iteration. 25 ms is below human perception of latency and above the
/// cost of one walk, so a launched application is still caught the moment it paints.
pub const MIN_POLL: Duration = Duration::from_millis(25);

/// Ceiling for a poll interval.
///
/// Above this the caller has stopped describing a launch timeout and started describing
/// a nap, and the verification would miss windows that appeared and closed between two
/// sleeps. The effective value is echoed back as `poll` in
/// [`WindowWait::DeadlineReached`] so the report never claims a cadence it did not use.
pub const MAX_POLL: Duration = Duration::from_millis(1_000);

/// The exit code `GetExitCodeProcess` reports while a process is still running.
///
/// Named here rather than trusted from memory of the header: any other value is a real
/// exit code, and the difference between "259" and "0" is the difference between
/// "still starting up" and "exited cleanly", which the caller must not be told are the
/// same thing.
#[cfg(windows)]
pub const STILL_ACTIVE: u32 = 259;

// ─────────────────────────────────────────────────────────────────────────────
// Types.
// ─────────────────────────────────────────────────────────────────────────────

/// One unfiltered top-level window, as reported by a [`WindowStation`].
///
/// The raw facts, before [`counts_as_operator_window`] and before title shaping. It is
/// public because the seam that produces it is public: a test, or a caller with its own
/// idea of "visible", needs to be able to name one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWindow {
    /// The process that owns the window, from `GetWindowThreadProcessId`.
    pub pid: u32,
    /// The window handle as an opaque number. `0` is never produced by a real walk.
    pub hwnd: isize,
    /// The thread that created the window. Cheap — it is the same call that yields
    /// `pid` — and the caller needs it to reason about input attach *elsewhere*; this
    /// module does nothing with it.
    pub thread_id: u32,
    /// Lossy-decoded title. May be empty; that is a fact, not a failure.
    pub title: String,
    pub is_visible: bool,
    pub is_minimised: bool,
    /// `WS_EX_TOOLWINDOW`.
    pub is_tool_window: bool,
    /// Whether the window has an owner. Paired with `is_tool_window` by
    /// [`counts_as_operator_window`]; never read on its own.
    pub has_owner: bool,
}

/// One window the operator can see, shaped for reporting.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub pid: u32,
    /// Opaque handle value, for logs and for a later call that *does* own the right to
    /// act on it. This module never uses it again.
    pub hwnd: isize,
    pub thread_id: u32,
    /// Never longer than [`MAX_TITLE_CHARS`] `char`s; see `title_truncated`.
    pub title: String,
    /// Always `true` in a report produced by this module — the field exists so a row
    /// stays meaningful if a caller widens the filter, and so the report says what it
    /// checked rather than leaving it implied.
    pub is_visible: bool,
    /// `true` when minimised. Reported, not filtered on: see the module docs.
    pub is_minimised: bool,
    /// `true` when `title` was cut at [`MAX_TITLE_CHARS`].
    ///
    /// A separate flag rather than an ellipsis appended into the text, because the
    /// ellipsis would be indistinguishable from a title that genuinely ends in that
    /// character, and a caller diffing titles would compare against a string that was
    /// never on screen.
    pub title_truncated: bool,
}

/// A bounded answer to "which windows does this process own".
#[derive(Debug, Clone, PartialEq)]
pub struct WindowReport {
    pub pid: u32,
    /// The windows that passed the filter, in enumeration order, at most
    /// `max_rows` of them.
    pub windows: Vec<WindowInfo>,
    /// `true` when more windows passed the filter than `max_rows` allowed.
    ///
    /// The rows returned are still valid — this says the coverage is incomplete, not
    /// that it is wrong.
    pub truncated: bool,
    /// Top-level windows this source reported for the whole station, before the pid
    /// filter. This is what makes "nothing matched" and "there was nothing to match"
    /// different answers.
    pub windows_examined: usize,
    /// Windows of `pid` that passed the filter, before the row bound cut them. Equal to
    /// `windows.len()` unless `truncated`.
    pub windows_matched: usize,
    /// The row bound actually applied, echoed from the caller or
    /// [`MAX_WINDOWS_REPORTED`].
    pub max_rows: usize,
    /// `true` when the *walk* stopped at [`MAX_WINDOWS_EXAMINED`] rather than finishing
    /// the station.
    ///
    /// Distinct from [`WindowReport::truncated`], which is about the rows this caller
    /// asked to carry: a capped walk means a window belonging to `pid` may exist that
    /// was never looked at, so `windows_matched == 0` on such a report is "not seen",
    /// not "absent". A fixture source that returns exactly the cap by coincidence sets
    /// this too, which is the safe direction — it asks a question rather than making one.
    pub walk_capped: bool,
}

impl WindowReport {
    /// True when this process owns at least one window the operator can see.
    ///
    /// Reads [`WindowReport::windows_matched`] rather than [`WindowReport::windows`], so
    /// a row bound can never turn a yes into a no: reaching
    /// [`MAX_WINDOWS_REPORTED`], or asking for zero rows, changes what is *carried* and
    /// not what *is*. The rows are the report; this is the answer.
    pub fn has_window(&self) -> bool {
        self.windows_matched > 0
    }

    /// True when at least one row is present and every row in this report is minimised.
    ///
    /// The useful question for a launch: a process that opened only minimised windows
    /// succeeded but is not in front of the operator, and "it didn't appear" is the
    /// wrong thing to conclude from that. Judged over the rows carried, so a truncated
    /// report cannot claim it.
    pub fn all_minimised(&self) -> bool {
        !self.windows.is_empty() && self.windows.iter().all(|w| w.is_minimised)
    }
}

/// The outcome of waiting for a launched process to own a window.
///
/// The three states a caller might collapse into "it failed" mean different things and
/// lead to different fixes, so they are different variants:
///
/// * [`WindowWait::PidGone`] — the process you launched is no longer running. For a
///   single-instance application this is **expected**, not an error: the new instance
///   handed its arguments to the running one and exited. The caller's honest report is
///   "your existing window was used", which needs a *name*-based or already-running-pid
///   based follow-up, not a retry.
/// * [`WindowWait::DeadlineReached`] — the process is alive and has no window yet. Slow
///   first-run, a splash screen, a EULA, or a crash dialog that owns no titled window.
///   Retrying the wait is legitimate; retrying the *launch* is not.
/// * [`WindowWait::ProbeUnavailable`] — this build could not look. Not a statement
///   about the application at all, and it must never be printed as one.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowWait {
    /// A window appeared. Carries the report, so the caller can name the title, say
    /// whether it came up minimised, and report a bound it hit.
    Found(WindowReport),
    /// The pid stopped running before any window appeared.
    PidGone {
        pid: u32,
        /// Time actually spent before noticing. Short on purpose: distinguishing a
        /// 40 ms exit from a 5 s one is how a crash gets told apart from a
        /// single-instance handoff.
        waited: Duration,
        attempts: usize,
    },
    /// The deadline passed with the process alive and window-less.
    DeadlineReached {
        pid: u32,
        waited: Duration,
        /// The clamped interval actually used.
        poll: Duration,
        attempts: usize,
    },
    /// The window station could not be read at all.
    ProbeUnavailable {
        pid: u32,
        reason: String,
        waited: Duration,
    },
}

impl WindowWait {
    /// The report when a window was found.
    pub fn report(&self) -> Option<&WindowReport> {
        match self {
            WindowWait::Found(r) => Some(r),
            _ => None,
        }
    }

    /// True when this build could not answer the question, as opposed to answering it
    /// negatively.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, WindowWait::ProbeUnavailable { .. })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The seam.
// ─────────────────────────────────────────────────────────────────────────────

/// Where the top-level window facts come from.
///
/// One trait carrying both questions [`wait_for_window`] asks, because they must be
/// answerable together and consistently: a fixture that fakes windows but not liveness
/// would test the classification against half-real behavior, which is the kind of test
/// that passes while lying.
///
/// This exists so the filters, the bounds and the whole [`WindowWait`] state machine
/// are production code under real test, rather than logic that only ever runs against a
/// live desktop and is therefore never asserted about. The production implementation is
/// [`SystemWindowStation`]; the walk itself is dark in this build (see the module docs).
pub trait WindowStation {
    /// Every top-level window in the station, unfiltered.
    ///
    /// Enumeration order is whatever the window station gives; this module does not
    /// sort, because a stable order across two calls is not something the OS promises
    /// and pretending otherwise would make a report look reproducible when it is not.
    fn top_level_windows(&self) -> Result<Vec<RawWindow>>;

    /// Is `pid` currently executing?
    ///
    /// The default is the real Win32 query. A fixture overrides it so "the process
    /// exited at attempt 3" is expressible without killing anything.
    fn pid_is_running(&self, pid: u32) -> bool {
        pid_is_running(pid)
    }
}

/// The real window station of this session.
///
/// Read-only, and holds no handle to anything: no process handle outlives a call, no
/// window handle is ever kept.
#[derive(Debug, Clone, Copy)]
pub struct SystemWindowStation;

impl WindowStation for SystemWindowStation {
    fn top_level_windows(&self) -> Result<Vec<RawWindow>> {
        top_level_windows()
    }
}

/// `MAX_WINDOWS_EXAMINED` windows, and no more, are collected from one walk.
///
/// A process on a workstation rarely owns more than a few dozen top-level windows, and
/// a window *station* with 8 192 of them is either a machine doing something unusual or
/// something trying to make this probe expensive. The bound exists so a caller cannot be
/// stalled by the desktop; hitting it is reported through
/// [`WindowReport::windows_examined`] and [`WindowReport::walk_capped`] rather than
/// being allowed to look like a complete answer.
pub const MAX_WINDOWS_EXAMINED: usize = 8_192;

/// The real walk: `EnumWindows` with a callback that reads four facts per window.
///
/// Every query here is a read of window-station state. None of them can change a
/// window, and none of them is sent to the owning process except `GetWindowTextW`,
/// which the OS itself bounds (`SendMessageTimeout`, not `SendMessage`) — so a hung
/// application cannot wedge this probe indefinitely.
#[cfg(windows)]
fn top_level_windows() -> Result<Vec<RawWindow>> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;

    /// The accumulator handed to the callback through `LPARAM`.
    ///
    /// Sound because the pointer is to a local created immediately before
    /// `EnumWindows`, lives until it returns, and the callback is the only holder. The
    /// callback runs synchronously on this thread, so there is no concurrent borrow.
    struct Walk {
        windows: Vec<RawWindow>,
        /// `true` once the row bound stopped the walk.
        capped: bool,
        /// Set when a query on this window failed in a way that made it unusable.
        /// Counted, not listed: a per-window failure is a fact about the walk's
        /// coverage, and a long list of them would be the noise this report exists to
        /// avoid.
        query_failures: usize,
    }

    let mut walk = Walk {
        windows: Vec::new(),
        capped: false,
        query_failures: 0,
    };

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
            GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, GW_OWNER,
            WS_EX_TOOLWINDOW,
        };

        let walk = unsafe { &mut *(lparam.0 as *mut Walk) };
        if walk.windows.len() >= MAX_WINDOWS_EXAMINED {
            walk.capped = true;
            // Returning `false` stops the walk. `EnumWindows` then reports failure
            // because the walk did not finish, which the caller must not read as an
            // API error — `capped` is what distinguishes the two.
            return BOOL(0);
        }
        if hwnd.is_invalid() {
            walk.query_failures += 1;
            return BOOL(1);
        }

        let mut pid = 0u32;
        let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if thread_id == 0 || pid == 0 {
            // The window vanished between enumeration and the query, which is normal
            // on a live desktop: windows close while we look at them.
            walk.query_failures += 1;
            return BOOL(1);
        }

        let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        let is_tool_window = (ex_style as u64) & (WS_EX_TOOLWINDOW.0 as u64) != 0;
        // `GetWindow` returns `Err` exactly when there is no owner window, so `is_ok`
        // is the ownership test and the error carries no information worth keeping.
        let has_owner = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok();

        let title = read_title(&|| unsafe { GetWindowTextLengthW(hwnd) }, &|buf| unsafe {
            GetWindowTextW(hwnd, buf)
        });

        walk.windows.push(RawWindow {
            pid,
            hwnd: hwnd.0 as isize,
            thread_id,
            title,
            is_visible: bool::from(unsafe { IsWindowVisible(hwnd) }),
            is_minimised: bool::from(unsafe { IsIconic(hwnd) }),
            is_tool_window,
            has_owner,
        });
        BOOL(1)
    }

    let status = unsafe { EnumWindows(Some(visit), LPARAM(&mut walk as *mut Walk as isize)) };

    // A callback that returned `false` (the row bound) makes `EnumWindows` report
    // failure. Only an *unexpected* failure is an error.
    if let Err(e) = status {
        if !walk.capped {
            return Err(ToolError::ProcessQuery {
                pid: 0,
                reason: format!("EnumWindows failed: {e}"),
            });
        }
    }

    if walk.capped {
        tracing::debug!(
            examined = walk.windows.len(),
            query_failures = walk.query_failures,
            "window walk stopped at its own bound; the station was not fully examined"
        );
    }
    Ok(walk.windows)
}

/// One window's title, lossy-decoded, and never longer than the shaping bound plus one.
///
/// Reading one character past [`MAX_TITLE_CHARS`] is deliberate: [`bound_title`] decides
/// `title_truncated` from the text it is given, so a title the walk cut short would
/// otherwise be reported as complete. A window with no title is reported as empty — the
/// filter drops it later, and the count of what was examined already includes it.
#[cfg(windows)]
fn read_title(length: &dyn Fn() -> i32, get: &dyn Fn(&mut [u16]) -> i32) -> String {
    let raw_len = length();
    if raw_len <= 0 {
        return String::new();
    }
    let want = raw_len.min(MAX_TITLE_CHARS as i32) + 1;
    let mut buffer = vec![0u16; want as usize + 1];
    let written = get(&mut buffer);
    if written <= 0 {
        return String::new();
    }
    let taken = (written as usize).min(buffer.len());
    String::from_utf16_lossy(&buffer[..taken])
}

/// Non-Windows builds have no window station, and this crate does not compile there.
#[cfg(not(windows))]
fn top_level_windows() -> Result<Vec<RawWindow>> {
    Err(window_station_unavailable())
}

/// The error the walk raises when it cannot be performed at all.
///
/// Only reachable on a build with no window station to reach; the Windows path names
/// the failing call instead of guessing at a reason.
#[cfg(not(windows))]
fn window_station_unavailable() -> ToolError {
    ToolError::ProcessQuery {
        pid: 0,
        reason: "no window station is reachable from this build; refusing to report \
                 'no windows' from a probe that could not look"
            .to_owned(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Filtering and shaping. Pure, and the reason the tests are meaningful.
// ─────────────────────────────────────────────────────────────────────────────

/// Whether one raw window is something the operator would call a window.
///
/// The three rules, and why each one is there, are in the module docs. Kept as a
/// function over [`RawWindow`] with no Win32 in sight so that "the filter rejects the
/// invisible helper and accepts an owned palette" is a checked statement rather than a
/// reading of a style-flag expression.
pub fn counts_as_operator_window(window: &RawWindow) -> bool {
    // A tool window with an owner is a real application's dialog or palette: visible
    // family, absent from Alt+Tab, and the operator would say the app has a window.
    // A tool window with no owner is floating plumbing that belongs to nobody.
    let belongs = !window.is_tool_window || window.has_owner;
    window.is_visible && !window.title.trim().is_empty() && belongs
}

/// Shape a raw window into a report row, applying the title bound.
fn shape(window: &RawWindow) -> WindowInfo {
    let (title, title_truncated) = bound_title(&window.title);
    WindowInfo {
        pid: window.pid,
        hwnd: window.hwnd,
        thread_id: window.thread_id,
        title,
        is_visible: window.is_visible,
        is_minimised: window.is_minimised,
        title_truncated,
    }
}

/// Cut a title at [`MAX_TITLE_CHARS`] `char`s, saying whether it was cut.
fn bound_title(title: &str) -> (String, bool) {
    // `char_indices` rather than a byte slice: the bound is in characters, and slicing
    // at a byte offset would panic on a non-ASCII title at the wrong boundary. No
    // panic is reachable here, and the reason is that no index is ever computed from
    // an assumption about encoding width.
    if let Some((byte_offset, _)) = title.char_indices().nth(MAX_TITLE_CHARS) {
        return (title[..byte_offset].to_owned(), true);
    }
    (title.to_owned(), false)
}

/// Filter, bound and shape a raw window list for one pid.
///
/// `max_rows` of `0` is a legal "just tell me if any exists" request: the walk still
/// happens and `windows_matched` is still true, so a caller can ask the yes/no question
/// without paying for a `Vec` it will drop.
fn report_from(windows: &[RawWindow], pid: u32, max_rows: usize) -> WindowReport {
    let mut rows: Vec<WindowInfo> = Vec::new();
    let mut matched = 0usize;
    for window in windows.iter().filter(|w| w.pid == pid) {
        if !counts_as_operator_window(window) {
            continue;
        }
        matched += 1;
        if rows.len() < max_rows {
            rows.push(shape(window));
        }
    }
    WindowReport {
        pid,
        windows: rows,
        truncated: matched > max_rows,
        windows_examined: windows.len(),
        windows_matched: matched,
        max_rows,
        walk_capped: windows.len() >= MAX_WINDOWS_EXAMINED,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry points.
// ─────────────────────────────────────────────────────────────────────────────

/// Which windows does `pid` own that an operator can see?
///
/// **Deviation from the brief's `Result<Vec<WindowInfo>>`:** the return is a
/// [`WindowReport`], because `Vec<WindowInfo>` cannot say that [`MAX_WINDOWS_REPORTED`]
/// was reached, and a row limit that is invisible in the type is a row limit that gets
/// silently truncated. The list is `report.windows`.
///
/// Goes through [`SystemWindowStation`]; see the module docs for what that currently
/// returns in this build.
pub fn windows_for_pid(pid: u32) -> Result<WindowReport> {
    windows_for_pid_with(&SystemWindowStation, pid, MAX_WINDOWS_REPORTED)
}

/// As [`windows_for_pid`], with a caller-chosen row bound.
///
/// The bound a report carries should be the bound the caller actually has. A status
/// line that prints five windows and passes [`MAX_WINDOWS_REPORTED`] is stating a limit
/// it did not choose, and its `max_rows` then lies about the row budget in use; passing
/// 5 makes `truncated` mean "there is more than you can print here", which is the
/// sentence the operator needs.
pub fn windows_for_pid_capped(pid: u32, max_rows: usize) -> Result<WindowReport> {
    windows_for_pid_with(&SystemWindowStation, pid, max_rows)
}

/// As [`windows_for_pid`] against an injected source.
///
/// Production callers want [`windows_for_pid`]; tests of the pipeline want this, and
/// `jean-core`'s verification tests can drive the exact sequence a real launch produces
/// (window on attempt 3, process gone on attempt 2) without creating a window or
/// killing a process.
pub fn windows_for_pid_with(
    station: &dyn WindowStation,
    pid: u32,
    max_rows: usize,
) -> Result<WindowReport> {
    let windows = station.top_level_windows()?;
    Ok(report_from(&windows, pid, max_rows))
}

/// Does `pid` own at least one visible window?
///
/// The cheap question for a verification loop that does not need titles.
///
/// **`false` is not proof of absence.** It covers "no window", "the pid is gone",
/// "the pid is a different process that reused the number", and, in this build, "the
/// probe could not look" — and those are different sentences. When the distinction
/// matters, use [`windows_for_pid`] (which errors rather than guessing) or
/// [`wait_for_window`] (which classifies).
pub fn any_window_for_pid(pid: u32) -> bool {
    windows_for_pid(pid).is_ok_and(|report| report.has_window())
}

/// Wait for `pid` to own a window, for at most `timeout`.
///
/// The primitive Phase 1 verifies a launch against. It answers the three-way question —
/// appeared / still starting / already exited — because collapsing those is how a
/// single-instance application gets misdiagnosed as a failed launch.
///
/// * `timeout` is honoured as given. `Duration::ZERO` is legal and means *exactly one
///   immediate attempt, no sleep*, which is a real question ("is it up right now") and
///   is why the timeout is not clamped like the poll is.
/// * `poll` is clamped into [`MIN_POLL`]..=[`MAX_POLL`]: a caller passing zero would
///   otherwise spin a core and hammer the window station for the whole timeout. The
///   clamped value is echoed in the outcome.
/// * Each attempt enumerates before checking liveness, so a window that exists is never
///   reported as "the process is gone" because of a race between the two questions.
/// * A probe error ends the wait immediately rather than spending the timeout
///   rediscovering that this build cannot read the window station.
pub fn wait_for_window(pid: u32, timeout: Duration, poll: Duration) -> WindowWait {
    wait_for_window_with(&SystemWindowStation, pid, timeout, poll)
}

/// As [`wait_for_window`] against an injected source.
pub fn wait_for_window_with(
    station: &dyn WindowStation,
    pid: u32,
    timeout: Duration,
    poll: Duration,
) -> WindowWait {
    let started = Instant::now();
    let poll = clamp_poll(poll);
    let mut attempts = 0usize;

    loop {
        attempts += 1;
        match station.top_level_windows() {
            Ok(windows) => {
                let report = report_from(&windows, pid, MAX_WINDOWS_REPORTED);
                if report.has_window() {
                    return WindowWait::Found(report);
                }
                if !station.pid_is_running(pid) {
                    return WindowWait::PidGone {
                        pid,
                        waited: started.elapsed(),
                        attempts,
                    };
                }
            }
            Err(error) => {
                return WindowWait::ProbeUnavailable {
                    pid,
                    reason: error.to_string(),
                    waited: started.elapsed(),
                };
            }
        }

        let waited = started.elapsed();
        if waited >= timeout {
            return WindowWait::DeadlineReached {
                pid,
                waited,
                poll,
                attempts,
            };
        }
        // Sleep the smaller of the cadence and what is left of the deadline, so a
        // deadline is never overshot by a whole poll interval — the `waited` a caller
        // prints is the real one.
        std::thread::sleep(poll.min(timeout - waited));
    }
}

/// Clamp a poll interval into [`MIN_POLL`]..=[`MAX_POLL`].
///
/// Clamped rather than rejected, mirroring [`crate::process::clamp_sample_window`]: a
/// caller passing zero is asking for something slightly different from what it meant,
/// not doing something wrong, and the used value is echoed back so no report claims a
/// cadence that did not happen.
pub fn clamp_poll(poll: Duration) -> Duration {
    poll.clamp(MIN_POLL, MAX_POLL)
}

/// True when a process with this pid is currently executing.
///
/// See the module docs for the two things this does *not* mean: it does not mean the
/// process has stopped being referenced (an exited process with a live handle still
/// opens), and it does not mean *this* pid is the process you launched — pids are
/// recycled, and identity needs the creation time `jean-process` records.
///
/// A query that fails is answered `false`. "Cannot tell" must not be reported as alive
/// to a caller that is about to decide a launch succeeded.
#[cfg(windows)]
pub fn pid_is_running(pid: u32) -> bool {
    let Some(handle) = open_for_query(pid) else {
        return false;
    };
    exit_code_is_still_active(*handle)
}

/// Non-Windows builds have no window station to inspect, and this crate does not
/// compile there at all; the function exists so the trait default has something to name.
#[cfg(not(windows))]
pub fn pid_is_running(_pid: u32) -> bool {
    false
}

/// Open a process for the two read-only queries, or `None` if it cannot be opened.
///
/// `PROCESS_QUERY_LIMITED_INFORMATION` and nothing more: it is the right that
/// non-elevated callers have against ordinary processes, and asking for more makes the
/// probe fail against exactly the interesting ones. `pid == 0` is rejected before the
/// call, because 0 is not a process and the system would not be obliged to say so.
#[cfg(windows)]
fn open_for_query(pid: u32) -> Option<windows::core::Owned<windows::Win32::Foundation::HANDLE>> {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    if pid == 0 {
        return None;
    }
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    if raw.is_invalid() {
        return None;
    }
    // SAFETY: `OpenProcess` handed us an unowned handle we are now responsible for, and
    // `Owned<HANDLE>` closes it with `CloseHandle` on drop. Nothing else can see it.
    Some(unsafe { windows::core::Owned::new(raw) })
}

#[cfg(windows)]
fn exit_code_is_still_active(handle: windows::Win32::Foundation::HANDLE) -> bool {
    use windows::Win32::System::Threading::GetExitCodeProcess;

    let mut code = 0u32;
    match unsafe { GetExitCodeProcess(handle, &mut code) } {
        // `Ok` with `STILL_ACTIVE` is the only alive answer. Any other code is an exit.
        Ok(()) => code == STILL_ACTIVE,
        Err(_) => false,
    }
}
