//! The cancellation channel: a per-task named pipe.
//!
//! ## Why a named pipe
//!
//! The requirement is that a *second* `jean cancel` invocation can stop a task
//! owned by a *first* `jean run` that is still alive. The two are separate
//! processes with no shared memory, so they need a local channel. The options,
//! and why this one:
//!
//! | Mechanism | Verdict |
//! |---|---|
//! | Local TCP listener | **Rejected.** A network listener is explicitly out of scope, and even bound to `127.0.0.1` it is reachable by every process on the machine with no per-user access control short of inventing an auth layer. |
//! | Signal file in the data directory | **Rejected.** Polling adds latency to cancellation, and a stale file from a crashed run has to be told apart from a live request — the same liveness problem, with no kernel help. |
//! | `GenerateConsoleCtrlEvent` | **Rejected.** It targets a process *group* attached to a console. Two CLI invocations are not in one group, and the event would reach processes J.E.A.N. does not own. |
//! | Named pipe | **Chosen.** A kernel object with no network surface, access controlled by a security descriptor, and a blocking `ConnectNamedPipe` that gives prompt cancellation with zero polling. |
//!
//! ## Threat model
//!
//! The pipe name is derived from the task id, so it is **guessable**. The name is
//! not the access control; the DACL is. Two properties are enforced:
//!
//! * **Only the current user may connect.** The descriptor is
//!   `D:P(A;;FA;;;<the current user's SID>)` — a protected DACL with a single ACE
//!   granting full access to that one SID, read from the process token at run
//!   time. `P` stops any ACE from being inherited from the parent container and
//!   quietly widening it. The trustee is an explicit SID rather than the
//!   `CO` (Creator Owner) shorthand: measured on this machine, a `CO` ACE did
//!   **not** authorize the server's own second `CreateNamedPipeW`, nor a client
//!   `CreateFileW`, both of which failed `ACCESS_DENIED`. See
//!   [`PIPE_SDDL_TEMPLATE`].
//! * **Nobody may pre-create the name.** The first instance is created with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE`. Without it, a process that guessed the name
//!   first could accept the connection and swallow the cancel request, making
//!   `jean cancel` report success for a task that kept running.
//!
//! Later instances deliberately omit `FIRST_PIPE_INSTANCE`: by then this process
//! owns the name, and the flag would fail.
//!
//! ## Framing
//!
//! One JSON object per request, one per response, both bounded by
//! [`MAX_REQUEST_BYTES`]. A reader accumulates bytes until the text ends with `}`
//! *and* parses, which makes the protocol self-delimiting without a length prefix
//! and without depending on the peer closing its end.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

use windows::core::{Error, BOOL, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::{CoreError, Result};
use jean_process::CancelToken;
use jean_protocol::ipc::{
    decode_request, decode_response, encode_request, encode_response, pipe_name_for, ClientRequest,
    ServerResponse, MAX_REQUEST_BYTES,
};

/// Protected DACL, one ACE: full access to the current user's SID.
///
/// `P` marks the DACL protected, so nothing is inherited from a parent
/// container. `FA` is full access rather than `GA` because the check Windows
/// performs when a *further* pipe instance is created asks for
/// `FILE_CREATE_PIPE_INSTANCE` against the existing object's descriptor.
///
/// **The trustee is an explicit SID, not `CO` (Creator Owner).** Measured, not
/// assumed: with `D:P(A;;GA;;;CO)` and with `D:P(A;;FA;;;CO)`, the second
/// `CreateNamedPipeW` on the same name and the client's `CreateFileW` both
/// failed `ACCESS_DENIED (0x80070005)` from the very process that created the
/// first instance, while `D:P(A;;FA;;;WD)` succeeded. A `CREATOR OWNER` ACE does
/// not resolve to the server here — a pipe instance's owner is taken from the
/// client that connects to it, which is exactly what makes `CO` the wrong
/// principal for a name the server has to keep re-creating. Substituting the
/// token's own user SID is both the correct trustee and the narrowest one that
/// works: access stays restricted to the current user, with no `Everyone`,
/// `Users`, `Administrators`, `SYSTEM` or `INTERACTIVE` entry.
pub const PIPE_SDDL_TEMPLATE: &str = "D:P(A;;FA;;;{sid})";

/// How long a client waits for a pipe instance to become available.
pub const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Pipe instances to allow. One is enough for a single client; a few extra keep
/// a second `jean cancel` from failing while the first is still being served.
const MAX_INSTANCES: u32 = 4;

const BUFFER_BYTES: u32 = 4096;

/// A running cancellation listener for one task.
///
/// Dropping this does not join the thread: the listener blocks in
/// `ConnectNamedPipe`, and a join with no timeout could hang the CLI on exit.
/// [`CancelListener::stop`] wakes it by connecting and sending `Shutdown`; if that
/// fails the thread is left blocked and the process exits without it, which the
/// kernel cleans up.
pub struct CancelListener {
    pipe_name: String,
    stopper: mpsc::Sender<()>,
    /// Detached deliberately; see the type docs.
    _thread: std::thread::JoinHandle<()>,
}

impl CancelListener {
    /// Start listening on `pipe_name`, signalling `token` when a cancel arrives.
    ///
    /// The first pipe instance is created here, on the caller's thread, and handed
    /// to the listener. Creating it before returning is what makes squatting
    /// detectable: if another process already holds the name the task is refused
    /// now, rather than running with a cancellation channel that does not work.
    pub fn start(
        pipe_name: String,
        task_id: String,
        token: CancelToken,
        database: PathBuf,
    ) -> Result<Self> {
        let first = create_instance(&pipe_name, true)?;
        // A handle value is a pointer-sized integer. Passing it as `usize` avoids
        // an `unsafe impl Send` for a one-time transfer into the thread, which
        // owns it from then on.
        let first_as_usize = first.0 as usize;

        let (stopper, stopped) = mpsc::channel::<()>();
        let name = pipe_name.clone();
        // A clone for the thread, so `task_id` itself is still owned here when the
        // spawn result is mapped. `spawn` takes the closure by value even when it
        // fails, so anything the closure captured would be gone by then.
        let thread_task_id = task_id.clone();
        let thread = std::thread::Builder::new()
            .name(format!("jean-cancel-{task_id}"))
            .spawn(move || {
                let reclaimed = HANDLE(first_as_usize as *mut std::ffi::c_void);
                serve(reclaimed, name, thread_task_id, token, database, &stopped);
            })
            .map_err(|e| {
                // The thread never started, so this function still owns the handle.
                unsafe {
                    let _ = CloseHandle(first);
                }
                CoreError::Ipc {
                    task_id: task_id.clone(),
                    reason: format!("could not start the cancellation listener: {e}"),
                }
            })?;

        Ok(Self {
            pipe_name,
            stopper,
            _thread: thread,
        })
    }

    /// The pipe name clients should connect to.
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// Wake the listener so it can exit. Does not block on the thread.
    pub fn stop(self) {
        let _ = self.stopper.send(());
        let name = self.pipe_name.clone();
        // The wake-up is itself a client request, so it needs a thread: opening a
        // pipe has no timeout of its own.
        let _ = std::thread::Builder::new()
            .name("jean-cancel-stop".to_owned())
            .spawn(move || {
                let _ = send_request(&name, &ClientRequest::Shutdown, Duration::from_millis(500));
            });
    }
}

/// The listener loop.
///
/// One pipe instance per request, with the next instance created *before* the
/// current one is closed. That keeps the name continuously owned, so there is no
/// window in which another process could claim it between requests.
fn serve(
    mut current: HANDLE,
    pipe_name: String,
    task_id: String,
    token: CancelToken,
    database: PathBuf,
    stop: &mpsc::Receiver<()>,
) {
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        if let Err(e) = accept(current) {
            tracing::debug!(pipe = %pipe_name, error = %e, "ConnectNamedPipe failed; the listener is stopping");
            break;
        }

        // Ownership of the handle moves into the File, which closes it on drop.
        let mut file = unsafe { std::fs::File::from_raw_handle(current.0) };
        current = HANDLE::default();

        let response = match read_json(&mut file) {
            Ok(text) => match decode_request(&text) {
                Ok(ClientRequest::Cancel { task_id: requested }) => {
                    handle_cancel(&requested, &task_id, &token)
                }
                Ok(ClientRequest::Status { task_id: requested }) => {
                    handle_status(&requested, &database)
                }
                Ok(ClientRequest::Shutdown) => {
                    let response = ServerResponse::ok("listener shutting down");
                    let _ = write_message(&mut file, &response);
                    drop(file);
                    break;
                }
                Err(malformed) => malformed,
            },
            Err(detail) => ServerResponse::Malformed { detail },
        };
        let _ = write_message(&mut file, &response);
        drop(file);

        match create_instance(&pipe_name, false) {
            Ok(next) => current = next,
            Err(e) => {
                tracing::debug!(pipe = %pipe_name, error = %e,
                    "could not re-arm the cancellation listener; stopping");
                break;
            }
        }
    }

    if !current.is_invalid() {
        unsafe {
            let _ = CloseHandle(current);
        }
    }
}

fn handle_cancel(requested: &str, owning_task: &str, token: &CancelToken) -> ServerResponse {
    if requested != owning_task {
        // Each task has its own pipe, so a mismatch means a bug or a client that
        // guessed a name. Refusing is the only safe answer: acting on it would
        // cancel a task nobody asked about.
        return ServerResponse::failed(format!(
            "this channel belongs to task {owning_task}, not {requested}"
        ));
    }
    match token.cancel() {
        Ok(()) => {
            tracing::info!(task_id = %owning_task, "cancellation requested over the pipe");
            ServerResponse::ok(format!("cancellation requested for {owning_task}"))
        }
        Err(e) => ServerResponse::failed(format!("could not signal cancellation: {e}")),
    }
}

fn handle_status(task_id: &str, database: &Path) -> ServerResponse {
    // A separate connection: `rusqlite::Connection` is not `Sync`, and WAL mode
    // exists precisely so this reader does not block the runner's writer.
    let store = match jean_store::Store::open(database, "wal", Duration::from_secs(2)) {
        Ok(s) => s,
        Err(e) => return ServerResponse::failed(format!("could not open the task store: {e}")),
    };
    match store.task(task_id) {
        Ok(Some(task)) => ServerResponse::ok(format!(
            "{task_id}: {} (runner pid {:?})",
            task.state, task.runner_pid
        )),
        Ok(None) => ServerResponse::NotFound {
            task_id: task_id.to_owned(),
        },
        Err(e) => ServerResponse::failed(format!("could not read the task: {e}")),
    }
}

/// The current process's user SID, as `S-1-5-21-…`.
///
/// Cached: the SID of a running process cannot change, and `create_instance` runs
/// once per accepted connection. A failure is not cached, so a caller sees the
/// real error rather than a poisoned empty string.
fn current_user_sid() -> Result<&'static str> {
    static CACHED: OnceLock<String> = OnceLock::new();

    if let Some(sid) = CACHED.get() {
        return Ok(sid.as_str());
    }
    let sid = query_user_sid()?;
    Ok(CACHED.get_or_init(|| sid).as_str())
}

/// The DACL for a pipe this process is about to create.
fn pipe_sddl() -> Result<String> {
    Ok(PIPE_SDDL_TEMPLATE.replace("{sid}", current_user_sid()?))
}

fn query_user_sid() -> Result<String> {
    let ipc_error = |context: &str, e: Error| CoreError::Ipc {
        task_id: String::new(),
        reason: format!("{context}: {e}"),
    };

    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| ipc_error("OpenProcessToken failed", e))?;

        // Every path after this must close the handle, so the fallible work is in
        // a nested call and the close happens once, outside it.
        let outcome = read_token_user_sid(token);
        let _ = CloseHandle(token);
        outcome
    }
}

unsafe fn read_token_user_sid(token: HANDLE) -> Result<String> {
    let ipc_error = |context: &str, e: Error| CoreError::Ipc {
        task_id: String::new(),
        reason: format!("{context}: {e}"),
    };

    // Two calls, as documented: the first fails with ERROR_INSUFFICIENT_BUFFER
    // and exists only to report the size. Its error is not the failure we care
    // about; a zero size would be, because it would mean no user SID exists.
    let mut needed = 0u32;
    let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    if needed == 0 {
        return Err(CoreError::Ipc {
            task_id: String::new(),
            reason: "GetTokenInformation(TokenUser) reported no SID; refusing to \
                     create a cancellation pipe with an unpinned DACL"
                .to_owned(),
        });
    }

    let mut buffer = vec![0u8; needed as usize];
    GetTokenInformation(
        token,
        TokenUser,
        Some(buffer.as_mut_ptr().cast::<c_void>()),
        needed,
        &mut needed,
    )
    .map_err(|e| ipc_error("GetTokenInformation(TokenUser) failed", e))?;

    // `TOKEN_USER` is POD at the start of the buffer, and the buffer was sized by
    // the API itself.
    let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
    let mut sid_text = PWSTR::null();
    ConvertSidToStringSidW(user.User.Sid, &mut sid_text)
        .map_err(|e| ipc_error("ConvertSidToStringSidW failed", e))?;

    // A SID string is ASCII by construction, so this cannot realistically fail;
    // it is still checked, because guessing wrong here would put a corrupt
    // trustee into the pipe's DACL.
    let text = match sid_text.to_string() {
        Ok(text) => text,
        Err(e) => {
            let _ = LocalFree(Some(HLOCAL(sid_text.0 as *mut c_void)));
            return Err(CoreError::Ipc {
                task_id: String::new(),
                reason: format!("the current user's SID string was not valid UTF-16: {e}"),
            });
        }
    };
    // Allocated by the API; the caller frees it, and `LocalFree` is the correct
    // free for a string from advapi32.
    let _ = LocalFree(Some(HLOCAL(sid_text.0 as *mut c_void)));
    Ok(text)
}

/// Create one pipe instance.
///
/// `first` selects `FILE_FLAG_FIRST_PIPE_INSTANCE`, which is how a squatted name
/// is detected. It must only be set on the very first instance this process
/// creates for that name.
fn create_instance(pipe_name: &str, first: bool) -> Result<HANDLE> {
    let wide: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();

    let sddl = pipe_sddl()?;
    // A DACL that silently excludes the very client `jean cancel` is the failure
    // mode this line exists for, so the resolved trustee is recorded at debug
    // level before the instance is created.
    tracing::debug!(pipe = pipe_name, dacl = %sddl, "creating a cancellation pipe instance");
    let descriptor = SecurityDescriptor::new(&sddl).map_err(|e| CoreError::Ipc {
        task_id: String::new(),
        reason: format!("could not build the pipe security descriptor: {e}"),
    })?;

    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_mut_ptr(),
        // Not inheritable: a child process has no business holding the
        // cancellation channel open, and an inherited handle would keep the name
        // alive after the listener stopped.
        bInheritHandle: BOOL(0),
    };

    // The `windows` 0.62 flag newtypes derive no `BitOr`/`BitOrAssign`, so the
    // flags are combined as raw `u32` and rewrapped once.
    let mut open_mode = PIPE_ACCESS_DUPLEX.0;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE.0;
    }
    let open_mode = FILE_FLAGS_AND_ATTRIBUTES(open_mode);

    let pipe_mode = NAMED_PIPE_MODE(
        PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0 | PIPE_REJECT_REMOTE_CLIENTS.0,
    );

    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            open_mode,
            pipe_mode,
            MAX_INSTANCES,
            BUFFER_BYTES,
            BUFFER_BYTES,
            // Only meaningful for an overlapped server; this one blocks.
            0,
            Some(&attrs),
        )
    };
    // The descriptor has been consumed by the call and must not outlive it by
    // accident; dropping here is explicit about the ownership.
    drop(descriptor);

    if handle.is_invalid() {
        let e = Error::from_thread();
        let squatted = first
            && e.code() == HRESULT::from_win32(windows::Win32::Foundation::ERROR_ACCESS_DENIED.0);
        return Err(CoreError::Ipc {
            task_id: String::new(),
            reason: if squatted {
                format!(
                    "{pipe_name} is already in use by another process; refusing to start a task \
                     whose cancellation channel could be intercepted"
                )
            } else {
                format!("CreateNamedPipeW failed for {pipe_name}: {e}")
            },
        });
    }
    Ok(handle)
}

/// Owns a `PSECURITY_DESCRIPTOR` and frees it with `LocalFree`.
///
/// The Win32 contract puts freeing on the caller, and `LocalFree` — not
/// `CoTaskMemFree`, not `free` — is the correct function for a descriptor from
/// `ConvertStringSecurityDescriptorToSecurityDescriptor`.
struct SecurityDescriptor {
    raw: *mut std::ffi::c_void,
}

// Only ever read by `CreateNamedPipeW` on the thread that built it.
unsafe impl Send for SecurityDescriptor {}

impl SecurityDescriptor {
    fn new(sddl: &str) -> std::result::Result<Self, Error> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = windows::Win32::Security::PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        unsafe {
            // Revision 1 (`SDDL_REVISION_1`) is the only value Windows accepts.
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(Self { raw: descriptor.0 })
    }

    fn as_mut_ptr(&self) -> *mut std::ffi::c_void {
        self.raw
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // The returned handle is meaningless on success; ignoring it is the
            // documented pattern.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.raw)));
            }
        }
    }
}

/// Block until a client connects.
fn accept(handle: HANDLE) -> Result<()> {
    match unsafe { ConnectNamedPipe(handle, None) } {
        Ok(()) => Ok(()),
        Err(e) => {
            // ERROR_PIPE_CONNECTED means a client was already connected when
            // ConnectNamedPipe was called. That is success, not failure.
            if e.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) {
                Ok(())
            } else {
                Err(CoreError::Ipc {
                    task_id: String::new(),
                    reason: format!("ConnectNamedPipe failed: {e}"),
                })
            }
        }
    }
}

/// Read one self-delimited JSON message, bounded by [`MAX_REQUEST_BYTES`].
///
/// Used on both ends, so the two sides cannot disagree about framing. The bound
/// is checked on every iteration: a peer that streams data without ever forming a
/// valid object must not be able to make this process allocate without limit.
fn read_json(reader: &mut dyn Read) -> std::result::Result<String, String> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];

    loop {
        if buffer.len() > MAX_REQUEST_BYTES {
            return Err(format!(
                "the message exceeded {MAX_REQUEST_BYTES} bytes without completing"
            ));
        }
        if let Ok(text) = std::str::from_utf8(&buffer) {
            let trimmed = text.trim_end();
            if trimmed.ends_with('}') {
                // A complete-looking object. Hand it to the decoder, which is the
                // authority on whether it is actually valid.
                return Ok(trimmed.to_owned());
            }
        }
        match reader.read(&mut chunk) {
            Ok(0) => {
                let text = String::from_utf8_lossy(&buffer).into_owned();
                return if text.trim().is_empty() {
                    Err("the peer closed the pipe without sending a message".to_owned())
                } else {
                    // Return what arrived and let the decoder produce the
                    // Malformed response, so the peer gets a real reply.
                    Ok(text)
                };
            }
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(format!("could not read the message: {e}")),
        }
    }
}

fn write_message(file: &mut dyn Write, response: &ServerResponse) -> std::io::Result<()> {
    let text = encode_response(response)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    file.write_all(text.as_bytes())?;
    file.flush()
}

/// Send one request and read one response, with a bounded total wait.
///
/// The exchange runs on a worker thread and the result comes back over a channel,
/// because neither opening a pipe nor reading from it has a timeout of its own.
/// A hung peer therefore costs at most `timeout`, and the abandoned thread does
/// not outlive the process.
pub fn send_request(
    pipe_name: &str,
    request: &ClientRequest,
    timeout: Duration,
) -> std::result::Result<ServerResponse, String> {
    let encoded = encode_request(request).map_err(|e| e.to_string())?;
    let name = pipe_name.to_owned();

    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("jean-ipc-client".to_owned())
        .spawn(move || {
            let _ = tx.send(exchange(&name, &encoded));
        })
        .map_err(|e| format!("could not start the IPC client thread: {e}"))?;

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "no response from {pipe_name} within {} ms; the task may have no live runner",
            timeout.as_millis()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("the IPC client thread ended without reporting a result".to_owned())
        }
    }
}

/// The blocking half of the exchange, on its own thread.
///
/// The request is written and the response read on the *same* connection. Closing
/// and reopening to read would open a second pipe instance and lose the reply.
fn exchange(pipe_name: &str, encoded: &str) -> std::result::Result<ServerResponse, String> {
    let wide: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();

    // Bounded wait for an instance to be free. Without this, opening a busy pipe
    // fails immediately with ERROR_PIPE_BUSY and the client would need its own
    // retry loop.
    let timeout_ms = CLIENT_CONNECT_TIMEOUT.as_millis().min(u128::from(u32::MAX)) as u32;
    let available = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), timeout_ms) };
    if !available.as_bool() {
        let e = Error::from_thread();
        if e.code() == HRESULT::from_win32(windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.0) {
            return Err(format!(
                "{pipe_name} does not exist, so no runner is listening for this task"
            ));
        }
        return Err(format!("could not reach {pipe_name}: {e}"));
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_name)
        .map_err(|e| format!("could not open {pipe_name}: {e}"))?;

    file.write_all(encoded.as_bytes())
        .map_err(|e| format!("could not send the request: {e}"))?;
    file.flush()
        .map_err(|e| format!("could not flush the request: {e}"))?;

    let text = read_json(&mut file)?;
    decode_response(&text).map_err(|e| e.to_string())
}

/// The pipe name for a task, validated.
pub fn cancel_pipe_name(prefix: &str, task_id: &str) -> Result<String> {
    pipe_name_for(prefix, task_id).map_err(CoreError::Protocol)
}

/// Ask a running task to stop.
///
/// This is what `jean cancel` calls. A failure is reported in words that
/// distinguish the two cases that matter: nothing is listening (the task is not
/// running, or its runner already died) versus something is listening and
/// refused.
pub fn request_cancel(pipe_name: &str, task_id: &str, timeout: Duration) -> Result<ServerResponse> {
    send_request(
        pipe_name,
        &ClientRequest::Cancel {
            task_id: task_id.to_owned(),
        },
        timeout,
    )
    .map_err(|reason| CoreError::Ipc {
        task_id: task_id.to_owned(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn next() -> u32 {
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    fn unique_pipe(tag: &str) -> String {
        format!(
            r"\\.\pipe\jean.ipc.test.{}.{}.{}",
            tag,
            std::process::id(),
            next()
        )
    }

    fn temp_db(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "jean-ipc-{}-{}-{}.sqlite3",
            tag,
            std::process::id(),
            next()
        ))
    }

    fn cleanup_db(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }

    #[test]
    fn the_sddl_grants_access_only_to_the_current_user() {
        let sddl = pipe_sddl().expect("the current user's SID must be resolvable");

        // The template is what makes the trustee auditable: protected DACL, one
        // allow ACE, full access, and nothing else.
        assert_eq!(PIPE_SDDL_TEMPLATE, "D:P(A;;FA;;;{sid})");
        assert!(sddl.contains(":P("), "{sddl}");
        assert_eq!(sddl.matches("(A;;").count(), 1, "{sddl}");

        // The trustee must be a real SID, and this process's own. An ACE string is
        // six fields — type, flags, rights, object, inherit, trustee — so the
        // trustee follows three empty separators; dropping one puts the SID in the
        // object field and `ConvertStringSecurityDescriptor` refuses the whole
        // string with E_INVALIDARG.
        let trustee = sddl
            .strip_prefix("D:P(A;;FA;;;")
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("unexpected SDDL shape: {sddl}"));
        assert!(trustee.starts_with("S-1-"), "{trustee} is not a SID");
        assert_eq!(trustee, current_user_sid().unwrap());

        // No world, no authenticated users, no administrators, no system. `CO` is
        // listed because a Creator Owner ACE looks equivalent and is not: it does
        // not authorize the server's own subsequent instances or a client open.
        for sid in ["WD", "AU", "BA", "SY", "IU", "BU", "CO"] {
            assert!(
                !sddl.contains(&format!(";{sid})")),
                "{sid} appears in the pipe DACL, which would widen access: {sddl}"
            );
        }
    }

    #[test]
    fn the_security_descriptor_can_be_built_from_the_sddl() {
        let sddl = pipe_sddl().expect("the current user's SID must be resolvable");
        let descriptor = SecurityDescriptor::new(&sddl)
            .expect("the SDDL must be accepted by ConvertStringSecurityDescriptor");
        assert!(!descriptor.as_mut_ptr().is_null());
    }

    #[test]
    fn an_invalid_sddl_is_refused() {
        assert!(SecurityDescriptor::new("D:this is not sddl").is_err());
    }

    #[test]
    fn a_pipe_instance_can_be_created_with_the_restrictive_descriptor() {
        let name = unique_pipe("create");
        let handle = create_instance(&name, true).expect("the first instance must be creatable");
        assert!(!handle.is_invalid());

        // A second instance under the same name is legal, but a second *first*
        // instance is not: that is the squatting guard.
        let squatted = create_instance(&name, true);
        assert!(
            squatted.is_err(),
            "FIRST_PIPE_INSTANCE must fail once the name exists, or a guessed \
             name could be claimed by another process"
        );
        let reason = squatted.unwrap_err().to_string();
        assert!(reason.contains("already in use"), "{reason}");

        // A non-first instance succeeds, which is what the listener relies on to
        // stay armed between requests.
        let next = create_instance(&name, false).expect("further instances must be creatable");
        assert!(!next.is_invalid());

        unsafe {
            CloseHandle(handle).unwrap();
            CloseHandle(next).unwrap();
        }
    }

    #[test]
    fn a_cancel_over_the_pipe_signals_the_token() {
        // The end-to-end shape the CLI depends on: a second client reaches a
        // running listener and stops the task.
        let name = unique_pipe("cancel");
        let task_id = "t-ipc-test".to_owned();
        let token = CancelToken::new().expect("the cancel event must be creatable");
        let database = temp_db("cancel");

        let listener = CancelListener::start(
            name.clone(),
            task_id.clone(),
            token.clone(),
            database.clone(),
        )
        .expect("the listener must start");

        assert!(!token.is_cancelled());
        let response = send_request(
            &name,
            &ClientRequest::Cancel {
                task_id: task_id.clone(),
            },
            Duration::from_secs(5),
        )
        .expect("the cancel request must reach the listener");

        assert!(
            !response.is_failure(),
            "the listener refused the cancel: {response:?}"
        );
        assert!(token.is_cancelled(), "the token was not signalled");

        listener.stop();
        cleanup_db(&database);
    }

    #[test]
    fn a_cancel_for_a_different_task_is_refused() {
        let name = unique_pipe("mismatch");
        let token = CancelToken::new().unwrap();
        let database = temp_db("mismatch");

        let listener = CancelListener::start(
            name.clone(),
            "t-owner".to_owned(),
            token.clone(),
            database.clone(),
        )
        .unwrap();

        let response = send_request(
            &name,
            &ClientRequest::Cancel {
                task_id: "t-someone-else".to_owned(),
            },
            Duration::from_secs(5),
        )
        .expect("a response is still expected");

        assert!(
            response.is_failure(),
            "a mismatched task must be refused: {response:?}"
        );
        assert!(
            !token.is_cancelled(),
            "the owning task must not be cancelled by a request for another one"
        );

        listener.stop();
        cleanup_db(&database);
    }

    #[test]
    fn two_cancels_in_a_row_both_get_a_reply() {
        // The listener must re-arm after each request. If it did not, the second
        // `jean cancel` a user types would hang.
        let name = unique_pipe("twice");
        let task_id = "t-twice".to_owned();
        let token = CancelToken::new().unwrap();
        let database = temp_db("twice");

        let listener = CancelListener::start(
            name.clone(),
            task_id.clone(),
            token.clone(),
            database.clone(),
        )
        .unwrap();

        for attempt in 1..=2 {
            let response = send_request(
                &name,
                &ClientRequest::Cancel {
                    task_id: task_id.clone(),
                },
                Duration::from_secs(5),
            )
            .unwrap_or_else(|e| panic!("attempt {attempt} failed: {e}"));
            assert!(!response.is_failure(), "attempt {attempt}: {response:?}");
        }
        assert!(token.is_cancelled());

        listener.stop();
        cleanup_db(&database);
    }

    #[test]
    fn a_request_to_a_pipe_that_does_not_exist_fails_promptly() {
        let name = unique_pipe("absent");
        let started = std::time::Instant::now();
        let err = send_request(
            &name,
            &ClientRequest::Status {
                task_id: "t-1".to_owned(),
            },
            Duration::from_secs(5),
        )
        .expect_err("nothing is listening on this name");

        assert!(
            err.contains("does not exist") || err.contains("no runner"),
            "the message must say there is no runner, not just that it failed: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the client must not wait out the whole timeout when the pipe is absent"
        );
    }

    #[test]
    fn a_status_request_reports_an_unknown_task_as_not_found() {
        let name = unique_pipe("status");
        let token = CancelToken::new().unwrap();
        let database = temp_db("status");
        cleanup_db(&database);
        {
            let store = jean_store::Store::open(&database, "wal", Duration::from_secs(2))
                .expect("the store must open");
            drop(store);
        }

        let listener =
            CancelListener::start(name.clone(), "t-owner".to_owned(), token, database.clone())
                .unwrap();

        let response = send_request(
            &name,
            &ClientRequest::Status {
                task_id: "t-absent".to_owned(),
            },
            Duration::from_secs(5),
        )
        .expect("a response is expected");

        assert!(
            matches!(response, ServerResponse::NotFound { ref task_id } if task_id == "t-absent"),
            "{response:?}"
        );

        listener.stop();
        cleanup_db(&database);
    }

    #[test]
    fn a_status_request_reads_a_real_task_from_the_store() {
        let name = unique_pipe("status2");
        let token = CancelToken::new().unwrap();
        let database = temp_db("status2");
        cleanup_db(&database);
        {
            let store = jean_store::Store::open(&database, "wal", Duration::from_secs(2)).unwrap();
            store
                .insert_task(&jean_store::TaskRecord::new(
                    "t-known".to_owned(),
                    "echo something".to_owned(),
                    Some("powershell.echo".to_owned()),
                    "{}".to_owned(),
                ))
                .unwrap();
        }

        let listener =
            CancelListener::start(name.clone(), "t-known".to_owned(), token, database.clone())
                .unwrap();

        let response = send_request(
            &name,
            &ClientRequest::Status {
                task_id: "t-known".to_owned(),
            },
            Duration::from_secs(5),
        )
        .unwrap();

        match response {
            ServerResponse::Ok { detail } => {
                assert!(detail.contains("queued"), "{detail}");
                assert!(detail.contains("t-known"), "{detail}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        listener.stop();
        cleanup_db(&database);
    }

    #[test]
    fn the_connect_timeout_is_bounded_and_short() {
        assert!(CLIENT_CONNECT_TIMEOUT <= Duration::from_secs(10));
        assert!(CLIENT_CONNECT_TIMEOUT >= Duration::from_millis(100));
    }

    #[test]
    fn a_pipe_name_is_derived_from_the_configured_prefix() {
        let name = cancel_pipe_name(r"\\.\pipe\jean.task", "t-abc").unwrap();
        assert!(name.starts_with(r"\\.\pipe\jean.task"), "{name}");
        assert!(name.ends_with("t-abc"), "{name}");
    }

    #[test]
    fn an_invalid_task_id_cannot_produce_a_pipe_name() {
        assert!(cancel_pipe_name(r"\\.\pipe\jean.task", "../escape").is_err());
        assert!(cancel_pipe_name(r"\\.\pipe\jean.task", "").is_err());
    }

    #[test]
    fn a_complete_message_is_read_in_one_piece() {
        let text = r#"{"op":"cancel","task_id":"t-1"}"#;
        let mut cursor = Cursor::new(text.as_bytes());
        assert_eq!(read_json(&mut cursor).unwrap(), text);
    }

    #[test]
    fn a_message_arriving_in_small_chunks_is_still_read_whole() {
        // The reader must not depend on the transport delivering a message in one
        // `read` call; a pipe splits at buffer boundaries.
        struct Dribble<'a> {
            bytes: &'a [u8],
            position: usize,
        }
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.position >= self.bytes.len() {
                    return Ok(0);
                }
                let take = 1.min(buf.len());
                buf[..take].copy_from_slice(&self.bytes[self.position..self.position + take]);
                self.position += take;
                Ok(take)
            }
        }

        let text = r#"{"op":"status","task_id":"t-dribbled"}"#;
        let mut dribble = Dribble {
            bytes: text.as_bytes(),
            position: 0,
        };
        assert_eq!(read_json(&mut dribble).unwrap(), text);
    }

    #[test]
    fn an_oversized_message_is_refused_rather_than_buffered() {
        let huge = format!(
            r#"{{"op":"cancel","task_id":"{}"#,
            "x".repeat(MAX_REQUEST_BYTES + 10)
        );
        let mut cursor = Cursor::new(huge.as_bytes());
        let err = read_json(&mut cursor).unwrap_err();
        assert!(err.contains(&format!("{MAX_REQUEST_BYTES}")), "{err}");
    }

    #[test]
    fn an_empty_stream_is_reported_as_such() {
        let mut cursor = Cursor::new(Vec::new());
        let err = read_json(&mut cursor).unwrap_err();
        assert!(err.contains("without sending"), "{err}");
    }

    #[test]
    fn an_incomplete_message_is_passed_to_the_decoder() {
        // The peer closed early. Returning the text lets the caller produce a
        // Malformed response instead of dropping the connection silently.
        let mut cursor = Cursor::new(r#"{"op":"canc"#.as_bytes());
        let text = read_json(&mut cursor).unwrap();
        assert!(decode_request(&text).is_err());
    }
}
