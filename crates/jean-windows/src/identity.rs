//! Window identity: which window, whose process, on which monitor, at what integrity level.
//!
//! "A window exists" is not an identity, and the difference is what makes the rest of desktop control
//! safe to automate. This module answers five separate questions that are usually collapsed into one:
//!
//! | Question | API | Why it cannot be skipped |
//! |---|---|---|
//! | is this handle still a window at all | `IsWindow` | Handles are recycled the moment a window is destroyed; a cached one is a coordinate into somebody else's application. |
//! | who owns it | `GetWindowThreadProcessId` | Attribution, and the only thing that lets an action be checked against the tool the operator named. |
//! | can we reach it | `GetTokenInformation(TokenIntegrityLevel)` on both sides | UIPI drops input and some UIA reads silently, across an integrity boundary (R-14). A silent failure here looks exactly like a dead control. |
//! | where is it, in units that mean something | `GetWindowRect`, `GetDpiForWindow`, `GetMonitorInfoW` | Physical pixels, window pixels and logical pixels are three different numbers on this machine, and clicking the wrong one clicks the wrong thing (R-15, R-03). |
//! | is there an interactive session to act on | `WTSGetActiveConsoleSessionId`, `OpenInputDesktop` | A locked screen or a secure desktop is not a protection and not a hang; it is the absence of a desktop to operate. |
//!
//! Two properties this module is designed around:
//!
//! * **Every fact is an observation with a timestamp, never a cached truth.** [`WindowIdentity::read`] is
//!   cheap enough to call before every action, and the callers in `jean_core::desktop` do exactly that,
//!   because the plan's rule is "revalidate targets before acting".
//! * **Unknown is a reported value, not a default.** DPI that could not be read is `None`, not 96; an
//!   integrity level that `OpenProcess` refused is a refusal with a reason, not Medium. A made-up default
//!   is how an automation quietly acts on the wrong monitor.

use std::ffi::c_void;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HWND, POINT, RECT as WinRect};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    SID_AND_ATTRIBUTES, TOKEN_QUERY,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, HDESK, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_FORMAT, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForWindow, GetWindowDpiAwarenessContext, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetSystemMetrics, GetWindowDisplayAffinity, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, IsWindow, IsWindowVisible, GA_ROOT, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::error::{WindowsError, WindowsResult};

/// The longest window title this module will read.
///
/// A title is text another process wrote, and `GetWindowTextW` can be handed a megabyte of it. The bound
/// is what keeps a report, and the audit line built from it, bounded; truncation is reported rather than
/// silently cut.
pub const MAX_TITLE_CHARS: usize = 512;

/// A window handle, held as a plain integer so nothing above this crate has to name a Win32 type.
///
/// The value is *not* a capability and carries no ownership: it cannot be closed, and it says nothing
/// about whether the window still exists. Call [`WindowId::is_alive`] before acting on one that was read
/// in an earlier call.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct WindowId {
    pub hwnd: isize,
}

impl WindowId {
    pub(crate) fn from_raw(hwnd: HWND) -> Self {
        Self {
            hwnd: hwnd.0 as isize,
        }
    }

    pub(crate) fn to_raw(self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }

    /// Does this handle still name a window *right now*?
    ///
    /// `IsWindow` is the cheap check that separates "stale" from "gone": every UIA and input call in this
    /// crate refuses a dead handle, and a handle that has been recycled by a new process must not be
    /// treated as the window it used to name.
    pub fn is_alive(self) -> bool {
        bool::from(unsafe { IsWindow(Some(self.to_raw())) })
    }

    /// The title, bounded and lossily decoded, or `None` if the window has none.
    ///
    /// `GetWindowTextW` reaches into the owning process, so this is the one identity read that can be
    /// slow. The OS bounds it with its own deadlock detection, and the value is untrusted text: never
    /// interpreted, never used as an identity key.
    pub fn title(self) -> Option<String> {
        let length = unsafe { GetWindowTextLengthW(self.to_raw()) };
        if length <= 0 {
            return None;
        }
        let want = (length as usize).min(MAX_TITLE_CHARS);
        let mut buffer = vec![0u16; want + 1];
        let read = unsafe { GetWindowTextW(self.to_raw(), &mut buffer) };
        if read <= 0 {
            return None;
        }
        let taken = (read as usize).min(want);
        buffer[taken] = 0;
        let text = String::from_utf16_lossy(&buffer[..taken]);
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

/// A rectangle in *physical* pixels on the virtual desktop.
///
/// This is the only coordinate space `SendInput` understands, which is why everything else in this crate
/// converts into it rather than working in logical units and hoping the numbers agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    pub fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    pub fn area(self) -> i64 {
        self.width().max(0) as i64 * self.height().max(0) as i64
    }

    pub fn is_empty(self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// The centre, which is where a click intended for a whole control lands.
    ///
    /// Saturating halves rather than floats: a rectangle with a negative extent is a window that is
    /// partially off-screen, and the midpoint of that is still a physical pixel coordinate.
    pub fn centre(self) -> (i32, i32) {
        let x = self.left + self.width() / 2;
        let y = self.top + self.height() / 2;
        (x, y)
    }
}

impl From<WinRect> for Rect {
    fn from(value: WinRect) -> Self {
        Self {
            left: value.left,
            top: value.top,
            right: value.right,
            bottom: value.bottom,
        }
    }
}

/// The monitor a window was found on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    /// Position on the virtual desktop, in physical pixels. May be negative.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Effective DPI as Windows renders this monitor. 96 is 100%, 120 is 125%.
    pub dpi: u32,
    /// `true` when this is the monitor the taskbar lives on.
    pub is_primary: bool,
}

impl Monitor {
    /// The scale factor a per-monitor-v2 process sees on this monitor.
    ///
    /// Derived from the DPI rather than from `GetScaleFactorForMonitor` because the derived number is the
    /// one that agrees with the pixels actually being produced.
    pub fn scale_percent(self) -> u32 {
        if self.dpi == 0 {
            0
        } else {
            self.dpi.saturating_mul(100) / 96
        }
    }
}

/// An integrity level, as the level a UIPI decision is made on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    /// `S-1-16-8192` — the canonical form, which is what a report should quote.
    pub sid: String,
    /// The last sub-authority: 0x1000 Low, 0x2000 Medium, 0x3000 High, 0x4000 System.
    pub rid: u32,
    /// A short human label, derived from `rid` and falling back to the number.
    pub label: String,
}

impl Integrity {
    fn from_rid(rid: u32) -> &'static str {
        match rid {
            0x0000 => "Untrusted",
            0x1000 => "Low",
            0x2000 => "Medium",
            0x3000 => "High",
            0x4000 => "System",
            0x5000 => "ProtectedProcess",
            _ => "Custom",
        }
    }

    /// This process's own integrity level, or `None` when its own token could not be read.
    ///
    /// A read that fails here is not fatal and is not assumed either: [`Integrity::input_reach`] turns
    /// the absence into `Unknown`, and the caller refuses the input rather than guessing that it would
    /// have been allowed.
    pub fn owned_by_this_process() -> Option<Integrity> {
        own_integrity()
    }

    /// Whether input from a process at our level can reach a window at theirs.
    ///
    /// UIPI lets a process send input to windows at *equal or lower* integrity, and drops it silently
    /// otherwise. `None` on either side means the comparison could not be made, which is reported as
    /// unknown rather than assumed safe — an assumption in the permissive direction is the one that ends
    /// up clicking something by accident.
    pub fn input_reach(from: Option<&Integrity>, to: Option<&Integrity>) -> Reach {
        let (Some(ours), Some(theirs)) = (from, to) else {
            return Reach::Unknown("one side's integrity level could not be read".to_owned());
        };
        if ours.rid >= theirs.rid {
            Reach::Allowed
        } else {
            Reach::Blocked {
                ours: format!("{} ({})", ours.label, ours.sid),
                theirs: format!("{} ({})", theirs.label, theirs.sid),
            }
        }
    }
}

/// The result of comparing two integrity levels for input purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Input is expected to be delivered.
    Allowed,
    /// UIPI will drop it. The right response is a clear blocker, not a retry (R-14).
    Blocked { ours: String, theirs: String },
    /// The comparison could not be made, so nothing may be assumed either way.
    Unknown(String),
}

/// What this process's own DPI awareness ended up being.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Awareness {
    /// This call set per-monitor-aware-v2; the process is now in the coordinate space the rest of the
    /// crate assumes.
    Set,
    /// Awareness was already fixed by the time this ran — the manifest or an earlier call decided it, and
    /// Windows will not let a process change its mind. The reported value is what is actually in force.
    AlreadyFixed(String),
    /// The call failed for some other reason.
    Unavailable(String),
}

/// Everything known about one window at one instant.
#[derive(Debug, Clone)]
pub struct WindowIdentity {
    pub id: WindowId,
    pub pid: u32,
    pub thread_id: u32,
    /// The registered class of the window, when user32 would say it.
    ///
    /// Read separately from the title because it is answered by user32 rather than by the owning process,
    /// so it survives in exactly the cases where a title read might not.
    pub class_name: Option<String>,
    pub title: Option<String>,
    /// `true` when the title was longer than [`MAX_TITLE_CHARS`] and had to be cut.
    pub title_truncated: bool,
    pub is_visible: bool,
    pub is_foreground: bool,
    /// Physical-pixel bounds on the virtual desktop. `None` when the query failed.
    pub rect: Option<Rect>,
    /// DPI of the window itself. `None` when `GetDpiForWindow` failed, which is not the same as 96.
    pub dpi: Option<u32>,
    pub monitor: Option<Monitor>,
    /// Full image path of the owning process. `None` when `OpenProcess` refused, which happens for
    /// another user's processes and for some protected ones, and is not an error worth failing a task on.
    pub process_image: Option<String>,
    /// Integrity of the owning process.
    pub integrity: Option<Integrity>,
    /// Why `integrity` or `process_image` is absent, in the words of the API that refused.
    pub identity_note: Option<String>,
    /// The capture-affinity bits Windows reports for this window.
    ///
    /// This is the *only* reliable signal that a blank capture is a deliberate exclusion rather than an
    /// accident, and it is a fact about the window, not about the capture attempt.
    pub display_affinity: DisplayAffinity,
    /// When this was read. A desktop observation without a timestamp is not evidence.
    pub observed_at_ms: u64,
}

/// `GetWindowDisplayAffinity` values, named for what they mean to a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayAffinity {
    /// No restriction.
    None,
    /// Capture is replaced by the monitor's content — the older "black frame" protection.
    Monitor,
    /// Capture is excluded entirely: the window is not in the image at all.
    ExcludeFromCapture,
    /// A value this build does not name, reported with the number so a caller can tell which.
    Other(u32),
    /// The query itself failed, so nothing is known. Not the same as `None`: absence of a restriction is
    /// not the same as an unreadable one, and the plan requires the two stay distinguishable.
    NotDetermined,
}

impl DisplayAffinity {
    /// Whether a blank or absent frame is *explained* by the window's own setting.
    ///
    /// `docs/desktop-control-plan.md` requires that an explicit protection and an unexplained blank are
    /// not reported as the same fact. This is the half of that which does not depend on the pixels.
    pub fn is_explicit_protection(self) -> bool {
        matches!(self, Self::Monitor | Self::ExcludeFromCapture)
    }
}

/// Milliseconds since the Unix epoch, for stamping an observation.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// What Windows currently reports about the interactive session.
///
/// Deliberately a bag of observations rather than a verdict: the plan requires that a locked screen, a
/// disconnected session and an ordinary permission refusal stay distinguishable, and the only way to keep
/// them apart is to keep the raw answers visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionObservation {
    /// Session the physical console is attached to. `None` when there is no console session at all.
    pub console_session: Option<u32>,
    /// Session this process runs in.
    pub our_session: Option<u32>,
    /// Name of the desktop keyboard input is going to, when it could be opened.
    pub input_desktop: Option<String>,
    /// Why `input_desktop` is absent, in the words of the failing call.
    pub input_desktop_note: Option<String>,
}

impl SessionObservation {
    /// Is there a desktop to operate?
    ///
    /// "Default" is the interactive desktop. Anything else — `Winlogon` while the machine is locked or on
    /// a secure prompt, `Disconnect` for a dropped remote session — means GUI actions will land nowhere,
    /// and the honest answer is *unavailable*, not *failed*.
    pub fn interactive(&self) -> Interactive {
        match self.input_desktop.as_deref() {
            Some("Default") => {
                if let (Some(console), Some(ours)) = (self.console_session, self.our_session) {
                    if console != ours {
                        return Interactive::NotTheConsoleSession { console, ours };
                    }
                }
                Interactive::Available
            }
            Some(other) => Interactive::NonInteractiveDesktop(other.to_owned()),
            None => Interactive::Unknown(
                self.input_desktop_note
                    .clone()
                    .unwrap_or_else(|| "the input desktop could not be read".to_owned()),
            ),
        }
    }
}

/// Whether GUI work can be performed at all right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interactive {
    Available,
    /// Locked, on a secure desktop, or otherwise not the user desktop.
    NonInteractiveDesktop(String),
    /// J.E.A.N. is running in a session the console is not attached to.
    NotTheConsoleSession {
        console: u32,
        ours: u32,
    },
    /// Not determined — reported as such, never assumed available.
    Unknown(String),
}

impl Interactive {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Ask for per-monitor-v2 DPI awareness, once, before any capture or coordinate math.
///
/// This is the mitigation R-15 names in one sentence, and it is process-global and irreversible: a
/// process that has already created a window or already fixed its awareness cannot change it. So the
/// result is *reported*, not asserted — a JEAN whose host application had already pinned system-DPI
/// awareness has to know that, because every coordinate it computes is then in a space that lies on the
/// second monitor.
pub fn ensure_dpi_awareness() -> Awareness {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    let outcome =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    match outcome {
        Ok(()) => Awareness::Set,
        Err(error) => {
            // `ERROR_OPERATION_DENIED` is the already-decided case, and it is distinguishable from a
            // generic failure only by the code, so the code is what gets reported.
            let code = error.code().0 as u32;
            let in_force = awareness_in_force();
            if code == 0x8007_0005 {
                Awareness::AlreadyFixed(in_force)
            } else {
                Awareness::Unavailable(format!("{error} (in force: {in_force})"))
            }
        }
    }
}

/// The awareness Windows says is in force for one window.
///
/// Read per window rather than per process because on a mixed-DPI machine those are not the same claim,
/// and the window's own context is the one that decides what its rectangle's numbers mean.
pub fn awareness_of(id: WindowId) -> String {
    use windows::Win32::UI::HiDpi::{
        GetAwarenessFromDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
        DPI_AWARENESS_CONTEXT_UNAWARE, DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED,
    };
    let context = unsafe { GetWindowDpiAwarenessContext(id.to_raw()) };
    // The `DPI_AWARENESS` *enum* this API also offers has no value for per-monitor v2 — it collapses v1
    // and v2 into `PER_MONITOR_AWARE` — so the context is compared against the context constants
    // directly. Naming the v2 case matters here: it is the difference between a window whose DPI follows
    // the monitor it is on and one whose DPI is frozen at the primary monitor's.
    let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(context) };
    let _ = awareness;
    let named = if context == DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 {
        "per-monitor-v2"
    } else if context == DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE {
        "per-monitor"
    } else if context == DPI_AWARENESS_CONTEXT_SYSTEM_AWARE {
        "system"
    } else if context == DPI_AWARENESS_CONTEXT_UNAWARE {
        "unaware"
    } else if context == DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED {
        "unaware-gdiscaled"
    } else {
        "unknown"
    };
    named.to_owned()
}

/// The awareness in force for *this* process.
///
/// `GetWindowDpiAwarenessContext(NULL)` is documented to return the calling thread's context, which is
/// the process's awareness for every practical purpose, and it is what [`ensure_dpi_awareness`] reports
/// when Windows has already refused to be changed.
fn awareness_in_force() -> String {
    awareness_of(WindowId { hwnd: 0 })
}

/// The virtual desktop: the bounding rectangle of every monitor, in physical pixels.
///
/// `SendInput` with `MOUSEEVENTF_VIRTUALDESK` maps into exactly this space, which is why the input module
/// converts through it instead of normalising against the primary monitor — the mistake R-03 and R-15
/// both warn about, and the one a two-monitor machine punishes.
pub fn virtual_desktop() -> Rect {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    Rect {
        left,
        top,
        right: left.saturating_add(width),
        bottom: top.saturating_add(height),
    }
}

/// The window the operator is currently looking at, if any.
///
/// Reported as the *top-level* window, because that is the question every caller asks and the answer they
/// can compare against. What `GetForegroundWindow` actually hands back can be a child: measured on this
/// machine, a fixture whose own application was in front reported its edit control — a child window whose
/// text is the control's content — and the input path then refused every click with a focus change that
/// had not happened. `GA_ROOT` is the difference between "which control has focus" and "which
/// application is in front", and only the second one is a fact about the operator's attention.
pub fn foreground() -> Option<WindowId> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    Some(WindowId::from_raw(if root.0.is_null() {
        hwnd
    } else {
        root
    }))
}

/// Physical-pixel bounds of one window on the virtual desktop.
pub fn rect_of(id: WindowId) -> Option<Rect> {
    let mut rect = WinRect::default();
    unsafe { GetWindowRect(id.to_raw(), &mut rect) }
        .ok()
        .map(|_| Rect::from(rect))
}

/// The DPI Windows renders one window at.
///
/// Per-window, not per-monitor and not per-process: on a mixed-DPI setup those three are three different
/// numbers, and only this one describes the pixels inside the window being captured.
pub fn dpi_of(id: WindowId) -> Option<u32> {
    let dpi = unsafe { GetDpiForWindow(id.to_raw()) };
    if dpi == 0 {
        None
    } else {
        Some(dpi)
    }
}

/// The monitor a window is on, with its effective DPI.
pub fn monitor_of(id: WindowId) -> Option<Monitor> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

    let handle = unsafe { MonitorFromWindow(id.to_raw(), MONITOR_DEFAULTTONEAREST) };
    if handle.0.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !bool::from(unsafe { GetMonitorInfoW(handle, &mut info) }) {
        return None;
    }
    let mut dpi = 0u32;
    let mut dpi_y = 0u32;
    let _ = unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y) };
    Some(Monitor {
        x: info.rcMonitor.left,
        y: info.rcMonitor.top,
        width: info.rcMonitor.right - info.rcMonitor.left,
        height: info.rcMonitor.bottom - info.rcMonitor.top,
        dpi,
        is_primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

/// The capture affinity Windows reports for a window.
pub fn display_affinity_of(id: WindowId) -> DisplayAffinity {
    let mut value = 0u32;
    if unsafe { GetWindowDisplayAffinity(id.to_raw(), &mut value) }.is_err() {
        return DisplayAffinity::NotDetermined;
    }
    match value {
        0 => DisplayAffinity::None,
        1 => DisplayAffinity::Monitor,
        17 => DisplayAffinity::ExcludeFromCapture,
        other => DisplayAffinity::Other(other),
    }
}

/// The executable path of the process that owns a window, when it can be read.
///
/// `PROCESS_QUERY_LIMITED_INFORMATION` is the least-privileged handle that gets a name, and it is the
/// right one: this is attribution, not control. Failure is normal — another user's process, or a
/// protected one — so the reason travels with the answer instead of disappearing.
pub fn process_image_of(pid: u32) -> Result<Option<String>, String> {
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(_) => {
            return Err(format!(
                "OpenProcess for pid {pid} failed: {}",
                describe_last_error()
            ))
        }
    };
    let mut buffer = vec![0u16; 1024];
    let mut size = buffer.len() as u32;
    let read = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(PROCESS_NAME_WIN32.0),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    match read {
        Ok(()) => {
            let taken = (size as usize).min(buffer.len());
            let text = String::from_utf16_lossy(&buffer[..taken]);
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(text))
            }
        }
        Err(_) => Err(format!(
            "QueryFullProcessImageNameW for pid {pid} failed: {}",
            describe_last_error()
        )),
    }
}

/// The integrity level of a process, when its token can be opened.
///
/// This is the read R-14's mitigation depends on. It fails often and legitimately — a non-elevated JEAN
/// cannot open the token of an elevated process, and that refusal *is* the diagnosis — so the reason
/// comes back too.
pub fn integrity_of(pid: u32) -> Result<Option<Integrity>, String> {
    let handle = if pid == std::process::id() {
        unsafe { GetCurrentProcess() }
    } else {
        match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(opened) => opened,
            Err(_) => {
                return Err(format!(
                    "OpenProcess for pid {pid} failed: {}",
                    describe_last_error()
                ))
            }
        }
    };

    let mut token = HANDLE::default();
    if unsafe { OpenProcessToken(handle, TOKEN_QUERY, &mut token) }.is_err() {
        let reason = describe_last_error();
        if pid != std::process::id() {
            let _ = unsafe { CloseHandle(handle) };
        }
        return Err(format!("OpenProcessToken for pid {pid} failed: {reason}"));
    }

    let integrity = read_integrity(token).map(Some);

    let _ = unsafe { CloseHandle(token) };
    if pid != std::process::id() {
        let _ = unsafe { CloseHandle(handle) };
    }
    integrity
}

/// Our own integrity level.
pub fn own_integrity() -> Option<Integrity> {
    integrity_of(std::process::id()).ok().flatten()
}

fn read_integrity(token: HANDLE) -> Result<Integrity, String> {
    let mut needed = 0u32;
    // The size probe always fails — that is how the length is obtained — so only its output is read.
    let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed) };
    if needed == 0 {
        return Err("GetTokenInformation reported no integrity level".to_owned());
    }
    let mut buffer = vec![0u8; needed as usize];
    let mut returned = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr() as *mut c_void),
            needed,
            &mut returned,
        )
    }
    .map_err(|_| format!("GetTokenInformation failed: {}", describe_last_error()))?;

    let attributes: *const SID_AND_ATTRIBUTES = buffer.as_ptr() as *const SID_AND_ATTRIBUTES;
    if attributes.is_null() {
        return Err("the integrity level was absent from the token".to_owned());
    }
    let sid = unsafe { (*attributes).Sid };
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    if count == 0 {
        return Err("the integrity SID carried no sub-authority".to_owned());
    }
    let rid = unsafe { *GetSidSubAuthority(sid, (count - 1) as u32) };

    // The string form is what a report should quote, so a caller can see `S-1-16-8192` rather than a
    // number they have to look up. `LocalFree` is the documented owner of that buffer.
    let mut string_sid = PWSTR::null();
    let sid_text = if unsafe { ConvertSidToStringSidW(sid, &mut string_sid) }.is_ok() {
        // The buffer belongs to the allocator until it is freed, so the text is read first and the
        // handle released immediately after — `LocalFree` takes `Option<HLOCAL>` and does the freeing.
        let text = sid_to_string(&string_sid);
        unsafe {
            windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                string_sid.0 as *mut c_void,
            )))
        };
        text
    } else {
        format!("S-1-16-{rid}")
    };

    Ok(Integrity {
        sid: sid_text,
        rid,
        label: Integrity::from_rid(rid).to_owned(),
    })
}

/// What the operator's session is doing, as a set of observations.
///
/// `OpenInputDesktop` is how a lock is detected without asking the shell, and it fails with a plain
/// access denial on a locked machine *and* on a secure-desktop prompt — which is exactly why the name of
/// the desktop, when it can be read, is reported separately from the failure, when it cannot.
pub fn session() -> SessionObservation {
    let console = {
        let id = unsafe { WTSGetActiveConsoleSessionId() };
        // 0xFFFFFFFF is documented as "no console session attached".
        if id == u32::MAX {
            None
        } else {
            Some(id)
        }
    };
    let our_session = {
        let mut id = 0u32;
        if unsafe { ProcessIdToSessionId(std::process::id(), &mut id) }.is_ok() {
            Some(id)
        } else {
            None
        }
    };

    let mut input_desktop = None;
    let mut note = None;
    match unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0),
        )
    } {
        Ok(desktop) => {
            input_desktop = read_desktop_name(desktop);
            if input_desktop.is_none() {
                note = Some("the input desktop opened but its name could not be read".to_owned());
            }
            let _ = unsafe { CloseDesktop(desktop) };
        }
        Err(_) => {
            note = Some(format!(
                "OpenInputDesktop failed: {}",
                describe_last_error()
            ));
        }
    }

    SessionObservation {
        console_session: console,
        our_session,
        input_desktop,
        input_desktop_note: note,
    }
}

fn read_desktop_name(desktop: HDESK) -> Option<String> {
    let mut needed = 0u32;
    let _ = unsafe {
        GetUserObjectInformationW(HANDLE(desktop.0), UOI_NAME, None, 0, Some(&mut needed))
    };
    if needed == 0 {
        return None;
    }
    let mut buffer = vec![0u8; needed as usize];
    let mut returned = 0u32;
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr() as *mut c_void),
            needed,
            Some(&mut returned),
        )
    }
    .ok()?;
    let units: Vec<u16> = buffer
        .chunks(2)
        .filter(|pair| pair.len() == 2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    let text = String::from_utf16_lossy(&units[..end]);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Read a null-terminated UTF-16 string out of a `PWSTR` the caller still owns.
///
/// Written by hand rather than through a helper because the buffer is borrowed from `ConvertSidToStringSidW`
/// and freed one line later: the only correct implementation is the one that stops at the first null and
/// never outlives the caller's free.
fn sid_to_string(value: &windows::core::PWSTR) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(24);
    let mut offset = 0usize;
    loop {
        let unit = unsafe { *(value.0 as *const u16).add(offset) };
        if unit == 0 {
            break;
        }
        units.push(unit);
        offset += 1;
        // A SID string cannot legitimately be thousands of characters, and a runaway loop here would be
        // reading past the buffer the API allocated.
        if offset > 512 {
            break;
        }
    }
    String::from_utf16_lossy(&units)
}

/// A human-readable `GetLastError`, for the notes that explain a refusal.
fn describe_last_error() -> String {
    let code = unsafe { GetLastError() }.0;
    format!("Win32 error {code:#010x}")
}

/// Convert a point in *window* coordinates (the space UIA's bounding rectangles are in for a
/// per-monitor-aware window) into physical pixels on the virtual desktop.
///
/// `ClientToScreen` is the documented conversion and it accounts for both the window's own DPI context and
/// the monitor it sits on, which is the whole reason this function exists instead of an arithmetic guess.
pub fn window_point_to_screen(id: WindowId, x: i32, y: i32) -> Option<(i32, i32)> {
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    let mut point = POINT { x, y };
    if bool::from(unsafe { ClientToScreen(id.to_raw(), &mut point) }) {
        Some((point.x, point.y))
    } else {
        None
    }
}

/// The window class name, when it can be read.
///
/// `GetClassNameW` is answered by user32 from the class registration, not by the owning process, so it is
/// available in exactly the case where the *title* and the UIA tree may not be. It is the cheapest
/// available hint about an embedded web-content frame — `Chrome_WidgetWin_1` and friends — and it is a
/// hint about the shell of a window, never a conclusion about its content.
pub fn class_name_of(id: WindowId) -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buffer = [0u16; 256];
    let length = unsafe { GetClassNameW(id.to_raw(), &mut buffer) };
    if length <= 0 {
        return None;
    }
    let taken = (length as usize).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..taken]);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Read everything about one window, in one place, with the failures kept as reasons.
///
/// Purely a set of Win32 reads; the bound and the cancellation live with the caller that runs it on a
/// worker, because a title read is the only cross-process call here and the OS already bounds it.
pub fn read_identity(id: WindowId) -> WindowIdentity {
    let mut pid = 0u32;
    let thread_id = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            id.to_raw(),
            Some(&mut pid),
        )
    };
    let raw_title_length = unsafe { GetWindowTextLengthW(id.to_raw()) }.max(0) as usize;
    let title = id.title();

    let (process_image, integrity, identity_note) = if pid == 0 {
        (
            None,
            None,
            Some("the window has no owning process".to_owned()),
        )
    } else {
        let mut note: Option<String> = None;
        let image = match process_image_of(pid) {
            Ok(value) => value,
            Err(reason) => {
                note = Some(reason);
                None
            }
        };
        let integrity = match integrity_of(pid) {
            Ok(value) => value,
            // The integrity refusal is the more load-bearing of the two reasons (R-14), so it wins if
            // both failed.
            Err(reason) => {
                if note.is_none() {
                    note = Some(reason);
                }
                None
            }
        };
        (image, integrity, note)
    };

    WindowIdentity {
        id,
        pid,
        thread_id,
        class_name: class_name_of(id),
        title,
        title_truncated: raw_title_length > MAX_TITLE_CHARS,
        is_visible: bool::from(unsafe { IsWindowVisible(id.to_raw()) }),
        is_foreground: foreground().map(|front| front == id).unwrap_or(false),
        rect: rect_of(id),
        dpi: dpi_of(id),
        monitor: monitor_of(id),
        process_image,
        integrity,
        identity_note,
        display_affinity: display_affinity_of(id),
        observed_at_ms: now_ms(),
    }
}

/// Every visible window that an operator would recognise, with its identity.
///
/// The filter itself is `jean_tools::desktop`'s — the same definition `jean launch` verifies against, so
/// the actuator and the probe cannot disagree about what a window is.
pub fn enumerate() -> WindowsResult<Vec<WindowIdentity>> {
    use jean_tools::desktop::{counts_as_operator_window, SystemWindowStation, WindowStation};
    let windows = SystemWindowStation
        .top_level_windows()
        .map_err(|error| WindowsError::Win32 {
            call: "EnumWindows".to_owned(),
            reason: error.to_string(),
        })?;
    let mut out = Vec::new();
    for raw in windows {
        if !counts_as_operator_window(&raw) {
            continue;
        }
        out.push(read_identity(WindowId { hwnd: raw.hwnd }));
    }
    Ok(out)
}

/// A stable, human-addressable description of an identity for reports and audit notes.
///
/// Deliberately excludes the title beyond a bounded prefix: a title is another process's text, and pasting
/// it into an audit line unbounded is how a log ends up quoting a document name the operator never
/// intended to record.
impl WindowIdentity {
    pub fn audit_label(&self) -> String {
        let title = self
            .title
            .as_deref()
            .map(|text| {
                let cut: String = text.chars().take(80).collect();
                cut
            })
            .unwrap_or_else(|| "<untitled>".to_owned());
        let process = self
            .process_image
            .as_deref()
            .and_then(|path| path.rsplit(['\\', '/']).next())
            .unwrap_or("unknown");
        format!(
            "hwnd={} pid={pid} {process} \"{title}\"",
            self.id.hwnd,
            pid = self.pid
        )
    }

    /// Whether the window is where the identity said it was a moment ago.
    ///
    /// A verifier calls this after an action: "the control moved" and "the control vanished" are both
    /// reasons not to trust a coordinate read before the action.
    pub fn same_bounds_as(&self, other: &WindowIdentity) -> bool {
        self.rect.is_some() && self.rect == other.rect
    }
}
