//! The Win32 surface used by the supervisor.
//!
//! Everything `unsafe` in this crate lives here, and each function is a thin,
//! single-purpose wrapper over one or two Win32 calls. The policy and sequencing
//! live in [`crate::runner`], which is safe Rust — so the part that decides what
//! J.E.A.N. does can be read without also reasoning about pointer lifetimes.
//!
//! ## Handles
//!
//! [`OwnedHandle`] closes exactly once, on drop. Leaked and double-closed
//! handles are the classic source of "works until the 500th task" bugs, so
//! ownership is expressed in the type rather than in comments.
//!
//! ## Why `Send` is implemented by hand
//!
//! `HANDLE` is a raw pointer, so the compiler will not let it cross a thread
//! boundary. Kernel handles are not thread-affine, and each pipe read end here
//! has exactly one owner that moves it into its reader thread and closes it
//! there. That is the whole justification for `unsafe impl Send`.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::core::{Error, BOOL, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, FILETIME, HANDLE, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, JobObjectBasicAccountingInformation, QueryInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    GetProcessTimes, InitializeProcThreadAttributeList, OpenProcess, ResumeThread, SetEvent,
    TerminateProcess, UpdateProcThreadAttribute, WaitForMultipleObjects, WaitForSingleObject,
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_ACCESS_RIGHTS,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use crate::error::{ProcessError, Result};

/// Exit code written into a job that J.E.A.N. killed because the deadline
/// elapsed. 124 matches GNU `timeout`, so the number is recognisable to anyone
/// who has read a CI log.
pub const TIMEOUT_EXIT_CODE: u32 = 124;

/// Exit code written into a job that J.E.A.N. killed on a cancel request.
/// 130 is the shell convention for "interrupted" (128 + SIGINT).
pub const CANCEL_EXIT_CODE: u32 = 130;

/// How long to wait for a terminated job to actually finish dying before
/// reporting what we have. Termination is asynchronous from the caller's point
/// of view, so a short bounded wait keeps the outcome honest without letting a
/// wedged process hold the supervisor.
const POST_KILL_GRACE: Duration = Duration::from_secs(5);

/// Pipe read chunk size. 64 KiB is comfortably above the default anonymous pipe
/// buffer, so a chatty child is drained in few large reads rather than many small
/// ones.
pub const READ_CHUNK: usize = 64 * 1024;

/// An empty environment block: one NUL-terminated empty string, i.e. the double
/// NUL terminator and nothing else.
///
/// `CREATE_UNICODE_ENVIRONMENT` requires a valid pointer. Passing the dangling
/// pointer of an empty `Vec` would be undefined behaviour, and passing null
/// would mean "inherit the parent environment" — the opposite of what the
/// allowlist decided.
static EMPTY_ENVIRONMENT_BLOCK: [u8; 2] = [0, 0];

/// A handle this value owns and will close.
#[derive(Debug)]
pub struct OwnedHandle(HANDLE);

// Kernel handles have no thread affinity; see the module docs.
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    /// Take ownership of an already-open handle.
    ///
    /// # Safety
    /// The handle must be valid, nothing else may close it, and `CloseHandle`
    /// must be the right way to release it. Process, thread, event, job and pipe
    /// handles all qualify. A pseudo-handle from `GetCurrentProcess` does **not**
    /// and must never be wrapped.
    pub unsafe fn from_raw(handle: HANDLE) -> Self {
        Self(handle)
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// A handle that refers to nothing, and is never closed.
    ///
    /// Used where an owned handle must be *released* while the field holding it still has a value —
    /// see [`crate::piped::PipedChild::close_stdin`]. `Drop` skips `CloseHandle` for this, so putting
    /// one in a slot is how a field empties itself without double-closing.
    pub fn invalid() -> Self {
        Self(HANDLE::default())
    }

    /// Whether this handle refers to nothing.
    pub fn is_invalid(&self) -> bool {
        self.0.is_invalid()
    }

    /// Release the handle without closing it, transferring ownership to the caller.
    ///
    /// Exists for [`crate::piped`], which hands a duplicated pipe end to a `std::fs::File` and must
    /// not also run this `Drop`. Two closers on one value is a double `CloseHandle`, and after pid
    /// recycling that can close a handle this process no longer owns.
    pub fn into_raw(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }

    /// A second handle for the same object, owned separately.
    ///
    /// Used by the keystroke relay, which must be able to close *its* copy when it
    /// stops while the pseudo console keeps the original for teardown. Two threads
    /// sharing one `OwnedHandle` would double-close it, and a double `CloseHandle`
    /// on a recycled value can close an unrelated handle belonging to another part
    /// of the process.
    pub fn try_clone(&self) -> Result<Self> {
        use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
        use windows::Win32::System::Threading::GetCurrentProcess;
        if self.0.is_invalid() {
            return Err(ProcessError::win32(
                "DuplicateHandle",
                "refusing to duplicate an invalid handle".to_owned(),
            ));
        }
        let mut copy = HANDLE::default();
        unsafe {
            // `DUPLICATE_SAME_ACCESS` rather than a chosen access mask: the copy is
            // for the same use as the original, and narrowing it here would be a
            // second thing to get wrong.
            DuplicateHandle(
                GetCurrentProcess(),
                self.0,
                GetCurrentProcess(),
                &mut copy,
                0,
                // The API declares this as `BOOL`; the 0.62 binding narrows the
                // parameter to `bool`, so an inherited handle would have to be
                // spelled `true` — which is not what a relay thread wants.
                false,
                DUPLICATE_SAME_ACCESS,
            )
            .map_err(|e| ProcessError::win32("DuplicateHandle", e))?;
        }
        Ok(unsafe { Self::from_raw(copy) })
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // `is_invalid` is true for null and for -1, which also covers a
        // pseudo-handle wrapped by mistake.
        if self.0.is_invalid() {
            return;
        }
        if let Err(e) = unsafe { CloseHandle(self.0) } {
            // Nothing useful can be done: the handle is gone either way, and
            // panicking in `Drop` would abort the process.
            tracing::debug!(error = %e, "CloseHandle reported an error during cleanup");
        }
    }
}

/// A NUL-terminated UTF-16 string, kept alive for the duration of a call.
///
/// `CreateProcessW` takes a *mutable* command-line pointer and may write to it,
/// so the buffer must be owned rather than borrowed from a temporary.
pub struct WideString {
    units: Vec<u16>,
}

impl WideString {
    pub fn from_os(s: &std::ffi::OsStr) -> Self {
        let mut units: Vec<u16> = s.encode_wide().collect();
        units.push(0);
        Self { units }
    }

    pub fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.units.as_ptr())
    }

    pub fn pwstr_mut(&mut self) -> PWSTR {
        PWSTR(self.units.as_mut_ptr())
    }
}

/// An anonymous pipe whose write end is inheritable.
///
/// The child inherits the write end. The parent keeps both until
/// `CreateProcessW` returns, then closes its copy of the write end so the reader
/// sees EOF once the child — and every descendant that inherited it — exits.
///
/// That last part is why pipe EOF is a meaningful signal at all: it means no
/// process anywhere still holds the write end, which is a stronger statement
/// than "the direct child exited".
pub struct PipePair {
    pub read: OwnedHandle,
    pub write: OwnedHandle,
}

pub fn create_capture_pipe() -> Result<PipePair> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();

    // nLength must be set: an uninitialised size makes CreatePipe fail with
    // ERROR_INVALID_PARAMETER, which reads like a bug in this code rather than a
    // missing field.
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: BOOL(1),
    };

    // nsize = 0 asks for the system default buffer size.
    unsafe {
        CreatePipe(&mut read, &mut write, Some(&attrs), 0)
            .map_err(|e| ProcessError::win32("CreatePipe", e))?;
    }

    Ok(PipePair {
        read: unsafe { OwnedHandle::from_raw(read) },
        write: unsafe { OwnedHandle::from_raw(write) },
    })
}

/// Mark a handle inheritable or not, after creation.
///
/// `create_capture_pipe` marks both ends inheritable because the write end is the one a capturing
/// child receives. A worker's **stdin** is the mirror image: the child gets the read end, and the
/// parent's write end must not travel with it, or a frame could be written into a pipe the child also
/// holds open as a reader and nobody would ever see EOF.
pub fn set_inheritable(handle: &OwnedHandle, inherit: bool) -> Result<()> {
    use windows::Win32::Foundation::{SetHandleInformation, HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
    let flags = if inherit {
        HANDLE_FLAG_INHERIT
    } else {
        HANDLE_FLAGS(0)
    };
    unsafe {
        SetHandleInformation(handle.raw(), HANDLE_FLAG_INHERIT.0, flags)
            .map_err(|e| ProcessError::win32("SetHandleInformation", e))
    }
}

/// A Job Object configured to kill its whole tree when the last handle closes.
///
/// This is the containment guarantee. A process assigned to a job with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` cannot outlive it, and neither can its
/// descendants: on Windows 8 and later a child created by a process in a job is
/// placed in the same job automatically, and the default limit flags do not
/// permit breakaway.
pub struct Job {
    inner: win32job::Job,
}

impl Job {
    /// Create the job and arm kill-on-close.
    ///
    /// **Version boundary.** `win32job` 2.0.3 depends on `windows` 0.61 while
    /// this workspace uses 0.62, so the dependency tree contains both. That is
    /// safe only because `win32job`'s public API is handle-opaque: it takes and
    /// returns `isize`, never a `windows::...::HANDLE`. Everything below
    /// therefore converts through `isize` and does its own `AssignProcessToJobObject`
    /// and `TerminateJobObject` against 0.62. Do not pass a `HANDLE` from this
    /// crate into `win32job`, and do not accept one back — the two `HANDLE`
    /// types are unrelated and the compiler will reject the attempt.
    pub fn create_kill_on_close() -> Result<Self> {
        let mut info = win32job::ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        let inner = win32job::Job::create_with_limit_info(&info)
            .map_err(|e| ProcessError::win32("CreateJobObject with kill-on-close", e))?;
        Ok(Self { inner })
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.inner.handle() as *mut c_void)
    }

    /// Place a process in the job.
    ///
    /// Must happen **before** the process's main thread is resumed. A process
    /// that has already started can create a child in the microseconds before
    /// assignment, and that child would never be in the job — exactly the leak
    /// `CREATE_SUSPENDED` exists to prevent.
    pub fn assign(&self, process: &OwnedHandle) -> Result<()> {
        unsafe {
            AssignProcessToJobObject(self.handle(), process.raw())
                .map_err(|e| ProcessError::win32("AssignProcessToJobObject", e))
        }
    }

    pub fn is_empty(&self) -> Result<bool> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                Some(self.handle()),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut _,
                std::mem::size_of_val(&info) as u32,
                None,
            )
            .map_err(|e| ProcessError::win32("QueryInformationJobObject", e))?;
        }
        Ok(info.ActiveProcesses == 0)
    }

    /// Terminate every process in the job, descendants included.
    pub fn terminate(&self, exit_code: u32) -> Result<()> {
        unsafe {
            TerminateJobObject(self.handle(), exit_code)
                .map_err(|e| ProcessError::win32("TerminateJobObject", e))
        }
    }
}

/// A suspended child that has not executed a single instruction yet.
pub struct SuspendedChild {
    pub process: OwnedHandle,
    pub thread: OwnedHandle,
    pub pid: u32,
}

/// A `PROC_THREAD_ATTRIBUTE_LIST` with one attribute: the handle list.
///
/// The buffer is owned here and its size comes from the API's own two-call protocol — ask for the
/// size, allocate, initialise — because guessing `size_of` for an opaque structure is exactly the
/// mistake the comment on [`create_capture_pipe`] records for `SECURITY_ATTRIBUTES.nLength`.
/// Deleted on drop, which is after `CreateProcessW` returns: the list is only read during the create
/// call, and dropping it earlier would leave the child inheriting from freed memory.
struct AttributeList {
    buffer: Vec<u8>,
}

impl AttributeList {
    fn new(handles: &[HANDLE]) -> Result<Self> {
        if handles.is_empty() {
            return Err(ProcessError::win32(
                "PROC_THREAD_ATTRIBUTE_HANDLE_LIST",
                "an empty handle list would be a child with no standard streams".to_owned(),
            ));
        }
        let mut size = 0usize;
        unsafe {
            // The probe call: `None` for the list, and the required byte count comes back in `size`.
            // The API signals that by failing, so a failure here is the expected result; what would be
            // an error is a size of zero, which cannot describe one attribute.
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        }
        if size == 0 {
            return Err(ProcessError::win32(
                "InitializeProcThreadAttributeList(probe)",
                "the API reported a zero-sized attribute list".to_owned(),
            ));
        }
        let mut buffer = vec![0u8; size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
        unsafe {
            InitializeProcThreadAttributeList(Some(list), 1, None, &mut size)
                .map_err(|e| ProcessError::win32("InitializeProcThreadAttributeList", e))?;
        }
        let update = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                // The attribute value is the array itself, so its size is the element size times the
                // count — which is what `size_of_val` on a slice computes.
                std::mem::size_of_val(handles),
                None,
                None,
            )
        };
        if let Err(e) = update {
            unsafe { DeleteProcThreadAttributeList(list) };
            return Err(ProcessError::win32(
                "UpdateProcThreadAttribute(HANDLE_LIST)",
                e,
            ));
        }
        Ok(Self { buffer })
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_ptr() as *mut core::ffi::c_void)
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// Create the child **suspended**, with stdout/stderr redirected to pipes.
///
/// The caller must assign the result to a job and then call [`resume`]. Doing it
/// in that order is not a style preference: it is the only sequence in which a
/// descendant can never exist outside the job.
///
/// `lpApplicationName` is set to the resolved executable, so Windows does not
/// search `PATH` and cannot be redirected by the inherited working directory.
/// The command line still carries argv[0], because that is what the child's CRT
/// parses.
#[allow(clippy::too_many_arguments)]
pub fn create_suspended(
    executable: &Path,
    command_line_units: &[u16],
    working_dir: Option<&Path>,
    environment_block: &[u8],
    stdin_read: Option<&OwnedHandle>,
    stdout_write: &OwnedHandle,
    stderr_write: &OwnedHandle,
) -> Result<SuspendedChild> {
    // Owned and mutable: CreateProcessW may write into the command-line buffer.
    // A terminator is appended if the caller did not supply one, so passing the
    // output of `build_command_line_units` directly is always correct.
    let mut command_line: Vec<u16> = command_line_units.to_vec();
    if command_line.last() != Some(&0) {
        command_line.push(0);
    }

    let application = WideString::from_os(executable.as_os_str());
    let current_dir = working_dir.map(|d| WideString::from_os(d.as_os_str()));

    let environment: &[u8] = if environment_block.is_empty() {
        &EMPTY_ENVIRONMENT_BLOCK
    } else {
        environment_block
    };

    // Which handles the child may receive, *named* rather than inherited by flag.
    //
    // `bInheritHandles = TRUE` alone walks the parent's whole handle table and takes every inheritable
    // entry. In a process that supervises several workers at once that is two problems at once:
    // unrelated handles travel into a foreign agent's process, and a handle another thread closes
    // while the table is being walked makes `CreateProcessW` fail with `ERROR_INVALID_PARAMETER` — a
    // race that looks like a bug in the arguments. `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` removes both:
    // only the ends below are considered, and nothing else in this process can be inherited by
    // accident.
    let mut inherit: Vec<HANDLE> = Vec::with_capacity(3);
    if let Some(stdin_read) = stdin_read {
        inherit.push(stdin_read.raw());
    }
    inherit.push(stdout_write.raw());
    inherit.push(stderr_write.raw());

    let attributes = AttributeList::new(&inherit)?;

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdOutput = stdout_write.raw();
    si.StartupInfo.hStdError = stderr_write.raw();
    // A supervised *command* has no interactive input, so `None` stays deliberately
    // invalid: a child that reads stdin then gets an immediate failure instead of
    // hanging on a handle it shares with the supervisor's console. A *worker* — a
    // protocol peer like an ACP agent — is given the read end of a pipe, which is
    // what [`crate::piped`] supplies.
    si.StartupInfo.hStdInput = stdin_read.map_or_else(HANDLE::default, OwnedHandle::raw);
    si.lpAttributeList = attributes.list();

    let mut pi = PROCESS_INFORMATION::default();
    let flags = CREATE_SUSPENDED
        | CREATE_UNICODE_ENVIRONMENT
        | CREATE_NO_WINDOW
        | EXTENDED_STARTUPINFO_PRESENT;
    let cwd = match &current_dir {
        Some(w) => w.pcwstr(),
        None => PCWSTR::null(),
    };

    unsafe {
        CreateProcessW(
            application.pcwstr(),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            true,
            flags,
            Some(environment.as_ptr() as *const c_void),
            cwd,
            &si.StartupInfo,
            &mut pi,
        )
        .map_err(|e| ProcessError::CreateProcess {
            executable: executable.to_path_buf(),
            reason: e.to_string(),
        })?;
    }

    // Wrap immediately: from here every path, including the caller's error
    // paths, closes both handles exactly once.
    let process = unsafe { OwnedHandle::from_raw(pi.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw(pi.hThread) };

    Ok(SuspendedChild {
        process,
        thread,
        pid: pi.dwProcessId,
    })
}

/// Let a suspended child start running.
pub fn resume(thread: &OwnedHandle) -> Result<()> {
    // Returns the previous suspend count, or 0xFFFFFFFF on failure — a sentinel,
    // so the reason has to come from the thread's last-error code.
    let previous = unsafe { ResumeThread(thread.raw()) };
    if previous == u32::MAX {
        return Err(ProcessError::win32("ResumeThread", last_error_message()));
    }
    Ok(())
}

/// Start a process that is **not** put in a Job Object, and return its identity.
///
/// This is the primitive behind `jean launch`, where the whole point is that the
/// application outlives the command that started it. Every other spawn in this crate
/// goes through `create_suspended` + a kill-on-close job, and that is the right shape
/// for a *command* whose output is being captured: J.E.A.N. owns the process tree and
/// takes it down with the task. An installed application is different — putting
/// Notepad in a job that dies with the CLI would close the user's window, which is not
/// what "open this" means.
///
/// Three consequences are handled here rather than left to the caller:
///
/// * **`CREATE_NEW_CONSOLE`.** A console application launched this way gets its own
///   window instead of inheriting (or sharing) the launcher's, which is what an
///   operator expects from "launch" and what keeps it from writing into J.E.A.N.'s own
///   terminal. Mutually exclusive with `CREATE_NO_WINDOW`, which is therefore not set.
/// * **`bInheritHandles = false` and no `STARTF_USESTDHANDLES`.** There is nothing the
///   child should receive: no pipe ends, no event handles. Inheriting the parent's
///   console handles would also have made the launched app's lifetime visible to the
///   parent's console, which is the opposite of detached.
/// * **The creation time is read from the handle we already hold, before closing it.**
///   A pid alone is not identity — Windows recycles them — and asking again later via
///   `process_creation_time_ms` races an application that exits immediately, which is
///   exactly the single-instance handoff case the launch verification has to describe.
///   Reading it here is the only moment both facts are certain.
pub fn create_detached(
    executable: &Path,
    command_line_units: &[u16],
    working_dir: Option<&Path>,
    environment_block: &[u8],
) -> Result<(u32, Option<i64>)> {
    let mut command_line: Vec<u16> = command_line_units.to_vec();
    if command_line.last() != Some(&0) {
        command_line.push(0);
    }

    let application = WideString::from_os(executable.as_os_str());
    let current_dir = working_dir.map(|d| WideString::from_os(d.as_os_str()));

    let environment: &[u8] = if environment_block.is_empty() {
        &EMPTY_ENVIRONMENT_BLOCK
    } else {
        environment_block
    };

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

    let mut pi = PROCESS_INFORMATION::default();
    let flags = CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_CONSOLE;
    let cwd = match &current_dir {
        Some(w) => w.pcwstr(),
        None => PCWSTR::null(),
    };

    unsafe {
        CreateProcessW(
            application.pcwstr(),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            Some(environment.as_ptr() as *const c_void),
            cwd,
            &si,
            &mut pi,
        )
        .map_err(|e| ProcessError::CreateProcess {
            executable: executable.to_path_buf(),
            reason: e.to_string(),
        })?;
    }

    let process = unsafe { OwnedHandle::from_raw(pi.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw(pi.hThread) };
    let pid = pi.dwProcessId;
    let created_ms = creation_time_ms_of(&process);
    // Both handles close here. The process keeps running: nothing owns it, which is
    // the intended asymmetry with the supervised path.
    drop(thread);
    drop(process);
    Ok((pid, created_ms))
}

/// Kill one specific process.
///
/// Used only when job assignment failed, where the process is by definition not
/// in a job and the job's kill-on-close cannot reach it. Every other termination
/// goes through [`Job::terminate`], because that is what reaches the whole tree.
pub fn terminate_process(process: &OwnedHandle, exit_code: u32) -> Result<()> {
    unsafe {
        TerminateProcess(process.raw(), exit_code)
            .map_err(|e| ProcessError::win32("TerminateProcess", e))
    }
}

/// Write to one end of a pipe the child is reading.
///
/// The counterpart of [`read_pipe`] for a worker that takes instructions on stdin.
/// A short write is not treated as success: a partial write on a full pipe buffer
/// would silently truncate a protocol frame, so anything less than the whole buffer
/// is reported and the caller fails the session rather than sending half a message.
pub fn write_pipe(handle: &OwnedHandle, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut written: u32 = 0;
    unsafe {
        windows::Win32::Storage::FileSystem::WriteFile(
            handle.raw(),
            Some(bytes),
            Some(&mut written),
            None,
        )
        .map_err(|e| ProcessError::win32("WriteFile(pipe)", e))?;
    }
    if written as usize != bytes.len() {
        return Err(ProcessError::Win32 {
            context: "WriteFile(pipe)",
            reason: format!(
                "wrote {written} of {} bytes; the pipe buffer is full and the reader is gone",
                bytes.len()
            ),
        });
    }
    Ok(())
}

/// Read the child's exit code.
pub fn exit_code(process: &OwnedHandle) -> Result<i32> {
    let mut code: u32 = 0;
    unsafe {
        GetExitCodeProcess(process.raw(), &mut code)
            .map_err(|e| ProcessError::win32("GetExitCodeProcess", e))?;
    }
    Ok(code as i32)
}

/// The outcome of one read from a capture pipe.
#[derive(Debug)]
pub enum PipeRead {
    /// Bytes were read; the count is returned and the data is in the caller's
    /// buffer.
    Data(usize),
    /// Every write end closed. Normal end of stream.
    Eof,
    /// The read failed for a reason other than end of stream.
    Failed(String),
}

/// One read from a pipe.
///
/// Blocks until data arrives or the write end closes, which is exactly what a
/// dedicated reader thread wants. The loop that calls this lives in
/// [`crate::runner`].
pub fn read_pipe(handle: &OwnedHandle, buffer: &mut [u8]) -> PipeRead {
    let mut read: u32 = 0;
    match unsafe { ReadFile(handle.raw(), Some(buffer), Some(&mut read), None) } {
        Ok(()) => PipeRead::Data(read as usize),
        Err(e) => {
            if is_broken_pipe(&e) {
                PipeRead::Eof
            } else {
                PipeRead::Failed(e.to_string())
            }
        }
    }
}

fn is_broken_pipe(e: &Error) -> bool {
    e.code() == HRESULT::from_win32(ERROR_BROKEN_PIPE.0)
}

/// Which of the two waited-on handles signalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The child exited.
    ProcessExited,
    /// A cancel request arrived.
    CancelRequested,
    /// The timeout elapsed with neither handle signalled.
    TimedOut,
}

/// Wait for the child or a cancellation, up to `timeout`.
///
/// A single wait covering both handles is what makes cancellation prompt. The
/// alternative — polling a flag between short waits — either adds latency to
/// cancellation or burns CPU for the whole life of a long task.
pub fn wait_for_child_or_cancel(
    process: &OwnedHandle,
    cancel: &OwnedHandle,
    timeout: Option<Duration>,
) -> Result<WaitOutcome> {
    let handles = [process.raw(), cancel.raw()];
    let millis = match timeout {
        Some(d) => millis_for_wait(d),
        None => INFINITE,
    };

    let status: WAIT_EVENT = unsafe { WaitForMultipleObjects(&handles, false, millis) };

    if status == WAIT_OBJECT_0 {
        return Ok(WaitOutcome::ProcessExited);
    }
    if status == WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
        return Ok(WaitOutcome::CancelRequested);
    }
    if status == WAIT_TIMEOUT {
        return Ok(WaitOutcome::TimedOut);
    }
    if status == WAIT_FAILED {
        return Err(ProcessError::win32(
            "WaitForMultipleObjects",
            last_error_message(),
        ));
    }
    Err(ProcessError::Supervision {
        executable: PathBuf::new(),
        reason: format!(
            "WaitForMultipleObjects returned an unexpected status {}",
            status.0
        ),
    })
}

/// Wait for the child alone, with a bounded grace period.
///
/// Returns false when the grace period elapsed first, which the caller reports
/// rather than pretending the process is gone.
pub fn wait_for_child(process: &OwnedHandle, grace: Duration) -> bool {
    let status = unsafe { WaitForSingleObject(process.raw(), millis_for_wait(grace)) };
    status == WAIT_OBJECT_0
}

/// The grace period applied after a forced termination.
pub fn post_kill_grace() -> Duration {
    POST_KILL_GRACE
}

/// Convert a `Duration` to the millisecond count a wait function expects.
///
/// Two edge cases matter. A zero duration must stay zero — that means "poll",
/// which is what an already-passed deadline should do. And a duration at or
/// above `INFINITE` must be clamped *down*, because `0xFFFFFFFF` means "wait
/// forever" and silently turning a very long timeout into an infinite one would
/// defeat the entire timeout feature.
///
/// `pub(crate)` because the keystroke relay waits on a console input handle and
/// needs the same two rules; duplicating the clamp is how an infinite wait creeps
/// back in.
pub(crate) fn millis_for_wait(d: Duration) -> u32 {
    let ms = d.as_millis();
    if ms == 0 {
        return 0;
    }
    if ms >= u128::from(INFINITE) {
        return INFINITE - 1;
    }
    ms as u32
}

/// A manual-reset event used as a cancellation signal.
pub struct CancelEvent {
    handle: OwnedHandle,
}

impl CancelEvent {
    /// Create an unsignalled, anonymous, manual-reset event.
    ///
    /// Manual reset rather than auto reset: once cancellation is requested it
    /// must stay requested. An auto-reset event would consume the signal in the
    /// first wait that observed it, and a later wait would never see it.
    pub fn new() -> Result<Self> {
        let handle = unsafe {
            CreateEventW(None, true, false, PCWSTR::null()).map_err(|e| {
                ProcessError::CancelToken {
                    reason: e.to_string(),
                }
            })?
        };
        Ok(Self {
            handle: unsafe { OwnedHandle::from_raw(handle) },
        })
    }

    /// Signal cancellation. Idempotent, because the event is manual reset.
    pub fn cancel(&self) -> Result<()> {
        unsafe { SetEvent(self.handle.raw()).map_err(|e| ProcessError::win32("SetEvent", e)) }
    }

    pub fn handle(&self) -> &OwnedHandle {
        &self.handle
    }
}

/// The exit code `GetExitCodeProcess` reports while a process is still running.
///
/// A terminated process object lingers while anything still holds a handle to it,
/// and `OpenProcess` succeeds against it. Without this check, "the pid opened"
/// and "the process is running" are different questions that look identical.
pub const STILL_ACTIVE: u32 = 259;

/// Open a process for the two read-only queries below, or `None` if it is gone.
fn open_limited(pid: u32) -> Option<OwnedHandle> {
    if pid == 0 {
        return None;
    }
    let raw = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    Some(unsafe { OwnedHandle::from_raw(raw) })
}

/// True when the process behind `handle` has not finished.
fn process_is_running(handle: &OwnedHandle) -> bool {
    let mut code = 0u32;
    match unsafe { GetExitCodeProcess(handle.raw(), &mut code) } {
        Ok(()) => code == STILL_ACTIVE,
        // No exit code readable means no basis for claiming it is alive:
        // "cannot tell" is treated as dead, the safe direction for reconciliation.
        Err(_) => false,
    }
}

/// A process's creation time, in milliseconds since the Unix epoch.
///
/// Paired with a pid this is a durable identity. Windows recycles pids, so
/// "pid 4210 exists" can be a completely different program; a recycled pid has a
/// different creation time, so comparing both distinguishes a live runner from
/// an impostor.
///
/// Returns `None` when the process does not exist or cannot be queried. That is
/// the answer reconciliation needs: "not alive" and "cannot tell" must both mean
/// "do not assume it is running".
pub fn process_creation_time_ms(pid: u32) -> Option<i64> {
    creation_time_ms_of(&open_limited(pid)?)
}

fn creation_time_ms_of(handle: &OwnedHandle) -> Option<i64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    unsafe {
        GetProcessTimes(
            handle.raw(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }

    // FILETIME is 100-nanosecond intervals since 1601-01-01 UTC.
    let intervals = ((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64);
    const EPOCH_DIFFERENCE_100NS: u64 = 116_444_736_000_000_000;
    if intervals < EPOCH_DIFFERENCE_100NS {
        return None;
    }
    Some(((intervals - EPOCH_DIFFERENCE_100NS) / 10_000) as i64)
}

fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Result<HANDLE> {
    unsafe { OpenProcess(access, false, pid).map_err(|e| ProcessError::win32("OpenProcess", e)) }
}

/// True when a process with this pid is **running** and, if a creation time was
/// given, that its creation time matches too.
///
/// Passing `None` for `expected_created_ms` checks identity-free liveness, which
/// is what a test wants when asserting a grandchild died. Production
/// reconciliation always passes the recorded creation time.
///
/// A process that has exited but is still referenced by an open handle is **not**
/// alive here. `OpenProcess` succeeds against those, so a purely handle-based
/// check would report a runner that had already stopped.
pub fn is_same_process_alive(pid: u32, expected_created_ms: Option<i64>) -> bool {
    let Some(handle) = open_limited(pid) else {
        return false;
    };
    if !process_is_running(&handle) {
        return false;
    }
    let Some(actual) = creation_time_ms_of(&handle) else {
        return false;
    };
    match expected_created_ms {
        None => true,
        // The value is truncated to milliseconds, so a runner's own recorded
        // creation time can differ from a later read by at most one.
        Some(expected) => actual.abs_diff(expected) <= 1,
    }
}

/// True when a process with this pid exists **and is still running**.
///
/// Exposed separately because it answers a different question from
/// [`is_same_process_alive`], and conflating the two is how a reconciliation pass
/// ends up trusting a recycled pid.
pub fn pid_exists(pid: u32) -> bool {
    open_limited(pid).is_some_and(|handle| process_is_running(&handle))
}

/// This process's pid and creation time, for recording runner ownership.
pub fn current_process_identity() -> (u32, Option<i64>) {
    // Queried through OpenProcess rather than the `GetCurrentProcess`
    // pseudo-handle: the pseudo-handle must never be closed, and routing both
    // cases through one owning wrapper removes that footgun entirely.
    let pid = std::process::id();
    (pid, process_creation_time_ms(pid))
}

/// Render the calling thread's last Win32 error.
///
/// Only needed where a function returns a bare sentinel instead of a `Result`,
/// and so cannot carry its own error.
fn last_error_message() -> String {
    // `Error::from_thread` reads the thread's last-error code and renders the
    // system message for it. It is what the generated bindings use internally.
    Error::from_thread().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn the_handle_wrapper_has_the_layout_of_a_handle() {
        assert_eq!(
            std::mem::size_of::<OwnedHandle>(),
            std::mem::size_of::<HANDLE>()
        );
        assert_eq!(
            std::mem::align_of::<OwnedHandle>(),
            std::mem::align_of::<HANDLE>()
        );
    }

    #[test]
    fn a_wait_duration_of_zero_stays_zero() {
        assert_eq!(millis_for_wait(Duration::ZERO), 0);
        assert_eq!(millis_for_wait(Duration::from_micros(100)), 0);
    }

    #[test]
    fn an_absurd_timeout_is_clamped_below_infinite() {
        // INFINITE means "wait forever". A configured timeout must never be
        // translated into that, or the timeout feature silently disappears.
        assert_eq!(
            millis_for_wait(Duration::from_secs(u64::MAX / 2)),
            INFINITE - 1
        );
        let just_over = Duration::from_millis(u64::from(INFINITE) + 5_000);
        assert_eq!(millis_for_wait(just_over), INFINITE - 1);
    }

    #[test]
    fn an_ordinary_timeout_is_converted_exactly() {
        assert_eq!(millis_for_wait(Duration::from_secs(30)), 30_000);
        assert_eq!(millis_for_wait(Duration::from_millis(1)), 1);
        assert_eq!(
            millis_for_wait(Duration::from_millis(u64::from(INFINITE) - 1)),
            INFINITE - 1
        );
    }

    #[test]
    fn wide_strings_are_nul_terminated() {
        let w = WideString::from_os(std::ffi::OsStr::new("abc"));
        let ptr = w.pcwstr().as_ptr();
        assert!(!ptr.is_null());
        let collected: Vec<u16> = (0..4).map(|i| unsafe { *ptr.add(i) }).collect();
        assert_eq!(collected, vec![b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }

    #[test]
    fn wide_strings_survive_non_ascii_paths() {
        let text = r"C:\Programme\Grüße 日本語.exe";
        let w = WideString::from_os(std::ffi::OsStr::new(text));
        let ptr = w.pcwstr().as_ptr();
        let mut len = 0usize;
        while unsafe { *ptr.add(len) } != 0 {
            len += 1;
        }
        assert_eq!(len, text.encode_utf16().count());
    }

    #[test]
    fn the_mutable_view_points_at_the_same_buffer() {
        let mut w = WideString::from_os(std::ffi::OsStr::new("abc"));
        assert_eq!(w.pcwstr().as_ptr(), w.pwstr_mut().as_ptr());
    }

    #[test]
    fn an_empty_environment_block_is_still_a_valid_block() {
        // Two NUL bytes and nothing else: the terminator for an empty string plus
        // the terminator for the block.
        assert_eq!(EMPTY_ENVIRONMENT_BLOCK.len(), 2);
        assert_eq!(EMPTY_ENVIRONMENT_BLOCK, [0, 0]);
    }

    #[test]
    fn the_current_process_identity_is_usable() {
        let (pid, created) = current_process_identity();
        assert_eq!(pid, std::process::id());
        let created = created.expect("this process must have a creation time");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(created > 0 && created <= now, "{created} vs {now}");
    }

    #[test]
    fn a_dead_pid_is_not_alive() {
        // pid 0 is never a real process, and these must say so rather than
        // calling OpenProcess on it.
        assert!(!pid_exists(0));
        assert!(!is_same_process_alive(0, None));
        assert!(!is_same_process_alive(0, Some(1)));
        assert!(process_creation_time_ms(0).is_none());
    }

    #[test]
    fn an_absurd_pid_is_not_alive() {
        assert!(!pid_exists(u32::MAX));
        assert!(!is_same_process_alive(u32::MAX, None));
        assert!(process_creation_time_ms(u32::MAX).is_none());
    }

    #[test]
    fn this_process_is_alive_by_both_checks() {
        let (pid, created) = current_process_identity();
        assert!(pid_exists(pid));
        assert!(is_same_process_alive(pid, None));
        assert!(
            is_same_process_alive(pid, created),
            "a process must match its own recorded creation time"
        );
    }

    #[test]
    fn a_pid_with_the_wrong_creation_time_counts_as_dead() {
        // The pid-reuse defence. A recycled pid belongs to a process with a
        // different creation time, and reconciliation must treat it as dead —
        // the safe direction, since the alternative is never cleaning up an
        // orphan.
        let (pid, created) = current_process_identity();
        let created = created.unwrap();
        assert!(!is_same_process_alive(pid, Some(created + 60_000)));
        assert!(!is_same_process_alive(pid, Some(created - 60_000)));
    }

    #[test]
    fn a_creation_time_within_one_millisecond_still_matches() {
        let (pid, created) = current_process_identity();
        let created = created.unwrap();
        assert!(is_same_process_alive(pid, Some(created + 1)));
        assert!(is_same_process_alive(pid, Some(created.saturating_sub(1))));
        assert!(!is_same_process_alive(pid, Some(created + 2)));
    }

    #[test]
    fn the_forced_exit_codes_are_distinct_and_nonzero() {
        assert_ne!(TIMEOUT_EXIT_CODE, CANCEL_EXIT_CODE);
        assert_ne!(TIMEOUT_EXIT_CODE, 0);
        assert_ne!(CANCEL_EXIT_CODE, 0);
        assert_eq!(TIMEOUT_EXIT_CODE, 124);
        assert_eq!(CANCEL_EXIT_CODE, 130);
    }

    #[test]
    fn a_broken_pipe_is_recognised_from_its_hresult() {
        let e = Error::from_hresult(HRESULT::from_win32(ERROR_BROKEN_PIPE.0));
        assert!(is_broken_pipe(&e));

        let other = Error::from_hresult(HRESULT::from_win32(
            windows::Win32::Foundation::ERROR_ACCESS_DENIED.0,
        ));
        assert!(!is_broken_pipe(&other));
    }

    #[test]
    fn a_capture_pipe_can_be_created_and_closed() {
        // Exercises CreatePipe, the inheritable SECURITY_ATTRIBUTES, and the
        // OwnedHandle drop path. Reading is covered by the integration tests,
        // which need a real child.
        let pair = create_capture_pipe().expect("CreatePipe must succeed");
        assert!(!pair.read.raw().is_invalid());
        assert!(!pair.write.raw().is_invalid());
        drop(pair);
    }

    #[test]
    fn a_kill_on_close_job_can_be_created() {
        let job = Job::create_kill_on_close().expect("CreateJobObject must succeed");
        assert!(!job.handle().is_invalid());
    }

    #[test]
    fn a_cancel_event_starts_unsignalled_and_stays_signalled() {
        let event = CancelEvent::new().expect("CreateEventW must succeed");

        // A zero-millisecond wait is a poll: WAIT_TIMEOUT proves it was not
        // already signalled, which is the state a fresh cancel token must be in.
        assert_eq!(
            unsafe { WaitForSingleObject(event.handle().raw(), 0) },
            WAIT_TIMEOUT
        );

        event.cancel().expect("SetEvent must succeed");
        assert_eq!(
            unsafe { WaitForSingleObject(event.handle().raw(), 0) },
            WAIT_OBJECT_0,
            "a manual-reset event must stay signalled"
        );

        // Cancelling twice must not fail or reset anything.
        event.cancel().expect("SetEvent is idempotent");
        assert_eq!(
            unsafe { WaitForSingleObject(event.handle().raw(), 0) },
            WAIT_OBJECT_0
        );
    }

    #[test]
    fn the_last_error_can_always_be_rendered() {
        // No assertion on content: the point is that this cannot panic and
        // never returns an empty string on a thread that has made a Win32 call.
        let _ = last_error_message();
    }
}
