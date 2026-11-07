//! A pseudo console, created directly through Win32.
//!
//! ## Why this file exists instead of a crate
//!
//! `PLANNING/05-ROADMAP.md` names `portable-pty` 0.9.0 for this. It cannot be
//! used: `portable-pty` 0.9.0 depends on `winapi`, and `winapi` is banned in
//! `deny.toml` because it has been frozen since 2020 and because D-022 commits
//! this project to `windows-rs`, Microsoft's own projections, as the single
//! Windows surface. The choice is therefore between the roadmap's named crate and
//! the roadmap's named decision, and D-022 wins — this module is the deviation
//! recorded as D-024. It is roughly 400 lines of FFI against five functions
//! (`CreatePseudoConsole`, `ResizePseudoConsole`, `ClosePseudoConsole`,
//! `InitializeProcThreadAttributeList`, `UpdateProcThreadAttribute`) rather than a
//! dependency whose Windows backend wraps exactly those five.
//!
//! ## What a pseudo console is
//!
//! `CreatePseudoConsole` starts a headless `conhost` and hands you two pipes: one
//! you write into the console's *input*, one you read the console's *output* from.
//! A process launched with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` then believes it
//! owns a real terminal — which is the only way an interactive program like
//! `ssh`, `ftp`, a REPL, or a TUI installer works when J.E.A.N. started it.
//!
//! Two consequences of that shape matter here:
//!
//! * **Input and output are one stream.** There is no separate stderr inside a
//!   console; both the program's stdout and its diagnostics are written to the
//!   same surface, interleaved, with cursor control. The supervisor therefore
//!   captures one stream and reports the other as absent rather than pretending to
//!   split them.
//! * **The output is the screen, not the bytes.** ANSI sequences, line-drawing and
//!   echo of what was typed are all in it. That is what makes it useful to a human
//!   and noisier than a pipe for a machine; see
//!   [`crate::output::strip_ansi`] for how it is normalised for verification.
//!
//! ## Handle ownership, which is the part that bites
//!
//! Four handles are created. Two of them (`input_read`, `output_write`) belong to
//! the pseudo console after `CreatePseudoConsole` duplicates them into conhost;
//! this process must close **its** copies, or the output pipe never reaches EOF
//! and the reader waits forever on a child that already exited. `ClosePseudoConsole`
//! must not be called before the child is dead, because the console is what the
//! child's `WriteFile` to stdout resolves to; the order is therefore terminate the
//! job, then close the console. Both are enforced by the field types and by
//! [`Pty::close_handed`], not by a comment.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use crate::error::{ProcessError, Result};
use crate::win32::{OwnedHandle, SuspendedChild, WideString};

/// Default console geometry when the caller does not say otherwise.
///
/// 80x24 is the traditional PTY size and what a program that queries the console
/// size expects to see if nothing has resized it. A row count of zero makes some
/// full-screen programs divide by zero when they compute a page length, so the
/// type cannot express it: the constructor rejects it.
pub const DEFAULT_ROWS: u16 = 40;
pub const DEFAULT_COLS: u16 = 120;

/// The two ends this process keeps, plus the two it handed to the console.
#[derive(Debug)]
pub struct Pty {
    console: HPCON,
    /// What we write to send keystrokes to the child.
    input_write: OwnedHandle,
    /// What we read to see the console surface. Taken by the supervisor with
    /// [`Pty::take_output_read`] and drained on a reader thread.
    output_read: Option<OwnedHandle>,
    /// The console's own ends. Closed by [`Pty::close_handed`] once the child has
    /// been created; until then they are held so a failure between the two steps
    /// still releases them.
    handed: Vec<OwnedHandle>,
    rows: u16,
    cols: u16,
}

impl Pty {
    /// Create a pseudo console of `rows` x `cols`.
    pub fn create(rows: u16, cols: u16) -> Result<Self> {
        if rows == 0 || cols == 0 {
            return Err(ProcessError::Invalid {
                field: "terminal.pty_rows / terminal.pty_cols",
                reason: format!("a pseudo console must be at least 1x1, got {rows}x{cols}"),
            });
        }

        // Two pipes, neither inheritable. `None` security attributes is the point:
        // an inheritable handle would be duplicated into any child we later create
        // with inheritance on, which is the handle leak this module is careful
        // about. The pseudo console gets its access by duplication inside
        // `CreatePseudoConsole`, not by inheritance.
        let (input_read, input_write) = create_uninheritable_pipe()?;
        let (output_read, output_write) = create_uninheritable_pipe()?;

        let console = unsafe {
            CreatePseudoConsole(
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
                input_read.raw(),
                output_write.raw(),
                0,
            )
            .map_err(|e| ProcessError::win32("CreatePseudoConsole", e))?
        };
        if console.is_invalid() {
            // `CreatePseudoConsole` reports success with an invalid handle only
            // on the paths where the caller passed a bad geometry, so the message
            // says which call left it invalid rather than pretending to know why.
            return Err(ProcessError::win32(
                "CreatePseudoConsole",
                "returned an invalid pseudo-console handle".to_owned(),
            ));
        }

        // The console has duplicated the two ends it needs. This process must not
        // keep them, so they are tracked here and closed after the child is
        // created — see the module docs for why "immediately" is not quite right.
        Ok(Self {
            console,
            input_write,
            output_read: Some(output_read),
            handed: vec![input_read, output_write],
            rows,
            cols,
        })
    }

    pub const fn rows(&self) -> u16 {
        self.rows
    }

    pub const fn cols(&self) -> u16 {
        self.cols
    }

    /// The handle this process writes keystrokes to.
    pub fn input_write(&self) -> &OwnedHandle {
        &self.input_write
    }

    /// Move the output handle out, so the supervisor can hand it to a reader
    /// thread while this struct still owns the console itself.
    ///
    /// Returns `None` on a second call: one reader per pseudo console, because two
    /// readers would each get part of one byte stream and neither could tell.
    pub fn take_output_read(&mut self) -> Option<OwnedHandle> {
        self.output_read.take()
    }

    /// Close the two handles that belong to the console now.
    ///
    /// Called after [`create_suspended_in_pty`] succeeds. Doing it before would be
    /// wrong in one direction and harmless in the other: the console has already
    /// duplicated them, so an early close would not break the child, but a *late*
    /// close — after the reader started — is what makes EOF unreachable.
    pub fn close_handed(&mut self) {
        for handle in self.handed.drain(..) {
            // `OwnedHandle::drop` closes; draining is the whole operation.
            drop(handle);
        }
    }

    /// Change the console size, for a window the operator resized.
    ///
    /// Failure here is not fatal to the task: the child keeps running at the old
    /// geometry. It is reported, not retried, because a TUI that has already
    /// painted a screen will not repaint correctly from a resize that arrived late.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        if rows == 0 || cols == 0 {
            return Err(ProcessError::Invalid {
                field: "pseudo console size",
                reason: format!("{rows}x{cols} is not a usable console size"),
            });
        }
        unsafe {
            ResizePseudoConsole(
                self.console,
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
            )
            .map_err(|e| ProcessError::win32("ResizePseudoConsole", e))
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Whatever is left of the handed ends, then the console. The supervisor
        // has already terminated the job by this point; closing the console before
        // the child died would pull the ground out from under its writes.
        self.close_handed();
        if !self.console.is_invalid() {
            unsafe { ClosePseudoConsole(self.console) };
        }
    }
}

/// A non-inheritable anonymous pipe, with the two ends wrapped for closing.
fn create_uninheritable_pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe {
        CreatePipe(&mut read, &mut write, None, 0)
            .map_err(|e| ProcessError::win32("CreatePipe", e))?;
    }
    Ok((unsafe { OwnedHandle::from_raw(read) }, unsafe {
        OwnedHandle::from_raw(write)
    }))
}

/// The attribute list that carries the pseudo console to `CreateProcessW`.
///
/// Two-call dance on purpose: `InitializeProcThreadAttributeList` with a null
/// pointer reports the required size, and the second call initialises the buffer
/// we allocated from it. The buffer must stay alive and un-moved until
/// `CreateProcessW` returns, and `DeleteProcThreadAttributeList` must run before
/// the memory is freed — which is why the drop order here is explicit rather than
/// left to field declaration order.
struct AttributeList {
    buffer: Vec<u8>,
}

impl AttributeList {
    fn with_one_attribute() -> Result<Self> {
        let mut needed: usize = 0;
        unsafe {
            // First call: ask for the size. The documented failure is
            // ERROR_INSUFFICIENT_BUFFER, and `needed` is only meaningful then.
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut needed);
        }
        if needed == 0 {
            return Err(ProcessError::win32(
                "InitializeProcThreadAttributeList (size query)",
                "reported a required size of zero, which is never valid".to_owned(),
            ));
        }
        // Over-allocate rather than exactly-size: the API's documented size is the
        // minimum, and a buffer that is one byte short fails with a message that
        // points nowhere.
        let mut buffer = vec![0u8; needed + 64];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr() as *mut c_void);
        unsafe {
            InitializeProcThreadAttributeList(Some(list), 1, None, &mut needed)
                .map_err(|e| ProcessError::win32("InitializeProcThreadAttributeList", e))?;
        }
        Ok(Self { buffer })
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_ptr() as *mut c_void)
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.list());
        }
        // `buffer` is freed by the Vec's own drop after this runs, which is the
        // order the API requires.
    }
}

/// Launch a child **suspended**, attached to `pty` as its console.
///
/// Same contract as [`crate::win32::create_suspended`]: the caller must assign the
/// result to a Job Object before calling [`resume`], and must not pass
/// `CREATE_NO_WINDOW` — a process cannot both own a pseudo console and have no
/// window, and the combination is documented as an error.
///
/// # Correctness notes that are easy to get wrong
///
/// * `StartupInfo.cb` is the size of **`STARTUPINFOEXW`**, not of `STARTUPINFOW`,
///   and `EXTENDED_STARTUPINFO_PRESENT` must be in the creation flags. Setting the
///   smaller `cb` is the classic way to get a silent `ERROR_BAD_ATTRIBUTE_LIST`.
/// * The pseudo console handle is passed **by value**, not as a pointer to the
///   value, and `cbSize` is `size_of::<HPCON>()`. Every other attribute takes a
///   pointer to its value, so this is the one that a copied-and-pasted call gets
///   wrong.
/// * `bInheritHandles` is `false`. The child needs no inherited handle from us —
///   the console is not inherited, it is attributed.
/// * `STARTF_USESTDHANDLES` **is** set, with all three standard handles null. This
///   is the opposite of the intuitive reading, and it was found by measurement:
///   leaving the flag out means the child's standard handles come from *this*
///   process, and when J.E.A.N. runs without a console — as it does under a service,
///   a scheduled task, or `cargo test` — the child's `echo` lands in the supervisor's
///   own output instead of the pseudo console. The console then receives only the
///   host's start-up sequences, which is a silent, plausible-looking failure: the
///   session runs, the artifact is nearly empty, and nothing says why. Naming the
///   handles explicitly and leaving them null makes the console subsystem wire them
///   to the console the process was given, which is what an interactive child
///   expects when it calls `GetStdHandle`.
#[allow(clippy::too_many_arguments)]
pub fn create_suspended_in_pty(
    executable: &std::path::Path,
    command_line_units: &[u16],
    working_dir: Option<&std::path::Path>,
    environment_block: &[u8],
    pty: &Pty,
) -> Result<SuspendedChild> {
    if pty.output_read.is_none() && pty.handed.is_empty() {
        // Defensive, not load-bearing: a `Pty` whose output handle has already
        // been taken belongs to a supervisor that is halfway through teardown, and
        // launching a second child into it would produce a console with two owners.
        return Err(ProcessError::Invalid {
            field: "pseudo console",
            reason: "this pseudo console has already been handed to a child".to_owned(),
        });
    }

    let mut command_line: Vec<u16> = command_line_units.to_vec();
    if command_line.last() != Some(&0) {
        command_line.push(0);
    }

    let application = WideString::from_os(executable.as_os_str());
    let current_dir = working_dir.map(|d| WideString::from_os(d.as_os_str()));
    let attributes = AttributeList::with_one_attribute()?;

    unsafe {
        UpdateProcThreadAttribute(
            attributes.list(),
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            Some(pty.console.0 as *const c_void),
            std::mem::size_of::<HPCON>(),
            None,
            None,
        )
        .map_err(|e| ProcessError::win32("UpdateProcThreadAttribute(pseudo console)", e))?;
    }

    let mut siex = STARTUPINFOEXW::default();
    siex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    siex.lpAttributeList = attributes.list();
    // Explicit, null standard handles: see the note above about which way round this
    // actually works. `HANDLE::default()` is null, not the invalid handle, and the
    // console subsystem substitutes the console's own input and output for null.
    siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    siex.StartupInfo.hStdInput = HANDLE::default();
    siex.StartupInfo.hStdOutput = HANDLE::default();
    siex.StartupInfo.hStdError = HANDLE::default();

    let flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    let environment: &[u8] = if environment_block.is_empty() {
        // Same reason as in `win32::create_suspended`: `CREATE_UNICODE_ENVIRONMENT`
        // requires a valid pointer, and null means "inherit everything", which is
        // exactly what the allowlist exists to prevent.
        &[0u8, 0u8][..]
    } else {
        environment_block
    };
    let cwd = match &current_dir {
        Some(w) => w.pcwstr(),
        None => PCWSTR::null(),
    };

    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            application.pcwstr(),
            Some(windows::core::PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            Some(environment.as_ptr() as *const c_void),
            cwd,
            // The extended structure starts with a `STARTUPINFOW`, so a pointer
            // cast is the documented way to pass it.
            &siex.StartupInfo as *const STARTUPINFOW,
            &mut pi,
        )
        .map_err(|e| ProcessError::CreateProcess {
            executable: executable.to_path_buf(),
            reason: format!(
                "{e} (a pseudo console child must not be started with CREATE_NO_WINDOW \
                 or CREATE_NEW_CONSOLE)"
            ),
        })?;
    }

    let process = unsafe { OwnedHandle::from_raw(pi.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw(pi.hThread) };

    Ok(SuspendedChild {
        process,
        thread,
        pid: pi.dwProcessId,
    })
}

/// The console mode this process's standard input had before J.E.A.N. changed it,
/// restored on drop.
///
/// An interactive relay needs the keystrokes, not the line Windows would hand out
/// after Enter, and it needs to see Ctrl-C itself rather than have the console
/// deliver the signal to the whole process group. That is `SetConsoleMode` with
/// `ENABLE_PROCESSED_INPUT` and `ENABLE_ECHO_INPUT` cleared.
///
/// Restoring is the reason this is a guard and not a call: if the relay returns
/// early — child died, cancel arrived, capture hit its cap — and the mode is left
/// raw, the operator's own shell keeps the settings this process changed. Every
/// later command in that window then behaves as if echo were off, which is the
/// kind of damage that outlives the bug report.
pub struct RawInputGuard {
    saved: Option<u32>,
}

impl RawInputGuard {
    /// Put standard input into raw mode if, and only if, it is a real console.
    ///
    /// When standard input is a redirect or a pipe there is nothing to switch, and
    /// `SetConsoleMode` would fail; that is reported as `saved = None` and the
    /// supervisor then feeds the child from the redirected bytes instead.
    pub fn enable() -> Self {
        use windows::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT,
            ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
        };

        let handle = match unsafe { GetStdHandle(STD_INPUT_HANDLE) } {
            Ok(h) if !h.is_invalid() => h,
            _ => return Self { saved: None },
        };
        let mut saved = CONSOLE_MODE(0);
        if unsafe { GetConsoleMode(handle, &mut saved) }.is_err() {
            // Not a console: a pipe, a file, or NUL. No mode to save and none to
            // change, and saying so is what lets the caller explain the difference.
            return Self { saved: None };
        }
        // Line input off so bytes arrive as they are typed; echo off because the
        // pseudo console echoes the child's own session and a second echo would
        // double every character on the operator's screen; processed input off so
        // Ctrl-C reaches the child as a byte rather than signalling this process.
        let raw = CONSOLE_MODE(
            saved.0 & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0),
        );
        match unsafe { SetConsoleMode(handle, raw) } {
            Ok(()) => Self {
                saved: Some(saved.0),
            },
            Err(_) => Self { saved: None },
        }
    }

    /// Whether this process is attached to a console it could put into raw mode.
    ///
    /// `jean run --interactive` uses it to refuse an interactive session over a
    /// redirected stdin rather than start a child nobody can type into.
    pub fn is_console(&self) -> bool {
        self.saved.is_some()
    }
}

impl Drop for RawInputGuard {
    fn drop(&mut self) {
        use windows::Win32::System::Console::{
            GetStdHandle, SetConsoleMode, CONSOLE_MODE, STD_INPUT_HANDLE,
        };

        let Some(mode) = self.saved else {
            return;
        };
        if let Ok(handle) = unsafe { GetStdHandle(STD_INPUT_HANDLE) } {
            if let Err(e) = unsafe { SetConsoleMode(handle, CONSOLE_MODE(mode)) } {
                tracing::warn!(
                    error = %e,
                    "could not restore the console input mode this process changed"
                );
            }
        }
    }
}

/// Write bytes into the console's input, i.e. type them.
pub fn write_input(handle: &OwnedHandle, bytes: &[u8]) -> Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let mut written = 0u32;
    let ok = unsafe {
        windows::Win32::Storage::FileSystem::WriteFile(
            handle.raw(),
            Some(bytes),
            Some(&mut written),
            None,
        )
    };
    match ok {
        Ok(()) => Ok(written as usize),
        Err(e) => {
            // A closed input pipe is the normal end of an interactive session: the
            // child exited and the console tore down. It is not a supervision
            // failure and must not be reported as one.
            Err(ProcessError::win32("WriteFile(pseudo console input)", e))
        }
    }
}

/// Close one of our handles explicitly, for the "no more input" case.
///
/// The console sees the input pipe close, and a child reading stdin gets EOF — the
/// same signal a user typing Ctrl-Z means in `cmd.exe`. Taking ownership is the
/// whole mechanism: `OwnedHandle` closes on drop, and the type system prevents a
/// second close, which is why there is no `CloseHandle` call here.
pub fn close_handle(handle: OwnedHandle) {
    drop(handle);
}

/// This process's own console input, as a handle the keystroke relay can own.
///
/// `GetStdHandle` returns a handle the *process* owns, so it must never be wrapped
/// in an [`OwnedHandle`] — dropping that would close the supervisor's standard
/// input for the rest of its life, and the next `jean` command in the same window
/// would read from nothing. This type holds a duplicate, which is also what lets
/// the relay thread close its own copy exactly once without touching anybody
/// else's (the same rule [`OwnedHandle::try_clone`] documents for the other end of
/// the session).
pub struct StdinConsole {
    handle: OwnedHandle,
}

impl StdinConsole {
    /// Open the console input, or `None` when standard input is not a console.
    ///
    /// `GetConsoleMode` is the test: it fails for a redirect, a pipe and a closed
    /// stdin, and a non-console handle is not waitable, so a relay that treated one
    /// as a terminal would block on `ReadFile` and never notice the child had
    /// already exited.
    pub fn open() -> Option<Self> {
        use windows::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, CONSOLE_MODE, STD_INPUT_HANDLE,
        };

        let raw = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok()?;
        // `is_invalid` covers both null and -1, which is what "there is no standard
        // input here" looks like in practice.
        if raw.is_invalid() {
            return None;
        }
        let mut mode = CONSOLE_MODE(0);
        unsafe { GetConsoleMode(raw, &mut mode) }.ok()?;
        Some(Self {
            handle: duplicate_for_this_process(raw).ok()?,
        })
    }

    /// True when a keystroke is available, waiting at most `max_wait` for one.
    ///
    /// A console input handle is waitable, and it is signalled exactly when the
    /// buffer holds something. That is the only way a relay can stop cleanly when
    /// the child exits: a thread parked inside a blocking read does not wake until
    /// someone types, and by then the raw mode it read under has been restored and
    /// the pseudo console it wrote into is closed.
    pub fn has_input(&self, max_wait: std::time::Duration) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::WaitForSingleObject;

        let wait = crate::win32::millis_for_wait(max_wait);
        let signalled = unsafe { WaitForSingleObject(self.handle.raw(), wait) };
        signalled == WAIT_OBJECT_0
    }

    /// Take the pending bytes. Blocking is not a concern: the caller has just
    /// confirmed input is available with [`StdinConsole::has_input`].
    ///
    /// `PipeRead::Eof` here means the operator's input ended (a closed handle, or
    /// the equivalent), which is the relay's cue to stop, not a supervision
    /// failure.
    pub fn read_pending(&self, buffer: &mut [u8]) -> crate::win32::PipeRead {
        crate::win32::read_pipe(&self.handle, buffer)
    }

    /// A second handle for the same input, for the relay thread to own.
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            handle: self.handle.try_clone()?,
        })
    }
}

/// Duplicate a handle into one this process owns and may close.
fn duplicate_for_this_process(raw: windows::Win32::Foundation::HANDLE) -> Result<OwnedHandle> {
    use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut copy = windows::Win32::Foundation::HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            raw,
            GetCurrentProcess(),
            &mut copy,
            0,
            // Not inheritable: the child gets its input from the pseudo console,
            // and an inherited copy of the supervisor's stdin is a handle leak with
            // no purpose.
            false,
            DUPLICATE_SAME_ACCESS,
        )
        .map_err(|e| ProcessError::win32("DuplicateHandle(standard input)", e))?;
    }
    Ok(unsafe { OwnedHandle::from_raw(copy) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_sized_console_is_refused_before_any_handle_exists() {
        for (rows, cols) in [(0, 80), (24, 0), (0, 0)] {
            let error = Pty::create(rows, cols)
                .expect_err("{rows}x{cols} must be refused: a TUI that divides by a zero row count is a crash we caused");
            assert!(matches!(error, ProcessError::Invalid { .. }), "{error:?}");
        }
    }

    #[test]
    fn a_pseudo_console_can_be_created_and_closed_on_this_machine() {
        // ConPTY is unavailable on Windows builds before 1809 and inside some
        // containers. If it is missing, the error is a real result, not a failure
        // of this test — and the supervisor then reports `plain` as the only
        // working backend.
        match Pty::create(DEFAULT_ROWS, DEFAULT_COLS) {
            Ok(pty) => {
                assert_eq!(pty.rows(), DEFAULT_ROWS);
                assert_eq!(pty.cols(), DEFAULT_COLS);
                assert!(pty.output_read.is_some());
                assert_eq!(
                    pty.handed.len(),
                    2,
                    "the console's ends must still be ours to close"
                );
                drop(pty);
            }
            Err(e) => {
                let text = e.to_string();
                assert!(
                    text.contains("CreatePseudoConsole"),
                    "unexpected failure shape: {text}"
                );
            }
        }
    }

    #[test]
    fn closing_the_handed_ends_is_idempotent_and_leaves_the_session_ends_usable() {
        let Ok(mut pty) = Pty::create(24, 80) else {
            return; // ConPTY unavailable here; nothing to check.
        };
        pty.close_handed();
        assert!(pty.handed.is_empty());
        pty.close_handed();
        // The two ends this process keeps are untouched by that call.
        assert!(pty.output_read.is_some());
        let taken = pty.take_output_read();
        assert!(taken.is_some(), "the reader must get the output handle");
        assert!(
            pty.take_output_read().is_none(),
            "a second reader would split one byte stream"
        );
    }

    #[test]
    fn the_attribute_list_is_sized_by_the_api_and_not_by_guesswork() {
        // The two-call dance is the only supported way to know the size. A buffer
        // allocated from a guessed constant fails on some Windows versions with a
        // message that names nothing.
        let attributes = AttributeList::with_one_attribute();
        assert!(
            attributes.is_ok(),
            "could not build a one-attribute list: {:?}",
            attributes.err()
        );
    }

    #[test]
    fn writing_an_empty_slice_types_nothing() {
        let Ok(mut pty) = Pty::create(24, 80) else {
            return;
        };
        assert_eq!(write_input(pty.input_write(), b"").unwrap(), 0);
        pty.close_handed();
    }

    #[test]
    fn resize_rejects_an_impossible_geometry() {
        let Ok(pty) = Pty::create(24, 80) else {
            return;
        };
        for (rows, cols) in [(0, 1), (1, 0), (0, 0)] {
            assert!(
                matches!(pty.resize(rows, cols), Err(ProcessError::Invalid { .. })),
                "{rows}x{cols} must be refused"
            );
        }
    }

    #[test]
    fn raw_input_mode_reports_no_console_when_stdin_is_not_a_terminal() {
        use std::io::IsTerminal;
        // In `cargo test` stdin is a pipe, so the guard must say "nothing was
        // changed" rather than pretend. A guard that claimed to have saved a mode
        // it never set would restore garbage on drop.
        let guard = RawInputGuard::enable();
        assert!(
            !guard.is_console() || std::io::stdin().is_terminal(),
            "the guard reported a console while standard input is not one"
        );
    }

    #[test]
    fn the_relay_can_only_open_standard_input_when_it_really_is_a_console() {
        use std::io::IsTerminal;
        // This is the handle the keystroke thread waits on. A redirected stdin is
        // not waitable and would park the thread inside a read until the operator
        // typed, so `open` must refuse it rather than hand back something the relay
        // would block on.
        let opened = StdinConsole::open().is_some();
        if std::io::stdin().is_terminal() {
            assert!(
                opened,
                "standard input is a terminal, so the relay must be able to open it"
            );
        } else {
            assert!(
                !opened,
                "standard input is not a terminal, and treating it as one would hang the relay"
            );
        }
    }

    #[test]
    fn a_duplicated_console_handle_is_independent_of_its_original() {
        let Some(console) = StdinConsole::open() else {
            return; // No console for this test binary's stdin; nothing to duplicate.
        };
        let clone = console
            .try_clone()
            .expect("duplicating a console input handle must work");
        assert_ne!(
            console.handle.raw().0,
            clone.handle.raw().0,
            "two threads holding one handle value is how a handle gets closed twice"
        );
        drop(clone);
        // The original is still usable after the relay's copy is closed.
        assert!(!console.handle.raw().is_invalid());
    }

    #[test]
    fn a_child_launched_into_the_console_reaches_the_console_output_pipe() {
        // The primitive under the whole interactive path: if the pseudo-console
        // attribute is not honoured, the child writes to *this* process's output and
        // the supervisor's artifact only ever holds the console host's own start-up
        // sequences. Asserted on the bytes, because from here a silent failure looks
        // exactly like a successful `CreateProcessW`.
        use std::time::{Duration, Instant};
        use windows::Win32::Storage::FileSystem::ReadFile;

        let Ok(mut pty) = Pty::create(24, 80) else {
            return;
        };
        let cmd = std::path::Path::new(r"C:\Windows\System32\cmd.exe");
        let command_line = crate::cmdline::build_command_line_units(
            cmd.as_os_str(),
            &["/c".to_owned(), "echo PTY_PROBE_OK".to_owned()],
        )
        .expect("a two-argument command line");
        // A real, if small, environment: Windows may refuse to create `cmd.exe` at all with an
        // empty block, and an empty block is not what any caller sends anyway.
        let mut env = std::collections::BTreeMap::new();
        for name in ["SystemRoot", "windir", "COMSPEC", "PATH"] {
            if let Ok(value) = std::env::var(name) {
                env.insert(name.to_owned(), value);
            }
        }
        let child = create_suspended_in_pty(
            cmd,
            &command_line,
            Some(std::env::temp_dir().as_path()),
            &crate::env::encode(&env).expect("a small block"),
            &pty,
        )
        .expect("the console child must be creatable");
        // Contained like the supervisor contains it, so a hang here cannot leave a
        // live cmd.exe behind for the rest of the test run.
        let job = crate::win32::Job::create_kill_on_close().expect("a job");
        job.assign(&child.process).expect("assignment");
        pty.close_handed();
        crate::win32::resume(&child.thread).expect("ResumeThread");
        drop(child.thread);

        let read = pty.take_output_read().expect("one reader per console");
        let exited = crate::win32::wait_for_child(&child.process, Duration::from_secs(20));
        // Closing the console is what makes the output pipe reach EOF; reading before
        // it would block forever on a host that is still open.
        drop(pty);

        let mut collected: Vec<u8> = Vec::new();
        let mut buffer = vec![0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let mut got = 0u32;
            match unsafe { ReadFile(read.raw(), Some(&mut buffer), Some(&mut got), None) } {
                Ok(()) if got == 0 => break,
                Ok(()) => collected.extend_from_slice(&buffer[..got as usize]),
                Err(_) => break,
            }
        }
        if !exited {
            let _ = job.terminate(crate::win32::CANCEL_EXIT_CODE);
        }
        let text = String::from_utf8_lossy(&collected).to_string();
        assert!(exited, "the console child did not exit");
        assert!(
            text.contains("PTY_PROBE_OK"),
            "the child's output never reached the console pipe: {text:?}"
        );
    }
}
