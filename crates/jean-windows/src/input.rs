//! Input fallback: keyboard and mouse, only after the target and the foreground have been re-checked.
//!
//! This module is deliberately last and deliberately boring. D-004 puts an application API, then a UIA
//! pattern, then verified synthetic input, then pixels — and every step down that ladder trades certainty
//! for reach. A click has no semantics: it is a coordinate, and a coordinate is a claim about where
//! something *is*, made at an earlier time. So the contract here is not "send input", it is "send input
//! only when the current state says the same thing the plan said".
//!
//! ## The three checks, in order, before anything is sent
//!
//! | Check | API | What it prevents |
//!|---|---|---|
//!| the handle is still the window we mean | `IsWindow`, then a full [`crate::identity::WindowIdentity`] re-read | acting on a recycled handle, which is somebody else's application |
//!| that window is actually in the foreground | `GetForegroundWindow` compared to the target | a capture-excluded overlay making the *image* show a window that is not the one input would reach — the case `docs/desktop-control-plan.md` calls out by name |
//!| the point is inside the window's *current* rectangle | `GetWindowRect`, re-read | clicking the coordinate saved before a layout change |
//!
//! None of them is a nicety and the order is not arbitrary: each is cheap, each can fail, and the one
//! that costs an operator data is the last one.
//!
//! ## Coordinates
//!
//! `SendInput` with `MOUSEEVENTF_ABSOLUTE` alone is normalised against the **primary monitor**, so on a
//! two-monitor machine it cannot address the second one at all. The combination this module uses is
//! `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, normalised against the virtual-desktop bounding
//! rectangle from `SM_XVIRTUALSCREEN`/`SM_CXVIRTUALSCREEN` — which is why R-03's mitigation is not a
//! fallback buried in this file but the only path through it, and why [`crate::identity`]'s DPI awareness
//! has to be established before anything here computes a number.
//!
//! ## What is not here
//!
//! No `AttachThreadInput`, no `AllowSetForegroundWindow`, no simulated Alt keystroke, no
//! `keybd_event`-style legacy calls, and no attempt to force the foreground when the check fails. Those
//! are the documented ways to *make* a window take focus, they are the same techniques malware uses, and
//! `jean-policy` has no rule that can tell the two apart. A failure here is reported as
//! [`WindowsError::FocusChanged`] and the caller stops.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IsChild, SetCursorPos, WindowFromPoint, SM_CXVIRTUALSCREEN,
};

use crate::bounded::{spawn_bounded, Outcome};
use crate::error::{WindowsError, WindowsResult};
use crate::identity::{
    foreground, read_identity, rect_of, virtual_desktop, Integrity, Reach, WindowId,
};

/// The normalised range `SendInput` expects for absolute coordinates.
///
/// 0..=65535 across the *virtual desktop* when `MOUSEEVENTF_VIRTUALDESK` is set. The number is a property
/// of the API and not a constant anyone chose, and getting the divisor wrong by one is a one-pixel error
/// at the far edge of the second monitor — which is the kind of defect that survives every test except
/// the one that matters.
const NORMALISED: i64 = 65535;

/// The longest a single injected input batch may take.
///
/// `SendInput` does not block on the receiving application, so this bound is about the worker rather than
/// the target: it exists so an input call can be given the same cancellation treatment as a UIA call,
/// and not so a hang can be survived. A stuck provider cannot be reached by input in the first place.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// A mouse button, named for the event pair Windows expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// A scroll direction, with the axis `SendInput` distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Vertical(i32),
    Horizontal(i32),
}

/// What may be injected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A click at a point that has been verified against the target's current rectangle.
    Click {
        button: Button,
    },
    /// A double click, which is two press pairs and not a different event.
    DoubleClick {
        button: Button,
    },
    /// Move the cursor without pressing anything.
    Move,
    Scroll {
        amount: Scroll,
    },
    /// Type text.
    ///
    /// Sent as `KEYEVENTF_UNICODE` events rather than as virtual-key presses: a layout-independent text
    /// injection is both more faithful to what was asked and free of the keyboard-layout bug where a
    /// symbol that needs Shift on one layout is typed as the wrong character on another.
    Text {
        text: String,
    },
    /// A named key with modifier keys held.
    ///
    /// `Text` cannot express Escape or Tab, and a control that needs them is a real control.
    Key {
        key: VirtualKey,
        modifiers: Vec<Modifier>,
    },
}

impl Input {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Click { button } => match button {
                Button::Left => "click",
                Button::Right => "right_click",
                Button::Middle => "middle_click",
            },
            Self::DoubleClick { .. } => "double_click",
            Self::Move => "move",
            Self::Scroll { .. } => "scroll",
            Self::Text { .. } => "type",
            Self::Key { .. } => "key",
        }
    }

    /// A description safe for the audit line.
    pub fn describe(&self) -> String {
        match self {
            // The typed text is content the operator supplied; its length is not.
            Self::Text { text } => format!("type {} chars", text.chars().count()),
            Self::Key { key, modifiers } => {
                let held = modifiers
                    .iter()
                    .map(|modifier| modifier.name())
                    .collect::<Vec<_>>()
                    .join("+");
                if held.is_empty() {
                    format!("key {}", key.name())
                } else {
                    format!("key {held}+{}", key.name())
                }
            }
            Self::Scroll { amount } => match amount {
                Scroll::Vertical(ticks) => format!("scroll vertical {ticks}"),
                Scroll::Horizontal(ticks) => format!("scroll horizontal {ticks}"),
            },
            other => other.name().to_owned(),
        }
    }

    /// Whether this input can change anything outside this process.
    pub fn has_side_effects(&self) -> bool {
        !matches!(self, Self::Move)
    }
}

/// The modifier keys this module can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Control,
    Alt,
    Shift,
    Windows,
}

impl Modifier {
    pub fn name(self) -> &'static str {
        match self {
            Self::Control => "ctrl",
            Self::Alt => "alt",
            Self::Shift => "shift",
            Self::Windows => "win",
        }
    }

    fn key(self) -> VIRTUAL_KEY {
        match self {
            Self::Control => VIRTUAL_KEY(0x11),
            Self::Alt => VIRTUAL_KEY(0x12),
            Self::Shift => VIRTUAL_KEY(0x10),
            Self::Windows => VIRTUAL_KEY(0x5B),
        }
    }
}

/// A small, closed set of named keys.
///
/// The full `VIRTUAL_KEY` space is not exposed: a caller that needs an arbitrary key code can supply it
/// through [`VirtualKey::Raw`], and the named half is where a typo in a plan becomes a readable failure
/// instead of a keystroke nobody asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualKey {
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Raw(u16),
}

impl VirtualKey {
    /// The name this build uses for the key, in the vocabulary `jean desktop input --key` accepts.
    ///
    /// Not the Win32 spellings (`VK_RETURN`, `VK_BACK`, `VK_PRIOR`), because this string is what an
    /// operator reads back in the audit line after typing one of those arguments: an audit that says
    /// `key ctrl+shift+Return` for a call made with `--key enter` is describing the same key in two
    /// vocabularies, and only one of them is the one the operator was told to use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Enter => "Enter",
            Self::Escape => "Escape",
            Self::Tab => "Tab",
            Self::Backspace => "Backspace",
            Self::Delete => "Delete",
            Self::Space => "Space",
            Self::Up => "Up",
            Self::Down => "Down",
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Home => "Home",
            Self::End => "End",
            Self::PageUp => "PageUp",
            Self::PageDown => "PageDown",
            // A raw code is not named, because naming it would be a claim about a keyboard layout this
            // machine may not be using.
            Self::Raw(_) => "raw",
        }
    }

    fn to_u16(self) -> u16 {
        match self {
            Self::Enter => 0x0D,
            Self::Escape => 0x1B,
            Self::Tab => 0x09,
            Self::Backspace => 0x08,
            Self::Delete => 0x2E,
            Self::Space => 0x20,
            Self::Up => 0x26,
            Self::Down => 0x28,
            Self::Left => 0x25,
            Self::Right => 0x27,
            Self::Home => 0x24,
            Self::End => 0x23,
            Self::PageUp => 0x21,
            Self::PageDown => 0x22,
            Self::Raw(code) => code,
        }
    }
}

/// The state an input request is checked against, all of it re-read at the moment of the call.
#[derive(Debug, Clone)]
pub struct Target {
    pub window: WindowId,
    /// The point to act on, in physical pixels on the virtual desktop.
    pub point: (i32, i32),
    /// The window that must be in the foreground for the input to be safe.
    ///
    /// Normally [`Target::window`] itself. It is a separate field because an action legitimately aimed at
    /// a child dialog has to name the frame that holds focus, and hard-coding "the target must be the
    /// foreground" would make that case impossible rather than expressible.
    pub expect_foreground: WindowId,
}

impl Target {
    /// Re-check everything this target claims, and say which claim failed.
    ///
    /// This is the function the brief's "before injected input, verify actual foreground and target
    /// location" sentence turns into code, and it is called by [`send`] rather than offered to the caller:
    /// a check someone can forget is not a boundary.
    pub fn verify(&self) -> WindowsResult<TargetState> {
        if !self.window.is_alive() {
            return Err(WindowsError::StaleWindow {
                hwnd: self.window.hwnd,
            });
        }
        let identity = read_identity(self.window);

        // A window that is not in the foreground is not "the window the operator is looking at", and the
        // one case where that distinction has teeth is the overlay: an excluded window can leave the
        // *image* showing something underneath it while input goes to the window on top.
        let actual_foreground = foreground();
        if actual_foreground != Some(self.expect_foreground) {
            return Err(WindowsError::FocusChanged {
                expected: describe_window(self.expect_foreground),
                actual: actual_foreground
                    .map(describe_window)
                    .unwrap_or_else(|| "no window is in the foreground".to_owned()),
            });
        }

        let rect = rect_of(self.window).ok_or(WindowsError::StaleWindow {
            hwnd: self.window.hwnd,
        })?;
        let (x, y) = self.point;
        if !rect.contains(x, y) {
            // The point came from an earlier observation and the window has moved, or the point came from
            // a capture of a window that is not where the capture said it was. Either way the number is
            // stale, and clicking it would click whatever now occupies it.
            return Err(WindowsError::StaleElement {
                control: format!(
                    "point ({x},{y}) is outside the window's current rectangle {}x{}@{},{}",
                    rect.width(),
                    rect.height(),
                    rect.left,
                    rect.top
                ),
            });
        }

        // R-14: an integrity boundary drops input *silently*. A dropped click that is reported as a
        // performed click is the worst thing an automation can do to an operator's trust, so the
        // comparison is made here, before the send, and the answer is a blocker rather than a retry.
        let ours = Integrity::owned_by_this_process();
        let reach = Integrity::input_reach(ours.as_ref(), identity.integrity.as_ref());
        match reach {
            Reach::Allowed => {}
            Reach::Blocked { ours, theirs } => {
                return Err(WindowsError::IntegrityBlocked {
                    hwnd: self.window.hwnd,
                    reason: format!(
                        "this process is at {ours} and the target is at {theirs}; Windows would drop the input without reporting it"
                    ),
                })
            }
            Reach::Unknown(reason) => {
                return Err(WindowsError::IntegrityBlocked {
                    hwnd: self.window.hwnd,
                    reason: format!("the integrity levels could not be compared ({reason})"),
                })
            }
        }

        Ok(TargetState {
            identity,
            foreground_confirmed: true,
        })
    }
}

/// What the pre-flight established.
#[derive(Debug, Clone)]
pub struct TargetState {
    pub identity: crate::identity::WindowIdentity,
    /// Always `true` when a [`Target::verify`] returned `Ok`, and carried anyway: a verifier that reads
    /// `true` here is reading a fact about the check that ran, not about a struct that could be
    /// constructed by hand.
    pub foreground_confirmed: bool,
}

/// Settings the input path runs under.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub call_timeout: Duration,
    /// Whether `SetCursorPos` may move the operator's pointer at all.
    ///
    /// `false` is not "no movement": the click still happens at the coordinates, because
    /// `MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE` addresses them without a visible pointer move being
    /// required. The flag exists for the case where an operator is mid-drag and a pointer jump is the
    /// difference between an automation and an accident, which is a policy question and therefore lives
    /// in the settings.
    pub move_visible_cursor: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            call_timeout: DEFAULT_TIMEOUT,
            move_visible_cursor: true,
        }
    }
}

/// The input boundary, so a different injection mechanism stays a different implementation.
pub trait InputBackend: Send + Sync {
    /// Send one input to one verified target.
    ///
    /// The verification happens inside, on the worker, immediately before the send — not in the caller's
    /// thread before it. The gap between those two is exactly the gap a focus change can fit through.
    fn send(&self, target: &Target, input: &Input, settings: &Settings) -> WindowsResult<Sent>;
}

/// What was actually put on the input queue.
#[derive(Debug, Clone)]
pub struct Sent {
    pub input: Input,
    /// The number of events `SendInput` accepted.
    ///
    /// `SendInput` returns a count, and a count lower than the events queued means some of them were
    /// dropped — usually by a lock or by integrity. This is the field that makes that visible instead of
    /// inferred.
    pub events_accepted: u32,
    pub events_queued: u32,
    /// Whether the whole batch was accepted.
    pub complete: bool,
    /// The foreground as seen immediately *after* the send.
    ///
    /// Reported because a click can change it, and a caller about to type has to know the current state
    /// rather than the state the plan assumed.
    pub foreground_after: Option<WindowId>,
    /// The target's rectangle immediately after the send.
    pub target_still_where: bool,
}

impl Sent {
    /// Whether the injection can be treated as having happened.
    ///
    /// A partial batch is not a success with a note; it is an unknown quantity of side effects, and the
    /// "never replay an uncertain action" rule is what this feeds.
    pub fn is_complete(&self) -> bool {
        self.complete && self.events_accepted == self.events_queued && self.events_queued > 0
    }
}

/// The real backend: raw `SendInput`.
pub struct SendInputBackend;

impl InputBackend for SendInputBackend {
    fn send(&self, target: &Target, input: &Input, settings: &Settings) -> WindowsResult<Sent> {
        let state = target.verify()?;
        if matches!(
            input,
            Input::Click { .. } | Input::DoubleClick { .. } | Input::Scroll { .. }
        ) {
            let (x, y) = target.point;
            let window = target.window.to_raw();
            let hit = unsafe { WindowFromPoint(POINT { x, y }) };
            // The foreground window can still be covered by a non-activating overlay.
            if hit != window && !unsafe { IsChild(window, hit) }.as_bool() {
                return Err(WindowsError::StaleElement {
                    control: format!(
                        "point ({x},{y}) hits hwnd={} instead of target hwnd={}",
                        hit.0 as isize, target.window.hwnd
                    ),
                });
            }
        }
        let desktop = virtual_desktop();
        let (dx, dy) = to_normalised(target.point, desktop);

        let events: Vec<INPUT> = build(input, target.point, dx, dy, settings);
        let queued = events.len() as u32;
        let accepted = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };

        let still = rect_of(target.window)
            .map(|rect| rect.contains(target.point.0, target.point.1))
            .unwrap_or(false);

        if accepted != queued {
            // Not a hard failure: the events that *were* accepted may have done their work, so the count
            // is returned and the caller decides. What is not done here is a retry, because a partial
            // batch of clicks replayed is a double click nobody asked for.
            tracing::warn!(
                target = state.identity.audit_label(),
                queued,
                accepted,
                "SendInput dropped part of an input batch"
            );
        }

        Ok(Sent {
            input: input.clone(),
            events_accepted: accepted,
            events_queued: queued,
            complete: accepted == queued,
            foreground_after: foreground(),
            target_still_where: still,
        })
    }
}

/// Convert a virtual-desktop physical point into the normalised range `SendInput` wants.
///
/// The `- 1` in the divisor is in the documented formula, and dropping it shifts the far edge of every
/// monitor by one pixel — the kind of error that shows up only at the right-hand edge of the secondary
/// display, which is where a second monitor's controls tend to live.
pub fn to_normalised(point: (i32, i32), desktop: crate::identity::Rect) -> (i32, i32) {
    (
        normalised_axis(point.0.saturating_sub(desktop.left), desktop.width()),
        normalised_axis(point.1.saturating_sub(desktop.top), desktop.height()),
    )
}

/// One axis of [`to_normalised`], with the division's own edge cases in one place.
///
/// The floor is on the *divisor*, not the extent: `extent - 1` is zero for a desktop that reports no
/// width at all, and `SM_CXVIRTUALSCREEN` is measured as zero on a session with no display attached.
/// A zero divisor is a panic, and a panic here is in the process that holds the operator's tasks, so the
/// degenerate geometry answers "the first pixel" rather than crashing the thing that asked.
fn normalised_axis(offset: i32, extent: i32) -> i32 {
    let divisor = extent.saturating_sub(1).max(1) as i64;
    (offset as i64 * NORMALISED / divisor).clamp(0, NORMALISED) as i32
}

/// The event list for one input.
///
/// `point` is the physical-pixel target and `dx`/`dy` are its normalised form, both supplied because
/// `SetCursorPos` works in the first space and `SendInput` in the second. They are passed in rather than
/// recomputed so that a single conversion — and therefore a single opportunity to get it wrong — is what
/// the rest of the crate depends on.
fn build(input: &Input, point: (i32, i32), dx: i32, dy: i32, settings: &Settings) -> Vec<INPUT> {
    let mut events = Vec::new();
    match input {
        Input::Move => {
            if settings.move_visible_cursor {
                // `SetCursorPos` takes physical pixels; the absolute-move event below carries the same
                // point in normalised form, so the pointer and the input agree rather than drifting by
                // the difference between the two spaces.
                let _ = unsafe { SetCursorPos(point.0, point.1) };
            }
            events.push(mouse(
                dx,
                dy,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ));
        }
        Input::Click { button } => {
            let (down, up) = button_flags(*button);
            events.push(mouse(
                dx,
                dy,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | down,
            ));
            events.push(mouse(
                dx,
                dy,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | up,
            ));
        }
        Input::DoubleClick { button } => {
            let (down, up) = button_flags(*button);
            for _ in 0..2 {
                events.push(mouse(
                    dx,
                    dy,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | down,
                ));
                events.push(mouse(
                    dx,
                    dy,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | up,
                ));
            }
        }
        Input::Scroll { amount } => {
            let (flag, ticks) = match amount {
                Scroll::Vertical(ticks) => (MOUSEEVENTF_WHEEL, *ticks),
                Scroll::Horizontal(ticks) => (MOUSEEVENTF_HWHEEL, *ticks),
            };
            // `mouseData` is ticks in multiples of `WHEEL_DELTA` (120), and the sign is the direction.
            // Scaling here rather than leaving it to the caller is what makes `Scroll::Vertical(3)` mean
            // three notches on every machine.
            //
            // The absolute addressing is the same combination the click path sends, and for the same
            // reason: a wheel event without it is read at the pointer's current position, which would make
            // the rectangle check above a promise this input does not keep. What the fixture cannot show —
            // it has no scrollable control — is which window Windows then routes the notch to, so the claim
            // here is "aimed where it was aimed", not "scrolled the control that was named".
            events.push(mouse_with_data(
                dx,
                dy,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | flag,
                (ticks * 120) as u32,
            ));
        }
        Input::Text { text } => {
            for unit in text.encode_utf16() {
                events.push(unicode_event(unit, false));
                events.push(unicode_event(unit, true));
            }
        }
        // The builder is `key_event`, not `key`: the pattern binds a value called `key` in this arm, and
        // a shadowed function name turns every call into a type error that reads like a design problem.
        Input::Key { key, modifiers } => {
            for modifier in modifiers {
                events.push(key_event(modifier.key(), KEYBD_EVENT_FLAGS(0)));
            }
            events.push(key_event(VIRTUAL_KEY(key.to_u16()), KEYBD_EVENT_FLAGS(0)));
            events.push(key_event(VIRTUAL_KEY(key.to_u16()), KEYEVENTF_KEYUP));
            // Released in the reverse order: pressing Ctrl then Alt and releasing Ctrl first would leave
            // Alt held against a key event that was never meant to carry it.
            for modifier in modifiers.iter().rev() {
                events.push(key_event(modifier.key(), KEYEVENTF_KEYUP));
            }
        }
    }
    events
}

fn button_flags(
    button: Button,
) -> (
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) {
    match button {
        Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

fn mouse(
    dx: i32,
    dy: i32,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> INPUT {
    mouse_with_data(dx, dy, flags, 0)
}

fn mouse_with_data(
    dx: i32,
    dy: i32,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    data: u32,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_event(code: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: code,
                // `wScan` is the field a scanned key does not need; the character form of an event has its
                // own builder below, because the two put the payload in different fields.
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// One typed character, as Windows documents it: the UTF-16 unit in `wScan`, `wVk` zero, and
/// `KEYEVENTF_UNICODE` saying which of the two fields carries the payload.
///
/// **This is the defect the fixture caught.** An earlier version passed the unit as `wVk` with `wScan`
/// zero, which is the mirror image of what the flag means. Windows then treated every character as a key
/// code: `injected text` became `9.*5354 5` on this machine — 0x69 is numpad 9, 0x6E is the decimal key,
/// 0x74 is F5, which types nothing at all — and the batch still reported 26 of 26 events accepted, because
/// `SendInput` counts events, not characters. Nothing downstream could see it until the typed text was read
/// back out of the control it was aimed at.
fn unicode_event(unit: u16, key_up: bool) -> INPUT {
    let flags = if key_up {
        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
    } else {
        KEYEVENTF_UNICODE
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn describe_window(window: WindowId) -> String {
    let identity = read_identity(window);
    identity.audit_label()
}

/// Send one input to one verified target, on a bounded worker.
///
/// The verification runs inside the worker, immediately before `SendInput`, which is the only ordering
/// that leaves no gap for a focus change to fit through.
pub fn send(
    target: Target,
    input: Input,
    settings: Settings,
    cancel: Option<&AtomicBool>,
) -> WindowsResult<Sent> {
    let target_window = target.window;
    match spawn_bounded("input", settings.call_timeout, cancel, move || {
        SendInputBackend.send(&target, &input, &settings)
    }) {
        Outcome::Completed(result) => result,
        Outcome::TimedOut { waited } => Err(WindowsError::ProviderTimeout {
            hwnd: target_window.hwnd,
            budget_secs: settings.call_timeout.as_secs().max(1),
            waited_ms: waited.as_millis() as u64,
        }),
        Outcome::Cancelled { waited } => Err(WindowsError::Cancelled {
            budget_secs: settings.call_timeout.as_secs().max(1),
            waited_ms: waited.as_millis() as u64,
        }),
        Outcome::Panicked => Err(WindowsError::Panicked {
            call: "input send".to_owned(),
        }),
        Outcome::NotStarted => Err(WindowsError::Win32 {
            call: "CreateThread for an input worker".to_owned(),
            reason: "no worker thread could be created".to_owned(),
        }),
    }
}

/// The virtual-desktop width as the OS reports it, exposed for the suite's own coordinate checks.
///
/// A test that recomputes a coordinate needs the same number the mechanism used, and reading it from a
/// second source inside the test would make the test agree with the code rather than with Windows.
pub fn virtual_desktop_width() -> i32 {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(SM_CXVIRTUALSCREEN) }
}

/// Whether the coordinates a caller is about to use are in the space this machine produces.
///
/// The R-15 probe: it reports the virtual desktop, the DPI of the window, and the awareness actually in
/// force, so a wrong coordinate can be diagnosed instead of guessed at.
pub fn coordinate_report(window: WindowId) -> WindowsResult<String> {
    if !window.is_alive() {
        return Err(WindowsError::StaleWindow { hwnd: window.hwnd });
    }
    let desktop = virtual_desktop();
    let identity = read_identity(window);
    Ok(format!(
        "virtual={}x{}@{},{} window={} dpi={} awareness={} target={},{}",
        desktop.width(),
        desktop.height(),
        desktop.left,
        desktop.top,
        identity
            .rect
            .map(|rect| format!(
                "{}x{}@{},{}",
                rect.width(),
                rect.height(),
                rect.left,
                rect.top
            ))
            .unwrap_or_else(|| "unreadable".to_owned()),
        identity
            .dpi
            .map(|dpi| dpi.to_string())
            .unwrap_or_else(|| "unknown".to_owned()),
        crate::identity::awareness_of(window),
        identity
            .rect
            .map(|rect| rect.centre().0)
            .unwrap_or(i32::MIN),
        identity
            .rect
            .map(|rect| rect.centre().1)
            .unwrap_or(i32::MIN),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The event list for one input, without a desktop to send it to.
    ///
    /// `build` is tested rather than `send` because the defect it is written against is invisible to
    /// `send`: Windows accepts a mis-encoded character batch whole, so an acceptance count — and a
    /// verdict that stops at "the input was sent" — cannot see it. Only the encoding, or a read of the
    /// control the text was aimed at, can.
    fn events(input: &Input) -> Vec<INPUT> {
        build(
            input,
            (40, 40),
            400,
            400,
            &Settings {
                // `build` calls `SetCursorPos` when this is on, and a unit test has no business moving the
                // pointer on a machine somebody is using. The encoding under test is the event list.
                move_visible_cursor: false,
                ..Default::default()
            },
        )
    }

    /// The keyboard arm of one event, once the discriminator says that is what it is.
    ///
    /// The union read is unsafe in the general case and sound here for exactly one reason: the line above
    /// it checked `type`, which is the field Windows itself uses to pick the arm.
    fn keyboard(event: &INPUT) -> KEYBDINPUT {
        assert_eq!(
            event.r#type.0, INPUT_KEYBOARD.0,
            "expected a keyboard event"
        );
        unsafe { event.Anonymous.ki }
    }

    fn mouse(event: &INPUT) -> MOUSEINPUT {
        assert_eq!(event.r#type.0, INPUT_MOUSE.0, "expected a mouse event");
        unsafe { event.Anonymous.mi }
    }

    #[test]
    fn typed_text_carries_its_characters_in_the_scan_field_and_names_no_key() {
        let text = "injected text";
        let units: Vec<u16> = text.encode_utf16().collect();
        let sent = events(&Input::Text {
            text: text.to_owned(),
        });
        assert_eq!(
            sent.len(),
            units.len() * 2,
            "one press and one release per character"
        );
        for (position, event) in sent.iter().enumerate() {
            let ki = keyboard(event);
            let unit = units[position / 2];
            assert_eq!(
                ki.wVk,
                VIRTUAL_KEY(0),
                "U+{unit:04X} must not be delivered as a key code"
            );
            assert_eq!(ki.wScan, unit, "the character is the scan field");
            assert_ne!(
                ki.dwFlags.0 & KEYEVENTF_UNICODE.0,
                0,
                "the flag is what makes wScan a character rather than a key"
            );
            assert_eq!(
                ki.dwFlags.0 & KEYEVENTF_KEYUP.0 != 0,
                position % 2 == 1,
                "press then release, in that order"
            );
        }
    }

    #[test]
    fn a_named_key_is_a_key_code_and_a_release_of_it() {
        let sent = events(&Input::Key {
            key: VirtualKey::Enter,
            modifiers: vec![Modifier::Control],
        });
        // Ctrl down, Enter down, Enter up, Ctrl up.
        assert_eq!(sent.len(), 4);
        let ctrl = keyboard(&sent[0]);
        assert_eq!(ctrl.wVk.0, 0x11, "Control is held, not typed");
        assert_eq!(ctrl.wScan, 0, "a key event carries no character");
        assert_eq!(ctrl.dwFlags.0 & KEYEVENTF_UNICODE.0, 0);
        let enter = keyboard(&sent[1]);
        assert_eq!(enter.wVk.0, 0x0D);
        assert_ne!(
            keyboard(&sent[2]).dwFlags.0 & KEYEVENTF_KEYUP.0,
            0,
            "the key itself releases before the modifier"
        );
        assert_ne!(
            keyboard(&sent[3]).dwFlags.0 & KEYEVENTF_KEYUP.0,
            0,
            "the modifier is released last, in the reverse order it was pressed"
        );
    }

    /// A desktop rectangle for the tests, built from the shape a real multi-monitor layout has.
    fn desktop(left: i32, top: i32, right: i32, bottom: i32) -> crate::identity::Rect {
        crate::identity::Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn a_monitor_to_the_left_or_above_is_addressed_from_the_virtual_origin_not_zero() {
        // The layout this machine does not have: a secondary parked left of the primary puts
        // `SM_XVIRTUALSCREEN` at -1366, so a point on that monitor is a *negative* physical coordinate.
        // Normalising from zero instead of from the origin would push every click on the left-hand display
        // toward a clamp, i.e. toward its top-left corner, and report the click as performed.
        let left_hand = desktop(-1366, 0, 1920, 1080);
        let right_hand = desktop(0, 0, 3286, 1080);
        assert_eq!(
            to_normalised((-1366, 0), left_hand),
            (0, 0),
            "the virtual origin is the zero of the range, not the primary's left edge"
        );
        assert_eq!(
            to_normalised((553, 0), left_hand),
            to_normalised((1919, 0), right_hand),
            "the same offset from the origin is the same normalised value whichever side the monitor is on"
        );
        assert_eq!(to_normalised((1919, 1079), left_hand).0, 65535);

        // A primary below a secondary: `SM_YVIRTUALSCREEN` is negative and the y axis must follow it.
        let above = desktop(0, -768, 1920, 1080);
        assert_eq!(to_normalised((0, -768), above).1, 0);
        assert_eq!(to_normalised((0, -1), above).1, 65535 * 767 / 1847);
    }

    #[test]
    fn the_last_pixel_of_the_virtual_desktop_reaches_the_top_of_the_range() {
        // `MOUSEEVENTF_ABSOLUTE` with `VIRTUALDESK` puts the pointer at `x / 65535` of the range, so the
        // final column is `width - 1`, not `width`. Divide by the width instead and that column lands at
        // 65515, which is a pixel short — one row of a scroll bar, or the resize edge of a window.
        let this = desktop(0, 0, 3286, 1080);
        assert_eq!(to_normalised((3285, 1079), this), (65535, 65535));
        assert_eq!(to_normalised((0, 0), this), (0, 0));
        // Values from the middle of the range, pinned rather than derived from the same expression the
        // function uses: 19151/32797 is the primary's centre inside this layout.
        assert_eq!(to_normalised((960, 540), this), (19151, 32797));

        // The same point converted by the formula without the `- 1`, so this test disagrees with that
        // variant rather than agreeing with it.
        let without = 3285_i64 * NORMALISED / 3286;
        assert_eq!(without, 65515);
        assert_ne!(
            without as i32,
            to_normalised((3285, 1079), this).0,
            "the `- 1` is load-bearing: without it the far edge is {without}"
        );
    }

    #[test]
    fn a_point_beyond_the_desktop_is_clamped_rather_than_wrapped_or_negative() {
        let left_hand = desktop(-1366, 0, 1920, 1080);
        // A stale target whose window moved off the left edge, and one where a caller handed an absolute
        // screen coordinate as if it were virtual: both must arrive inside the range, because a negative
        // `dx` is accepted by `SendInput` and delivered somewhere nobody looked.
        assert_eq!(to_normalised((-2000, 500), left_hand), (0, 30368));
        assert_eq!(to_normalised((5000, 500), left_hand), (65535, 30368));
        for point in [(-2000, 500), (5000, 500), (-1366, 0), (1919, 1079)] {
            let (x, y) = to_normalised(point, left_hand);
            assert!(
                (0..=NORMALISED as i32).contains(&x) && (0..=NORMALISED as i32).contains(&y),
                "{point:?} left the range as ({x},{y})"
            );
        }
    }

    #[test]
    fn a_desktop_that_reports_no_geometry_answers_instead_of_dividing_by_zero() {
        // `SM_CXVIRTUALSCREEN` is zero with no display attached, which is a disconnected-headless-session
        // state rather than an impossibility. The divisor is `width - 1`, so a floor on the width alone
        // still divides by zero, and a panic here is in the core process rather than in a worker that can
        // be reaped.
        assert_eq!(to_normalised((0, 0), desktop(0, 0, 0, 0)), (0, 0));
        assert_eq!(to_normalised((0, 0), desktop(0, 0, 1, 1)), (0, 0));
        // An inverted rectangle, which is what a partial read of a moving window can look like: still a
        // number, and still inside the range.
        assert_eq!(to_normalised((500, 500), desktop(1000, 1000, 0, 0)), (0, 0));
    }

    #[test]
    fn a_wheel_notch_is_scaled_and_aimed_and_a_move_presses_nothing() {
        let sent = events(&Input::Scroll {
            amount: Scroll::Vertical(3),
        });
        assert_eq!(
            sent.len(),
            1,
            "a scroll is one event, and this produced {}",
            sent.len()
        );
        let wheel = mouse(&sent[0]);
        assert_eq!(
            wheel.mouseData,
            3 * 120,
            "ticks are multiples of WHEEL_DELTA"
        );
        assert_eq!(
            wheel.dwFlags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_WHEEL,
            "a notch without the click path's addressing is read at wherever the pointer happens to be"
        );

        let moved = events(&Input::Move);
        let flags: Vec<_> = moved.iter().map(|event| mouse(event).dwFlags).collect();
        assert!(
            !flags
                .iter()
                .any(|flag| flag.0 & (MOUSEEVENTF_LEFTDOWN.0 | MOUSEEVENTF_LEFTUP.0) != 0),
            "moving the cursor presses nothing: {flags:?}"
        );
    }
}
