//! Running a blocking desktop call in a **child process that can be killed**.
//!
//! The Phase-4 requirement was explicit, and it was about the thing this module replaces: *"a hung
//! accessibility provider must be bounded by a worker timeout; a timed-out thread alone is not adequate
//! isolation if it remains stuck indefinitely."* [`crate::bounded`] could bound the *wait* and nothing
//! else: a thread parked inside a cross-process `SendMessage` cannot be made to return, so every hung
//! provider left one more parked thread, one more stack and one more COM apartment in the long-running
//! process, and the best available answer was a cap on how many leaks were tolerated at once.
//!
//! So the potentially blocking calls do not run here. They run in `jean-desktop-worker.exe`, a child
//! started through the same supervision every other J.E.A.N. child gets
//! (`crates/jean-process/src/piped.rs`: `CREATE_SUSPENDED`, Job Object with `KILL_ON_JOB_CLOSE`, and a
//! [`Control`] that *another* thread may pull the trigger on while the owner is parked in `ReadFile` —
//! which is precisely the composition this design needs). A provider that never answers costs a
//! **process**, and a process can be terminated with its whole tree. Then the parent has nothing left
//! parked: the pipes close, the two pump threads see EOF and are joined, the job handle is closed, and
//! the next call starts a fresh worker.
//!
//! Three consequences worth stating before the code:
//!
//! * **The leak is removed as a mechanism, not as a number.** Accounting remains ([`live_workers`]), but
//!   it counts processes that are demonstrably alive, and it stops counting them when their exit is
//!   observed. There is no state in which the parent is carrying an unkillable stuck thread.
//! * **An abandoned call is still uncertain.** Killing the worker does not undo an `Invoke` it had
//!   already delivered. The caller's vocabulary is unchanged — [`Stop::TimedOut`] and [`Stop::Cancelled`]
//!   map to the same `ProviderTimeout`/`Cancelled` errors the thread design produced — and
//!   `jean_core::desktop` still refuses to replay them (D-050).
//! * **There is no silent fallback.** If the worker binary cannot be found or started, or answers the
//!   handshake with a different [`PROTOCOL_VERSION`], desktop work is **refused with the reason**.
//!   Falling back to in-process provider calls would reintroduce the isolation this module exists to
//!   provide, behind a success message.
//!
//! ## The protocol
//!
//! One JSON line per request into the child's stdin, one JSON line per reply on its stdout, both bounded
//! ([`MAX_REQUEST_BYTES`], [`MAX_REPLY_BYTES`]) so a confused worker cannot make the parent read or
//! allocate without limit. The child is this repository's own binary running this repository's own code,
//! so the trust question is version drift rather than authorship, and the handshake answers it.
//!
//! Frames do not travel over the pipe: a 4 MP BGRA buffer is 16 MB, and base64 of it in a line the parent
//! parses byte-by-byte would be the worst capture path available. The child writes the raw pixels into a
//! directory **the parent created and owns** and returns the metadata; the parent reads the file back and
//! [`FrameMeta::into_capture`] refuses a byte count that does not match the metadata, so a half-written
//! frame is reported as an incomplete one rather than as a capture.
//!
//! ## What still runs in-process
//!
//! [`crate::input`] (`SendInput`) and the read-only `identity`/`jean-tools` enumeration. `SendInput`
//! posts to the system input queue; it does not send a message into another application's window proc,
//! and it is not the call the requirement was about. Moving it would mean performing an *action* in a
//! process that can be killed mid-action, which is worse for the no-replay rule than the local call it
//! would replace.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use jean_process::piped::{Control as WorkerControl, PipedChild};
use jean_process::spec::{LaunchSpec, TerminalMode};

use crate::accessibility::{Action, Control, Effect, Locator, Probe, StateRead, Tree};
use crate::capture::{Backend, Capture, FrameQuality};
use crate::error::{WindowsError, WindowsResult};
use crate::identity::WindowId;

/// The wire format's version, checked on every handshake.
///
/// Bump it when a request or reply shape changes in a way an older worker would misread. A mismatch is a
/// refusal with both versions named, never a negotiation.
pub const PROTOCOL_VERSION: u32 = 3;

/// The name of the sibling binary that does the work.
const WORKER_BINARY: &str = "jean-desktop-worker.exe";

/// The environment variable that overrides where that binary is.
///
/// Exists for the test suites, which do not run from the directory cargo put the worker in, and for an
/// operator who installs the two executables apart. Read once per session.
pub const WORKER_PATH_ENV: &str = "JEAN_DESKTOP_WORKER";

/// The variable cargo sets for a test binary so it can find this package's own bin.
///
/// Not configured by anyone; it is how the integration tests reach the worker without a path guess.
const CARGO_BIN_ENV: &str = "CARGO_BIN_EXE_jean-desktop-worker";

/// Ceiling on one request line.
///
/// A locator plus an action is a few kilobytes; 1 MB is the point at which a `SetValue` payload is
/// already a document, and a document does not belong in a control's Value pattern.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;

/// Ceiling on one reply line.
///
/// A capped 2 000-control tree with 256-character names is under 1 MB, so 8 MB is headroom, not a plan.
/// The bound is what stops a worker that has gone wrong from making the parent read without limit.
pub const MAX_REPLY_BYTES: usize = 8 << 20;

/// How much of the worker's stderr is retained for a failure message.
///
/// Our own diagnostics, kept short because what a report needs is the last thing it said, not its
/// transcript.
const MAX_STDERR_TAIL: usize = 512;

/// How long the wait is sliced into, so a cancel and a deadline are both seen promptly.
///
/// The same number [`crate::bounded`] uses, for the same reason: responsiveness is a property of the poll
/// interval, not of the deadline.
const POLL_SLICE: Duration = Duration::from_millis(25);

/// How long a politely closed worker may take to exit before the job object answers for it.
///
/// Public because the guarantee is stated against it: a session that completed its work must never reach
/// this bound, and a test that asserts that needs the number the design is actually using.
pub const SHUTDOWN_BOUND: Duration = Duration::from_secs(2);

/// Worker processes this process believes are alive.
///
/// Incremented when a spawn succeeds, decremented when its exit is observed. A worker whose exit never
/// becomes observable stays counted, which is the point: the budget then refuses further desktop work
/// instead of adding a process to a pile nobody is accounting for.
static LIVE_WORKERS: AtomicU32 = AtomicU32::new(0);

/// The session's frame directory needs unique names without a clock dependency.
static FRAME_SEQ: AtomicU64 = AtomicU64::new(1);

/// Worker processes alive right now, parent-side.
pub fn live_workers() -> u32 {
    LIVE_WORKERS.load(Ordering::SeqCst)
}

/// One thing a worker is asked to do.
///
/// Each variant carries the bounds it needs rather than a `Settings` struct, because the timeout belongs
/// to the parent — the parent is the party that decides when to give up — and a child must not be told how
/// long to wait for itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    /// Version negotiation, sent before anything else to a fresh worker.
    Hello {
        version: u32,
        parent_pid: u32,
    },
    ReadTree {
        window: isize,
        max_nodes: usize,
        probe_patterns: bool,
    },
    Resolve {
        window: isize,
        locator: Locator,
        max_nodes: usize,
        /// Whether the resolved controls' pattern lists are filled in.
        ///
        /// Part of the request rather than a constant in the worker because it is the *caller's* bound:
        /// `Settings::probe_patterns` decides whether a listing carries each control's patterns, and a
        /// worker that always answered `false` would silently empty the one field a pattern decision is
        /// made from. That is not hypothetical — the first version of this enum did exactly that, and a
        /// native test caught it by finding a combobox with no patterns at all.
        probe_patterns: bool,
    },
    Probe {
        window: isize,
        max_nodes: usize,
    },
    ReadState {
        window: isize,
        locator: Locator,
        probe_patterns: bool,
        max_nodes: usize,
    },
    /// Re-resolve, act, read back, and read the witness — all inside the one request.
    ///
    /// The whole action is one conversation because its before/after pair has to describe the same
    /// control instance. Splitting it across two requests would let a layout change land between them and
    /// have the report attribute that change to the action.
    Act {
        window: isize,
        locator: Locator,
        action: Action,
        witness: Option<Locator>,
        probe_patterns: bool,
        max_nodes: usize,
    },
    /// One capture attempt, with the mechanism named so the parent's recovery rule can try the alternate.
    Capture {
        window: isize,
        max_pixels: usize,
        backend: Backend,
        /// Where the raw BGRA buffer goes. A directory the parent created and owns; `None` asks the
        /// worker to report metadata only, which is how a quality check costs no pixels.
        frame_dir: Option<String>,
    },
}

/// A request line: the sequence number the reply must answer, and the work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub body: Body,
}

/// What a worker may answer.
///
/// No `PartialEq`: the payloads it carries — a [`Tree`], an [`Effect`] — are the crate's read types, and
/// they are compared by the fields that mean something, not by deep equality of every name a provider
/// happened to supply. A test that needs to know whether the wire preserved an answer compares the fields
/// it cares about, which is the same rule the rest of the crate follows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyBody {
    /// The handshake's answer. A stale binary is named by its own version rather than misdiagnosed as a
    /// hung provider.
    Pong {
        version: u32,
        pid: u32,
    },
    Tree(Tree),
    Controls(Vec<Control>),
    Probe(Probe),
    State(StateRead),
    /// Boxed because an action's outcome carries the control it acted on and the readback before and
    /// after, which together outweigh every other answer this enum can carry; the box keeps a reply line
    /// one pointer wide instead of moving hundreds of bytes for a `Pong`.
    Effect(Box<Effect>),
    Frame(FrameMeta),
    /// The call failed *inside* the worker, carrying the crate's own error vocabulary across.
    ///
    /// This is an answer, not a conversation failure: the worker was reachable, understood the request,
    /// and is reporting what the desktop said.
    Failure(WireError),
    /// The request could not be honoured at all — unknown version, unparseable line, or a body this
    /// worker does not implement.
    Refused {
        reason: String,
    },
}

/// A reply line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    pub body: ReplyBody,
}

impl ReplyBody {
    /// A short name for this answer, for messages that must describe a shape mismatch without dumping a
    /// payload that may contain somebody else's text.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Pong { .. } => "pong",
            Self::Tree(_) => "tree",
            Self::Controls(_) => "controls",
            Self::Probe(_) => "probe",
            Self::State(_) => "state",
            Self::Effect(_) => "effect",
            Self::Frame(_) => "frame",
            Self::Failure(_) => "failure",
            Self::Refused { .. } => "refused",
        }
    }

    /// The answer, or the failure the worker reported.
    pub fn into_result(self) -> WindowsResult<Self> {
        match self {
            Self::Failure(wire) => Err(wire.to_windows_error()),
            other => Ok(other),
        }
    }
}

/// The shape a frame takes on the wire.
///
/// Identical to [`Capture`] except that it has no pixels, because 16 MB of them do not go through a pipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMeta {
    pub window: isize,
    pub width: u32,
    pub height: u32,
    pub hash: u64,
    pub mean_luma: u8,
    pub uniform: bool,
    pub uniform_argb: Option<u32>,
    pub render_dpi: Option<u32>,
    pub captured_at_ms: u64,
    pub backend: Backend,
    /// The file the worker wrote, or `None` when the parent asked for metadata only.
    pub file: Option<String>,
    /// The bytes, once the parent has taken them.
    ///
    /// Not on the wire: it is filled in by [`one`] while the frame directory still exists, because the
    /// directory is deleted when the session ends and a frame the parent never managed to pick up is not
    /// evidence about a window. `into_capture` falls back to reading [`FrameMeta::file`] when this is
    /// `None`, which is the path a hand-built metadata takes.
    #[serde(skip, default)]
    pub pixels: Option<Vec<u8>>,
}

impl FrameMeta {
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }

    /// What the frame looks like, computed from the metadata alone.
    ///
    /// The same rule the in-process classifier applies, so a metadata-only read and a full capture cannot
    /// disagree about whether a frame is informative.
    pub fn quality(&self) -> FrameQuality {
        if self.uniform {
            let argb = self.uniform_argb.unwrap_or(0);
            if argb & 0x00FF_FFFF == 0 {
                FrameQuality::UniformBlack
            } else {
                FrameQuality::UniformColor
            }
        } else if self.mean_luma < 8 {
            FrameQuality::NearlyBlack
        } else if self.mean_luma > 247 {
            FrameQuality::NearlyWhite
        } else {
            FrameQuality::Usable
        }
    }

    /// Read the worker's file into this metadata, while the session that owns the directory is alive.
    ///
    /// A failure here is deliberately silent: `pixels` stays `None`, and the only caller that needs the
    /// bytes — [`FrameMeta::into_capture`] — reports it as the read failure it actually is, with the path
    /// in the message. A frame that cannot be fetched is not a frame.
    fn load_pixels(&mut self) {
        let Some(file) = self.file.clone() else {
            return;
        };
        let expected = self.pixel_count().saturating_mul(4);
        let mut pixels = Vec::new();
        // The ceiling is the metadata's own byte count plus one, so a worker that wrote more than it
        // claimed is detected rather than allocated.
        let read = std::fs::File::open(&file)
            .and_then(|handle| handle.take((expected + 1) as u64).read_to_end(&mut pixels));
        if read.is_ok() {
            self.pixels = Some(pixels);
        }
    }

    /// Read the frame back into the type the rest of the crate uses.
    ///
    /// A byte count that does not match the metadata is an error, not a padded buffer: half a frame is
    /// not evidence about a window.
    pub fn into_capture(self) -> WindowsResult<Capture> {
        let file = self
            .file
            .clone()
            .ok_or_else(|| WindowsError::CaptureUnusable {
                hwnd: self.window,
                reason: "the worker reported a frame with no file behind it".to_owned(),
            })?;
        let expected =
            self.pixel_count()
                .checked_mul(4)
                .ok_or_else(|| WindowsError::CaptureUnusable {
                    hwnd: self.window,
                    reason: "the worker reported a frame size that overflows".to_owned(),
                })?;
        let pixels = match self.pixels {
            Some(pixels) => pixels,
            None => {
                let mut pixels = Vec::new();
                std::fs::File::open(&file)
                    .and_then(|handle| handle.take((expected + 1) as u64).read_to_end(&mut pixels))
                    .map_err(|error| WindowsError::CaptureUnusable {
                        hwnd: self.window,
                        reason: format!(
                            "the worker claimed {expected} bytes of pixels at {file} and the read failed: {error}"
                        ),
                    })?;
                pixels
            }
        };
        if pixels.len() != expected {
            // The read was capped at `expected + 1`, so a file longer than its metadata is described as
            // the lower bound it actually is rather than as a byte count nobody measured.
            let reason = if pixels.len() < expected {
                format!(
                    "the worker's frame is {} bytes, not the {expected} its metadata claims; the capture \
                     is incomplete and is not reported as one",
                    pixels.len()
                )
            } else {
                format!(
                    "the worker's frame file is longer than the {expected} bytes its metadata claims; it \
                     is not reported as a capture"
                )
            };
            return Err(WindowsError::CaptureUnusable {
                hwnd: self.window,
                reason,
            });
        }
        Ok(Capture {
            window: WindowId { hwnd: self.window },
            width: self.width,
            height: self.height,
            pixels,
            backend: self.backend,
            hash: self.hash,
            mean_luma: self.mean_luma,
            uniform: self.uniform,
            uniform_argb: self.uniform_argb,
            render_dpi: self.render_dpi,
            captured_at_ms: self.captured_at_ms,
        })
    }
}

/// [`WindowsError`] as it crosses the pipe.
///
/// A mirror rather than a derive on the error type, for two reasons that are both about drift: the wire
/// vocabulary is a contract between two binaries that may not be the same age, and it should change loudly
/// rather than whenever an in-process type is refactored. [`WireError::Unknown`] is what that buys — a
/// variant this side has never heard of arrives *named*, instead of failing to parse and costing the whole
/// conversation.
///
/// Every variant round-trips, including the call names, so nothing above this module can tell that the
/// answer came from another process. That is the property the two `String` fields in
/// [`WindowsError::Panicked`] and [`WindowsError::Win32`] exist to protect: any attempt to borrow them back
/// out of a closed list of names would silently rewrite a failure's identity when the list went stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireError {
    StaleWindow {
        hwnd: isize,
    },
    NoSuchWindow {
        wanted: String,
    },
    AccessibilityUnavailable {
        reason: String,
    },
    ProviderTimeout {
        hwnd: isize,
        budget_secs: u64,
        waited_ms: u64,
    },
    Cancelled {
        budget_secs: u64,
        waited_ms: u64,
    },
    Panicked {
        call: String,
    },
    Ambiguous {
        locator: String,
        count: usize,
    },
    PatternUnsupported {
        control: String,
        pattern: String,
    },
    StaleElement {
        control: String,
    },
    FocusChanged {
        expected: String,
        actual: String,
    },
    IntegrityBlocked {
        hwnd: isize,
        reason: String,
    },
    SessionUnavailable {
        reason: String,
    },
    CaptureUnusable {
        hwnd: isize,
        reason: String,
    },
    File {
        path: String,
        reason: String,
    },
    InvalidSetting {
        reason: String,
    },
    Win32 {
        call: String,
        reason: String,
    },
    /// A variant the other side has and this side does not.
    ///
    /// The honest answer to protocol drift: reported as an unknown failure with the original text, never
    /// guessed into the nearest known variant, because the nearest known variant is a claim about a
    /// different mechanism.
    Unknown {
        kind: String,
        message: String,
    },
}

impl WireError {
    /// Carry a worker's failure into the parent's vocabulary.
    pub fn to_windows_error(self) -> WindowsError {
        match self {
            Self::StaleWindow { hwnd } => WindowsError::StaleWindow { hwnd },
            Self::NoSuchWindow { wanted } => WindowsError::NoSuchWindow { wanted },
            Self::AccessibilityUnavailable { reason } => {
                WindowsError::AccessibilityUnavailable { reason }
            }
            Self::ProviderTimeout {
                hwnd,
                budget_secs,
                waited_ms,
            } => WindowsError::ProviderTimeout {
                hwnd,
                budget_secs,
                waited_ms,
            },
            Self::Cancelled {
                budget_secs,
                waited_ms,
            } => WindowsError::Cancelled {
                budget_secs,
                waited_ms,
            },
            Self::Panicked { call } => WindowsError::Panicked { call },
            Self::Ambiguous { locator, count } => WindowsError::Ambiguous { locator, count },
            Self::PatternUnsupported { control, pattern } => {
                WindowsError::PatternUnsupported { control, pattern }
            }
            Self::StaleElement { control } => WindowsError::StaleElement { control },
            Self::FocusChanged { expected, actual } => {
                WindowsError::FocusChanged { expected, actual }
            }
            Self::IntegrityBlocked { hwnd, reason } => {
                WindowsError::IntegrityBlocked { hwnd, reason }
            }
            Self::SessionUnavailable { reason } => WindowsError::SessionUnavailable { reason },
            Self::CaptureUnusable { hwnd, reason } => WindowsError::CaptureUnusable { hwnd, reason },
            Self::File { path, reason } => WindowsError::File {
                path: PathBuf::from(path),
                reason,
            },
            Self::InvalidSetting { reason } => WindowsError::InvalidSetting { reason },
            Self::Win32 { call, reason } => WindowsError::Win32 { call, reason },
            Self::Unknown { kind, message } => WindowsError::AccessibilityUnavailable {
                reason: format!(
                    "the desktop worker reported a failure this build cannot name ({kind}): {message}"
                ),
            },
        }
    }
}

impl From<&WindowsError> for WireError {
    fn from(error: &WindowsError) -> Self {
        match error {
            WindowsError::StaleWindow { hwnd } => Self::StaleWindow { hwnd: *hwnd },
            WindowsError::NoSuchWindow { wanted } => Self::NoSuchWindow {
                wanted: wanted.clone(),
            },
            WindowsError::AccessibilityUnavailable { reason } => Self::AccessibilityUnavailable {
                reason: reason.clone(),
            },
            WindowsError::ProviderTimeout {
                hwnd,
                budget_secs,
                waited_ms,
            } => Self::ProviderTimeout {
                hwnd: *hwnd,
                budget_secs: *budget_secs,
                waited_ms: *waited_ms,
            },
            WindowsError::Cancelled {
                budget_secs,
                waited_ms,
            } => Self::Cancelled {
                budget_secs: *budget_secs,
                waited_ms: *waited_ms,
            },
            WindowsError::Panicked { call } => Self::Panicked { call: call.clone() },
            WindowsError::Ambiguous { locator, count } => Self::Ambiguous {
                locator: locator.clone(),
                count: *count,
            },
            WindowsError::PatternUnsupported { control, pattern } => Self::PatternUnsupported {
                control: control.clone(),
                pattern: pattern.clone(),
            },
            WindowsError::StaleElement { control } => Self::StaleElement {
                control: control.clone(),
            },
            WindowsError::FocusChanged { expected, actual } => Self::FocusChanged {
                expected: expected.clone(),
                actual: actual.clone(),
            },
            WindowsError::IntegrityBlocked { hwnd, reason } => Self::IntegrityBlocked {
                hwnd: *hwnd,
                reason: reason.clone(),
            },
            WindowsError::SessionUnavailable { reason } => Self::SessionUnavailable {
                reason: reason.clone(),
            },
            WindowsError::CaptureUnusable { hwnd, reason } => Self::CaptureUnusable {
                hwnd: *hwnd,
                reason: reason.clone(),
            },
            WindowsError::File { path, reason } => Self::File {
                path: path.to_string_lossy().into_owned(),
                reason: reason.clone(),
            },
            WindowsError::InvalidSetting { reason } => Self::InvalidSetting {
                reason: reason.clone(),
            },
            WindowsError::Win32 { call, reason } => Self::Win32 {
                call: call.clone(),
                reason: reason.clone(),
            },
        }
    }
}

/// How the *conversation* failed, as distinct from how the call failed.
///
/// The distinction is why this type exists. A [`ReplyBody::Failure`] is the desktop answering with a
/// refusal; a `Stop` is J.E.A.N. giving up on the conversation, which may mean a side effect landed with
/// nobody watching. That difference decides whether a caller may retry (D-050).
#[derive(Debug, Clone)]
pub enum Stop {
    /// The worker did not answer in time. It has been terminated with its tree.
    TimedOut { waited: Duration },
    /// The run was cancelled. Same, and the call itself was never retractable.
    Cancelled { waited: Duration },
    /// The worker died, closed its streams, or wrote something that is not a reply.
    Lost { reason: String },
    /// A worker was never started, so nothing was attempted and no side effect is possible.
    Unavailable { reason: String },
}

impl Stop {
    /// The one sentence a caller needs for its own error type.
    pub fn reason(&self) -> String {
        match self {
            Self::TimedOut { waited } => {
                format!("the worker did not answer within {}ms", waited.as_millis())
            }
            Self::Cancelled { waited } => {
                format!(
                    "the run was cancelled after {}ms of waiting",
                    waited.as_millis()
                )
            }
            Self::Lost { reason } => reason.clone(),
            Self::Unavailable { reason } => reason.clone(),
        }
    }

    /// Whether anything the worker was asked to do may have happened.
    ///
    /// `false` only for [`Stop::Unavailable`], where no process ever ran. Every other stop is treated as
    /// possibly-effective, which keeps the no-replay rule from depending on a caller remembering which
    /// stop code means what.
    pub fn may_have_acted(&self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }
}

/// A directory one session owns for its frames, removed with the session.
///
/// The parent creates it and names it; the worker writes inside it. Deleting it therefore deletes only
/// files this process made, which is the whole reason frames are not cleaned up by pattern.
struct FrameDir {
    path: PathBuf,
}

impl FrameDir {
    fn new() -> WindowsResult<Self> {
        let root = std::env::temp_dir()
            .join("jean-desktop-frames")
            .join(format!(
                "{}-{}",
                std::process::id(),
                FRAME_SEQ.fetch_add(1, Ordering::SeqCst)
            ));
        std::fs::create_dir_all(&root).map_err(|error| WindowsError::File {
            path: root.clone(),
            reason: format!("the worker's frame directory could not be created: {error}"),
        })?;
        Ok(Self { path: root })
    }

    fn path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for FrameDir {
    fn drop(&mut self) {
        // Nothing lives in here but frames this session's own worker wrote to this exact path. A failure
        // is a leftover file, not a reason to fail a desktop call that already answered.
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %self.path.display(), %error, "worker frame directory left behind");
            }
        }
    }
}

/// One live worker process, asked one thing at a time.
pub struct Session {
    child: PipedChild,
    control: WorkerControl,
    /// The parent's own copy of the write end of the worker's stdin.
    ///
    /// `Option` because it has to be closed *before* the session ends, not with it — see
    /// [`Session::shutdown`].
    stdin: Option<std::fs::File>,
    replies: Receiver<String>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    stdout_pump: Option<JoinHandle<()>>,
    stderr_pump: Option<JoinHandle<()>>,
    frames: FrameDir,
    next_id: u64,
    dead: bool,
    reaped: bool,
}

impl Session {
    /// Start a worker and complete its handshake, bounded by [`startup_bound`] of `budget`.
    pub fn start(budget: Duration) -> WindowsResult<Self> {
        let startup = startup_bound(budget);
        let exe = worker_program()?;
        let spec = LaunchSpec {
            executable: exe.clone(),
            args: Vec::new(),
            working_dir: None,
            env: worker_environment(&exe),
            // The parent owns every deadline, and `piped::spawn` does not read this field.
            timeout: None,
            output_cap_bytes: MAX_REPLY_BYTES as u64,
            stdout_path: None,
            stderr_path: None,
            terminal: TerminalMode::Pipes,
        };
        let child = jean_process::piped::spawn(&spec).map_err(|error| {
            WindowsError::AccessibilityUnavailable {
                reason: format!(
                    "the desktop worker at {} could not be started: {error}",
                    exe.display()
                ),
            }
        })?;
        LIVE_WORKERS.fetch_add(1, Ordering::SeqCst);
        let control = child.control();
        // Every failure before a session object exists has to end the process *and* come off the count,
        // or a wiring problem would leave phantom workers in the budget forever.
        let abort = |reason: String| -> WindowsError {
            abort_worker(&control);
            WindowsError::AccessibilityUnavailable {
                reason: format!("the desktop worker could not be wired: {reason}"),
            }
        };
        let stdin = child
            .stdin_file()
            .map_err(|error| abort(format!("its stdin could not be taken: {error}")))?;
        let stdout = child
            .stdout_file()
            .map_err(|error| abort(format!("its stdout could not be taken: {error}")))?;
        let stderr = child
            .stderr_file()
            .map_err(|error| abort(format!("its stderr could not be taken: {error}")))?;

        let (tx, replies) = mpsc::channel::<String>();
        let stdout_pump = std::thread::spawn(move || pump_stdout(stdout, tx));
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_pump = {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || pump_stderr(stderr, tail))
        };

        // The frame directory is created before the session exists, because a session without somewhere to
        // put a frame is a session that will fail later for a reason nobody can see coming.
        let frames = match FrameDir::new() {
            Ok(frames) => frames,
            Err(error) => {
                abort_worker(&control);
                // Both reads hit EOF the moment the job object kills the child, so joining here is
                // bounded by the kill rather than by the provider.
                let _ = stdout_pump.join();
                let _ = stderr_pump.join();
                return Err(error);
            }
        };

        let mut session = Self {
            child,
            control,
            stdin: Some(stdin),
            replies,
            stderr_tail,
            stdout_pump: Some(stdout_pump),
            stderr_pump: Some(stderr_pump),
            frames,
            next_id: 1,
            dead: false,
            reaped: false,
        };

        // The handshake is also the liveness probe. A worker that starts and never answers has already
        // failed, and the operator should hear "the worker did not complete its handshake" rather than a
        // timeout blamed on the application under inspection.
        let hello = Body::Hello {
            version: PROTOCOL_VERSION,
            parent_pid: std::process::id(),
        };
        let id = session.next_id;
        session.next_id += 1;
        match session.exchange(id, &hello, startup, None) {
            Ok(ReplyBody::Pong { version, .. }) if version == PROTOCOL_VERSION => Ok(session),
            Ok(ReplyBody::Pong { version, pid }) => {
                let reason = format!(
                    "the desktop worker speaks protocol version {version} and this build speaks \
                     {PROTOCOL_VERSION}; refusing rather than guessing at the older binary (worker pid {pid})"
                );
                Err(session.stop(reason))
            }
            Ok(ReplyBody::Refused { reason }) => {
                Err(session.stop(format!("the worker refused its handshake: {reason}")))
            }
            Ok(ReplyBody::Failure(wire)) => Err(wire.to_windows_error()),
            Ok(other) => Err(session.stop(format!(
                "the worker answered the handshake with a {} instead of a pong",
                other.tag()
            ))),
            Err(stop) => {
                let reason = format!(
                    "the worker did not complete its handshake within {}s: {}",
                    startup.as_secs().max(1),
                    stop.reason()
                );
                Err(session.stop(reason))
            }
        }
    }

    /// The directory this session's frames go in, as the string the worker is given.
    pub fn frame_dir(&self) -> String {
        self.frames.path()
    }

    /// Ask one thing, and give up on the answer after `budget`.
    pub fn ask(
        &mut self,
        body: &Body,
        budget: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<ReplyBody, Stop> {
        if self.dead {
            return Err(Stop::Lost {
                reason: "this worker was already terminated".to_owned(),
            });
        }
        let id = self.next_id;
        self.next_id += 1;
        self.exchange(id, body, budget, cancel)
    }

    /// Write one request and wait for the reply that answers it, with no session assumptions about order.
    fn exchange(
        &mut self,
        id: u64,
        body: &Body,
        budget: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<ReplyBody, Stop> {
        let line = match encode(id, body) {
            Ok(line) => line,
            Err(error) => return Err(self.halt(error.to_string())),
        };
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(Stop::Lost {
                reason: "this session's stdin is already closed".to_owned(),
            });
        };
        if let Err(error) = write_line(stdin, &line) {
            return Err(self.halt(format!(
                "the request could not be written to the worker: {error}"
            )));
        }
        match wait_for(&self.replies, id, budget, cancel) {
            Ok(reply) => Ok(reply),
            Err(stop) => {
                // The timeout and the cancel both mean the worker is still inside somebody else's
                // provider, and the only action that is actually available is to end the process.
                self.kill();
                Err(stop)
            }
        }
    }

    /// Terminate the worker and its tree, and stop counting it once its exit is observed.
    fn kill(&mut self) {
        self.dead = true;
        let _ = self.control.terminate();
        self.reap(SHUTDOWN_BOUND);
    }

    /// The error form of a failure that ends the session, after the worker has been reaped.
    ///
    /// Reserved for the handshake, where there is not yet a session to report a [`Stop`] through.
    fn stop(&mut self, reason: String) -> WindowsError {
        self.kill();
        WindowsError::AccessibilityUnavailable { reason }
    }

    /// The stop form of a conversation that could not be had, after the worker has been reaped.
    ///
    /// Reported as [`Stop::Lost`] rather than [`Stop::Unavailable`] even when the request never left: a
    /// half-written line is a thing a worker could have acted on, and the conservative answer is the one
    /// that keeps the no-replay rule (D-050) from depending on this function knowing how much got written.
    fn halt(&mut self, reason: String) -> Stop {
        self.kill();
        Stop::Lost { reason }
    }

    /// Close both write ends this process holds, let the worker exit by itself, then fall back to the
    /// job object.
    ///
    /// Politeness first is not ceremony: a worker that exits cleanly has finished writing its reply, and
    /// one terminated mid-write is the difference between a frame and a half-frame on disk.
    ///
    /// The order of the two closes is the part that was got wrong first, and the measurement caught it:
    /// `close_stdin` hands back jean-process's handle, but this session holds a *duplicate* write end, and
    /// a pipe reports EOF only when every write end is closed. Closing the supervisor's and then waiting
    /// left the worker reading a pipe this process still kept open, so every healthy call paid the whole
    /// [`SHUTDOWN_BOUND`] before the job object ended a worker that had nothing left to do.
    fn shutdown(&mut self) {
        if self.dead {
            return;
        }
        let _ = self.stdin.take();
        let _ = self.child.close_stdin();
        if self.reap(SHUTDOWN_BOUND) {
            return;
        }
        self.kill();
    }

    /// Wait for the exit to become observable, and come off the live count exactly once when it does.
    fn reap(&mut self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            if !self.control.is_running() {
                if !self.reaped {
                    self.reaped = true;
                    LIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
                }
                self.dead = true;
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL_SLICE);
        }
    }

    /// The stderr the worker wrote, for a failure message.
    pub fn stderr_tail(&self) -> String {
        let guard = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&guard).trim().to_owned()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
        // Both pumps block only on pipes the child closes when it dies, so joining them here has no
        // deadline of its own — that is the claim of this whole module: after the job object closes,
        // there is nothing left parked. If a pump somehow does not return, the worker is not dead, and it
        // stays counted in `live_workers` so the budget refuses the next call instead of joining a pile.
        if let Some(pump) = self.stdout_pump.take() {
            let _ = pump.join();
        }
        if let Some(pump) = self.stderr_pump.take() {
            let _ = pump.join();
        }
        if !self.reaped {
            // The exit was never observed. Terminate once more and leave the count where it is, so a
            // wedged worker shows up as budget pressure rather than as a clean slate.
            let _ = self.control.terminate();
        }
    }
}

/// Kill a worker that never became a session, and come off the live count if its death is observable.
fn abort_worker(control: &WorkerControl) {
    let _ = control.terminate();
    if exit_is_coming(control, SHUTDOWN_BOUND) {
        LIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Poll for an exit that is expected, without ever blocking the caller past `bound`.
fn exit_is_coming(control: &WorkerControl, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    loop {
        if !control.is_running() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_SLICE);
    }
}

/// The startup bound for a worker, derived from the operation's own bound.
///
/// A worker that takes longer than a fifth of the budget to complete its handshake has already spent too
/// much of it, and a floor of one second keeps a merely busy machine from being read as a broken install.
fn startup_bound(budget: Duration) -> Duration {
    (budget / 5).clamp(Duration::from_secs(1), Duration::from_secs(15))
}

/// Find the worker binary.
///
/// Overridden by [`WORKER_PATH_ENV`], then cargo's own bin variable, then the sibling of the running
/// executable, then the build directory one level above that. The last two shapes are what exist in
/// practice: an installed pair in one directory, and cargo, which puts `jean.exe` and
/// `jean-desktop-worker.exe` in `target\debug` but runs this crate's unit tests from `target\debug\deps`.
pub fn worker_program() -> WindowsResult<PathBuf> {
    locate_worker(
        std::env::var(WORKER_PATH_ENV).ok().as_deref(),
        std::env::var(CARGO_BIN_ENV).ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
        &Path::is_file,
    )
}

/// The lookup itself, with every input passed in.
///
/// Split out because the interesting cases — an override that names nothing, an executable that cannot be
/// located, a sibling that is not there — are all worth testing, and a test that mutates the process
/// environment would race every other test in the binary.
fn locate_worker(
    override_path: Option<&str>,
    cargo_bin: Option<&str>,
    current_exe: Option<&Path>,
    is_file: &dyn Fn(&Path) -> bool,
) -> WindowsResult<PathBuf> {
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    if let Some(raw) = override_path {
        candidates.push((format!("${WORKER_PATH_ENV}"), PathBuf::from(raw)));
    }
    if let Some(raw) = cargo_bin {
        candidates.push((format!("${CARGO_BIN_ENV}"), PathBuf::from(raw)));
    }
    if let Some(dir) = current_exe.and_then(Path::parent) {
        candidates.push(("beside this executable".to_owned(), dir.join(WORKER_BINARY)));
        if let Some(above) = dir.parent() {
            candidates.push((
                "the build directory above this executable".to_owned(),
                above.join(WORKER_BINARY),
            ));
        }
    }
    let mut tried = Vec::new();
    for (where_, path) in &candidates {
        if is_file(path) {
            return Ok(path.clone());
        }
        tried.push(format!("{} [{where_}]", path.display()));
    }
    Err(missing_worker(&tried))
}

fn missing_worker(tried: &[String]) -> WindowsError {
    WindowsError::AccessibilityUnavailable {
        reason: format!(
            "the desktop worker was not found ({}); desktop inspection and capture are refused rather \
             than run in this process, because a thread parked in another application's window procedure \
             is the failure this design exists to avoid",
            if tried.is_empty() {
                WORKER_BINARY.to_owned()
            } else {
                tried.join(", ")
            }
        ),
    }
}

/// The environment the worker is given.
///
/// Minimal, for the same reason `jean-browser` builds a minimal environment for the browser: the child
/// runs our code and needs the system to load `UIAutomationCore`, not the operator's shell. Anything else
/// in the parent's environment would be a thing the worker had to be reasoned about.
pub fn worker_environment(exe: &Path) -> BTreeMap<String, String> {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:\\Windows"));
    let system = Path::new(&root).join("System32");
    let mut env = BTreeMap::new();
    env.insert("SystemRoot".to_owned(), root.clone());
    env.insert(
        "PATH".to_owned(),
        match exe.parent() {
            Some(dir) => format!("{};{}", system.display(), dir.display()),
            None => system.display().to_string(),
        },
    );
    // The frame directory is under TEMP, and the worker writes there.
    env.insert(
        "TEMP".to_owned(),
        std::env::var("TEMP").unwrap_or(root.clone()),
    );
    env.insert(
        "TMP".to_owned(),
        std::env::var("TMP").unwrap_or(root.clone()),
    );
    env
}

fn encode(id: u64, body: &Body) -> WindowsResult<String> {
    let line = serde_json::to_string(&Request {
        id,
        body: body.clone(),
    })
    .map_err(|error| WindowsError::AccessibilityUnavailable {
        reason: format!("the worker request could not be built: {error}"),
    })?;
    if line.len() > MAX_REQUEST_BYTES {
        return Err(WindowsError::InvalidSetting {
            reason: format!(
                "the worker request is {} bytes, over the {MAX_REQUEST_BYTES} byte frame limit",
                line.len()
            ),
        });
    }
    Ok(line)
}

/// Encode one request for the worker's own reader, so the child and the parent share the frame rules.
pub fn encode_request(request: &Request) -> WindowsResult<String> {
    let line =
        serde_json::to_string(request).map_err(|error| WindowsError::AccessibilityUnavailable {
            reason: format!("the worker request could not be read: {error}"),
        })?;
    if line.len() > MAX_REQUEST_BYTES {
        return Err(WindowsError::InvalidSetting {
            reason: format!(
                "a worker request of {} bytes is over the {MAX_REQUEST_BYTES} byte frame limit",
                line.len()
            ),
        });
    }
    Ok(line)
}

/// Decode one request line inside the worker.
pub fn decode_request(line: &str) -> Result<Request, String> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(format!(
            "a request line of {} bytes is over the {MAX_REQUEST_BYTES} byte frame limit",
            line.len()
        ));
    }
    serde_json::from_str::<Request>(line).map_err(|error| {
        format!(
            "not a worker request ({}): {error}",
            line.chars().take(96).collect::<String>()
        )
    })
}

/// Encode one reply line, from inside the worker.
pub fn encode_reply(reply: &Reply) -> WindowsResult<String> {
    let line =
        serde_json::to_string(reply).map_err(|error| WindowsError::AccessibilityUnavailable {
            reason: format!("the worker reply could not be written: {error}"),
        })?;
    if line.len() > MAX_REPLY_BYTES {
        return Err(WindowsError::AccessibilityUnavailable {
            reason: format!(
                "the worker reply is {} bytes, over the {MAX_REPLY_BYTES} byte frame limit",
                line.len()
            ),
        });
    }
    Ok(line)
}

fn write_line(file: &mut std::fs::File, line: &str) -> std::io::Result<()> {
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()
}

/// Read replies until the one that answers `id` arrives, honouring the deadline and the cancel flag.
fn wait_for(
    replies: &Receiver<String>,
    id: u64,
    budget: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<ReplyBody, Stop> {
    let started = Instant::now();
    loop {
        match replies.recv_timeout(POLL_SLICE) {
            Ok(line) => match decode(&line) {
                Ok(reply) if reply.id == id => return Ok(reply.body),
                // An answer to a request this session no longer expects. Discarded, not fatal.
                Ok(_) => continue,
                Err(reason) => return Err(Stop::Lost { reason }),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Stop::Lost {
                    reason: "the worker closed its reply stream".to_owned(),
                });
            }
        }
        if let Some(flag) = cancel {
            if flag.load(Ordering::SeqCst) {
                return Err(Stop::Cancelled {
                    waited: started.elapsed(),
                });
            }
        }
        let waited = started.elapsed();
        if waited >= budget {
            return Err(Stop::TimedOut { waited });
        }
    }
}

fn decode(line: &str) -> Result<Reply, String> {
    if line.len() > MAX_REPLY_BYTES {
        return Err(format!(
            "the worker wrote a {} byte reply, over the {MAX_REPLY_BYTES} byte limit",
            line.len()
        ));
    }
    serde_json::from_str::<Reply>(line).map_err(|error| {
        format!(
            "the worker wrote a line that is not a reply ({}): {error}",
            line.chars().take(96).collect::<String>()
        )
    })
}

/// Read stdout until it closes, forwarding whole lines.
///
/// The pump exists because a blocking read with no timeout is the call this module is trying not to make:
/// the *parent* waits against a deadline and pulls the trigger, and this thread only ever blocks on a pipe
/// that closes when the worker dies.
fn pump_stdout(mut stdout: std::fs::File, tx: mpsc::Sender<String>) {
    // The limit is per line in the protocol and per session here, because `take` bounds the stream.
    // Four replies' worth is more than one legitimate session, and an overlong read ends the session at
    // the decode step with its length in the message.
    let capped = (&mut stdout).take((MAX_REPLY_BYTES * 4) as u64);
    let mut reader = BufReader::new(capped);
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                let text = String::from_utf8_lossy(&line).into_owned();
                if tx.send(text).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Read stderr until it closes, retaining the last [`MAX_STDERR_TAIL`] bytes.
fn pump_stderr(mut stderr: std::fs::File, tail: Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0u8; 4096];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let mut guard = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.extend_from_slice(&buffer[..read]);
                let drop_bytes = guard.len().saturating_sub(MAX_STDERR_TAIL);
                if drop_bytes > 0 {
                    guard.drain(..drop_bytes);
                }
            }
        }
    }
}

/// Ask one thing of a fresh worker and reap it: the shape almost every call site wants.
///
/// The worker exists for the duration of the conversation and for no longer, which is what makes "no
/// accumulated abandoned anything" a property of the design rather than a hope about provider behaviour.
pub fn one(body: &Body, budget: Duration, cancel: Option<&AtomicBool>) -> Result<ReplyBody, Stop> {
    let mut session = match Session::start(budget) {
        Ok(session) => session,
        Err(error) => {
            return Err(Stop::Unavailable {
                reason: error.to_string(),
            })
        }
    };
    // A capture needs somewhere to put pixels, and the directory is the parent's, not the worker's.
    let owned = match body {
        Body::Capture {
            window,
            max_pixels,
            backend,
            ..
        } => Body::Capture {
            window: *window,
            max_pixels: *max_pixels,
            backend: *backend,
            frame_dir: Some(session.frame_dir()),
        },
        other => other.clone(),
    };
    let answer = session.ask(&owned, budget, cancel);
    if matches!(answer, Err(Stop::Lost { .. })) {
        tracing::debug!(
            tail = session.stderr_tail(),
            "desktop worker ended without a reply"
        );
    }
    // The pixels have to be taken before the session ends, because the session owns the directory they
    // are in. A frame left in a deleted directory is a frame the parent has lost.
    let answer = match answer {
        Ok(ReplyBody::Frame(mut meta)) => {
            meta.load_pixels();
            Ok(ReplyBody::Frame(meta))
        }
        other => other,
    };
    drop(session);
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire is the only thing the two processes share, so its round trip is tested without a process.
    fn capture_body() -> Body {
        Body::Capture {
            window: 42,
            max_pixels: 1_000_000,
            backend: Backend::PrintWindow,
            frame_dir: Some("C:\\tmp\\frames".to_owned()),
        }
    }

    #[test]
    fn every_bound_the_worker_is_told_to_honour_arrives_intact() {
        // The bounds are the caller's, so a field the enum grows without the request carrying it is a
        // bound silently dropped on the floor. This covers the three bodies with a pattern probe and the
        // two with a node cap; `Resolve` is the one that used to hard-code `false` and empty a combobox's
        // pattern list in a native test.
        let cases = vec![
            Body::ReadTree {
                window: 1,
                max_nodes: 11,
                probe_patterns: true,
            },
            Body::Resolve {
                window: 2,
                locator: Locator::AutomationId("1005".to_owned()),
                max_nodes: 12,
                probe_patterns: true,
            },
            Body::ReadState {
                window: 3,
                locator: Locator::Name {
                    name: "Total".to_owned(),
                    control_type: None,
                },
                probe_patterns: true,
                max_nodes: 13,
            },
            Body::Act {
                window: 4,
                locator: Locator::AutomationId("1002".to_owned()),
                action: Action::Invoke,
                witness: None,
                probe_patterns: true,
                max_nodes: 14,
            },
            Body::Probe {
                window: 5,
                max_nodes: 15,
            },
        ];
        for body in cases {
            let request = decode_request(&encode(9, &body).expect("encodable")).expect("readable");
            assert_eq!(
                request.body, body,
                "a bound must reach the worker unchanged"
            );
        }
    }

    #[test]
    fn a_capture_request_survives_the_line_with_its_directory() {
        // The frame directory is the one part of a capture that the parent decides and the worker has to
        // honour, so it travels in the request rather than being guessed from TEMP on the other side.
        let line = encode(3, &capture_body()).expect("encodable");
        let request = decode_request(&line).expect("readable by the worker");
        assert_eq!(request.body, capture_body());
        match request.body {
            Body::Capture {
                window,
                max_pixels,
                backend,
                frame_dir,
            } => {
                assert_eq!((window, max_pixels), (42, 1_000_000));
                assert_eq!(backend, Backend::PrintWindow);
                assert_eq!(frame_dir.as_deref(), Some("C:\\tmp\\frames"));
            }
            other => panic!("a capture must arrive as a capture: {other:?}"),
        }
    }

    #[test]
    fn a_request_and_a_reply_survive_the_line_as_the_same_things() {
        let body = Body::Act {
            window: -1,
            locator: Locator::Path(vec![
                Locator::Name {
                    name: "Group".to_owned(),
                    control_type: None,
                },
                Locator::Index(2),
            ]),
            action: Action::SetValue {
                value: "typed by jean".to_owned(),
            },
            witness: Some(Locator::AutomationId("1006".to_owned())),
            probe_patterns: true,
            max_nodes: 12,
        };
        let line = encode(7, &body).expect("encodable");
        let request = decode_request(&line).expect("readable by the worker");
        assert_eq!(request.id, 7);
        assert_eq!(
            request.body, body,
            "the worker must be asked the same thing"
        );

        let reply = Reply {
            id: 7,
            body: ReplyBody::Pong {
                version: PROTOCOL_VERSION,
                pid: 99,
            },
        };
        let line = encode_reply(&reply).expect("encodable");
        let decoded = decode(&line).expect("readable");
        assert_eq!(
            decoded.id, 7,
            "the reply must answer the request it arrived for"
        );
        assert_eq!(
            decoded.body.tag(),
            "pong",
            "a handshake answer must not arrive as anything else"
        );
        match decoded.body {
            ReplyBody::Pong { version, pid } => {
                assert_eq!(
                    (version, pid),
                    (PROTOCOL_VERSION, 99),
                    "both fields survive"
                );
            }
            other => panic!("expected a pong, got {}", other.tag()),
        }
    }

    #[test]
    fn every_failure_the_worker_can_report_comes_back_as_the_same_variant() {
        let cases = vec![
            WindowsError::StaleWindow { hwnd: 7 },
            WindowsError::NoSuchWindow {
                wanted: "pid 14640".to_owned(),
            },
            WindowsError::AccessibilityUnavailable {
                reason: "no COM".to_owned(),
            },
            WindowsError::ProviderTimeout {
                hwnd: 7,
                budget_secs: 5,
                waited_ms: 5_010,
            },
            WindowsError::Cancelled {
                budget_secs: 5,
                waited_ms: 1_000,
            },
            WindowsError::Panicked {
                call: "uia-act".to_owned(),
            },
            WindowsError::Ambiguous {
                locator: "name=Continue".to_owned(),
                count: 2,
            },
            WindowsError::PatternUnsupported {
                control: "id=1001".to_owned(),
                pattern: "Invoke".to_owned(),
            },
            WindowsError::StaleElement {
                control: "id=1001".to_owned(),
            },
            WindowsError::FocusChanged {
                expected: "hwnd=1".to_owned(),
                actual: "hwnd=2".to_owned(),
            },
            WindowsError::IntegrityBlocked {
                hwnd: 7,
                reason: "High".to_owned(),
            },
            WindowsError::SessionUnavailable {
                reason: "locked".to_owned(),
            },
            WindowsError::CaptureUnusable {
                hwnd: 7,
                reason: "minimised".to_owned(),
            },
            WindowsError::File {
                path: PathBuf::from("C:\\tmp\\frame.bmp"),
                reason: "denied".to_owned(),
            },
            WindowsError::InvalidSetting {
                reason: "zero timeout".to_owned(),
            },
            WindowsError::Win32 {
                call: "PrintWindow".to_owned(),
                reason: "5".to_owned(),
            },
        ];
        for original in cases {
            let line = serde_json::to_string(&ReplyBody::Failure(WireError::from(&original)))
                .expect("serialisable");
            let reply: ReplyBody = serde_json::from_str(&line).expect("deserialisable");
            let crossed = match reply {
                ReplyBody::Failure(wire) => wire.to_windows_error(),
                other => panic!("a failure must arrive as a failure, got {}", other.tag()),
            };
            assert_eq!(
                crossed.to_string(),
                original.to_string(),
                "{original:?} must mean the same thing on both sides of the pipe"
            );
        }
    }

    #[test]
    fn an_unknown_worker_failure_is_reported_as_unknown_rather_than_guessed() {
        let line = r#"{"failure":{"unknown":{"kind":"gpu_blocked","message":"the frame is not a frame"}}}"#;
        let reply: ReplyBody = serde_json::from_str(line).expect("a wire variant the parent lacks");
        let error = match reply {
            ReplyBody::Failure(wire) => wire.to_windows_error(),
            other => panic!("expected a failure, got {}", other.tag()),
        };
        let text = error.to_string();
        assert!(
            text.contains("gpu_blocked") && text.contains("cannot name"),
            "drift has to be visible in the message: {text}"
        );
    }

    #[test]
    fn only_a_worker_that_never_ran_is_a_side_effect_free_stop() {
        assert!(
            Stop::TimedOut {
                waited: Duration::from_millis(1)
            }
            .may_have_acted(),
            "a killed worker may already have delivered its call"
        );
        assert!(Stop::Cancelled {
            waited: Duration::ZERO
        }
        .may_have_acted());
        assert!(Stop::Lost {
            reason: "closed".to_owned()
        }
        .may_have_acted());
        assert!(
            !Stop::Unavailable {
                reason: "not found".to_owned()
            }
            .may_have_acted(),
            "nothing ran, so nothing landed"
        );
    }

    #[test]
    fn an_overlong_reply_is_refused_before_it_is_parsed() {
        let line = "x".repeat(MAX_REPLY_BYTES + 1);
        let reason = decode(&line).expect_err("over the frame limit");
        assert!(reason.contains("over the"), "{reason}");
    }

    #[test]
    fn a_request_larger_than_the_frame_limit_is_never_written() {
        let huge = "a".repeat(MAX_REQUEST_BYTES);
        let body = Body::Act {
            window: 1,
            locator: Locator::Name {
                name: huge,
                control_type: None,
            },
            action: Action::Invoke,
            witness: None,
            probe_patterns: false,
            max_nodes: 1,
        };
        let error = encode(1, &body).expect_err("over the request limit");
        assert!(
            matches!(error, WindowsError::InvalidSetting { .. }),
            "a bound refused is a setting problem, not a desktop problem: {error}"
        );
    }

    #[test]
    fn a_frame_without_its_file_is_refused_instead_of_invented() {
        let meta = FrameMeta {
            window: 3,
            width: 2,
            height: 2,
            hash: 0,
            mean_luma: 128,
            uniform: false,
            uniform_argb: None,
            render_dpi: Some(96),
            captured_at_ms: 1,
            backend: Backend::PrintWindow,
            file: None,
            pixels: None,
        };
        assert_eq!(meta.quality(), FrameQuality::Usable);
        let error = meta.into_capture().expect_err("no pixels to read");
        assert!(
            matches!(error, WindowsError::CaptureUnusable { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_half_written_frame_is_not_reported_as_a_capture() {
        let dir = std::env::temp_dir().join(format!(
            "jean-worker-test-{}",
            FRAME_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("a test directory");
        let file = dir.join("frame-1.bgra");
        // 2x2 at 4 bytes per pixel is 16; four of them are written.
        std::fs::write(&file, [0u8; 4]).expect("a short frame");
        let error = crate::worker::FrameMeta {
            window: 5,
            width: 2,
            height: 2,
            hash: 0,
            mean_luma: 0,
            uniform: false,
            uniform_argb: None,
            render_dpi: None,
            captured_at_ms: 0,
            backend: Backend::ScreenBitBlt,
            file: Some(file.to_string_lossy().into_owned()),
            pixels: None,
        }
        .into_capture()
        .expect_err("a short frame is not a frame");
        assert!(error.to_string().contains("incomplete"), "{error}");

        // And a frame longer than its metadata is named as that, rather than as the shortage.
        std::fs::write(&file, [0u8; 20]).expect("an overlong frame");
        let overlong = crate::worker::FrameMeta {
            file: Some(file.to_string_lossy().into_owned()),
            ..sample_meta()
        }
        .into_capture()
        .expect_err("20 bytes is not 16");
        assert!(
            overlong.to_string().contains("longer than the 16 bytes"),
            "{overlong}"
        );
        std::fs::remove_dir_all(&dir).expect("the test cleans up its own directory");
    }

    #[test]
    fn the_pixels_are_taken_before_the_session_that_owns_them_ends() {
        // The frame directory is deleted by `Session::drop`, so a metadata-only answer would be a frame
        // the parent lost on the way out. `load_pixels` is the step that has to happen first.
        let dir = std::env::temp_dir().join(format!(
            "jean-worker-test-{}",
            FRAME_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("a test directory");
        let file = dir.join("frame-2.bgra");
        std::fs::write(&file, [7u8; 16]).expect("a whole frame");
        let mut meta = crate::worker::FrameMeta {
            file: Some(file.to_string_lossy().into_owned()),
            ..sample_meta()
        };
        meta.load_pixels();
        assert_eq!(meta.pixels.as_deref(), Some([7u8; 16].as_slice()));

        // The worker is gone, its directory with it, and the frame is still deliverable.
        std::fs::remove_dir_all(&dir).expect("the session cleanup this stands in for");
        let capture = meta.into_capture().expect("a frame already in hand");
        assert_eq!(capture.pixels, vec![7u8; 16]);
        assert_eq!(capture.width * capture.height, 4);
    }

    #[test]
    fn a_frame_file_that_is_not_there_is_reported_as_the_read_that_failed() {
        let mut meta = crate::worker::FrameMeta {
            file: Some(
                std::env::temp_dir()
                    .join("jean-worker-absent-frame.bgra")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..sample_meta()
        };
        meta.load_pixels();
        assert!(
            meta.pixels.is_none(),
            "a missing file is not a zero-byte frame"
        );
        let error = meta.into_capture().expect_err("no pixels anywhere");
        assert!(error.to_string().contains("read failed"), "{error}");
    }

    fn sample_meta() -> crate::worker::FrameMeta {
        crate::worker::FrameMeta {
            window: 5,
            width: 2,
            height: 2,
            hash: 0,
            mean_luma: 0,
            uniform: false,
            uniform_argb: None,
            render_dpi: None,
            captured_at_ms: 0,
            backend: Backend::ScreenBitBlt,
            file: None,
            pixels: None,
        }
    }

    #[test]
    fn the_startup_bound_never_borrows_the_whole_operation() {
        assert_eq!(
            startup_bound(Duration::from_secs(5)),
            Duration::from_secs(1)
        );
        assert_eq!(
            startup_bound(Duration::from_secs(120)),
            Duration::from_secs(15)
        );
        assert_eq!(
            startup_bound(Duration::from_millis(10)),
            Duration::from_secs(1),
            "a floor, so a busy machine is not read as a broken install"
        );
    }

    #[test]
    fn a_worker_that_is_not_there_refuses_rather_than_running_in_process() {
        // Nothing in this tree may start a provider call on the caller's thread, so the absence of the
        // binary has to be an error that names where it looked.
        let absent = |_: &Path| false;
        let error = locate_worker(
            Some("C:\\definitely-not-here\\jean-desktop-worker.exe"),
            None,
            Some(Path::new("C:\\tools\\bin\\jean.exe")),
            &absent,
        )
        .expect_err("no worker anywhere");
        let text = error.to_string();
        assert!(
            text.contains("definitely-not-here") && text.contains("tools"),
            "both locations have to be named, or the operator cannot fix either: {text}"
        );
        assert!(text.contains("refused"), "{text}");
    }

    #[test]
    fn the_override_wins_and_the_build_directory_is_the_last_guess() {
        let present = |_: &Path| true;
        let chosen = locate_worker(
            Some("D:\\custom\\worker.exe"),
            Some("C:\\ignored\\worker.exe"),
            Some(Path::new("C:\\tools\\jean.exe")),
            &present,
        )
        .expect("an override that exists");
        assert_eq!(chosen, PathBuf::from("D:\\custom\\worker.exe"));

        // The order the four shapes are tried in is the contract the test suites and the installed pair
        // both depend on.
        let seen = std::cell::RefCell::new(Vec::new());
        let record = |path: &Path| {
            seen.borrow_mut()
                .push(path.to_string_lossy().replace('\\', "/"));
            false
        };
        let error = locate_worker(
            None,
            Some("C:\\cargo\\bin\\jean-desktop-worker.exe"),
            Some(Path::new("C:\\target\\debug\\deps\\jean_windows-1.exe")),
            &record,
        )
        .expect_err("nothing exists in this test");
        assert_eq!(
            *seen.borrow(),
            vec![
                "C:/cargo/bin/jean-desktop-worker.exe",
                "C:/target/debug/deps/jean-desktop-worker.exe",
                "C:/target/debug/jean-desktop-worker.exe",
            ],
            "cargo's bin variable, then the sibling, then the build directory above it"
        );
        assert!(error.to_string().contains("cargo"), "{error}");

        let sibling = locate_worker(None, None, Some(Path::new("C:\\tools\\jean.exe")), &present)
            .expect("a worker next to the executable");
        assert_eq!(sibling, PathBuf::from("C:\\tools\\jean-desktop-worker.exe"));
        // The shape the crate's own unit tests are in: cargo puts the worker in `target\debug` and runs
        // them from `target\debug\deps`, so the sibling is genuinely absent and resolution has to step one
        // level up — in that order, which is what the fake filesystem below is for.
        let only_above = |path: &Path| {
            path.to_string_lossy()
                .ends_with(r"debug\jean-desktop-worker.exe")
        };
        let from_deps = locate_worker(
            None,
            None,
            Some(Path::new(r"C:\target\debug\deps\jean_windows-1.exe")),
            &only_above,
        )
        .expect("a worker in the build directory above the test binary");
        assert_eq!(
            from_deps,
            PathBuf::from(r"C:\target\debug\jean-desktop-worker.exe")
        );
        let error = locate_worker(None, None, None, &present).expect_err("an unknown own path");
        assert!(
            matches!(error, WindowsError::AccessibilityUnavailable { .. }),
            "a parent that cannot find itself must not guess: {error}"
        );
    }
}
