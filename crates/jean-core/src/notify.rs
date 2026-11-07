//! Toast notifications: the raw-WinRT deliverable Phase 1 asked for.
//!
//! [`PLANNING/05-ROADMAP.md`] Phase 1 names one notification deliverable — "toast
//! notifications via raw WinRT (`UI_Notifications`), **with the AUMID registered**
//! under `HKCU\Software\Classes\AppUserModelId\JEAN.Orchestrator`" — and this module
//! is exactly that: register the identity, then deliver a display-only toast under
//! it. It is Windows-only in substance (a registry write plus WinRT), so, like
//! [`crate::ipc`], it reaches for the `windows` crate directly rather than through a
//! `#[cfg]`-gated fallback; the whole crate builds for one platform.
//!
//! ## Why this is raw WinRT and not a wrapper
//!
//! `PLANNING/03-DEPENDENCIES.md` rejects the third-party toast crates (pre-1.0,
//! low adoption) and chooses the raw path precisely so this file is short, auditable,
//! and free of a dependency that could decide to do *more* than show text. A wrapper
//! that also wires up a button handler is the failure mode raw WinRT avoids.
//!
//! ## What a toast may do — and the one thing it must not
//!
//! A toast here is **display-only**. There is no click action, no activation
//! argument, no `Launch`/`protocol:` handler, and no Start Menu shortcut created.
//! The reason is the same one that keeps this module from calling the policy engine:
//! anything that makes a notification *do* something is an execution path, and
//! execution belongs to `jean-policy` (`OperationKind::RegisterNotifications` and
//! `OperationKind::DeliverNotification` are the gates the *caller* consults before
//! reaching in here). This module is what those kinds protect, not a peer of them —
//! it does not decide whether to run; it only performs, or refuses, what it is told.
//!
//! ## What `Delivered::Shown` honestly means
//!
//! **`Shown` means the WinRT API accepted the toast, not that the operator saw it.**
//! For an un-packaged (non-MSIX) executable the AUMID path is a known-degraded route
//! on Windows 11: depending on the build and the shell state the OS may return
//! success and display nothing at all, or refuse outright with a class-not-registered
//! HRESULT (`REGDB_E_CLASSNOTREG`/`0x80040201`, or the `0x80070490` family). This
//! module cannot detect "accepted but never rendered" — that would require a desktop
//! readback it is not allowed to do — so it reports `Shown` only in the weak, literal
//! sense above and says so here rather than letting a caller overstate it.
//!
//! ## Where it writes, and what it refuses to write
//!
//! `register` opens `HKCU\Software\Classes\AppUserModelId\<id>` and sets the single
//! `DisplayName` value — nothing else. It does not touch HKLM, does not request
//! `REG_OPTION_BACKUP_RESTORE` (which would make an access failure look like a
//! successful read through a different security context, the same argument
//! [`crate::ipc`] makes about privileges), and deletes nothing. An
//! `app_user_model_id` that could not be a legal single registry key name — empty, or
//! containing a NUL or a `\` (which would silently create sub-keys and break the
//! one-AUMID guarantee) — is refused before any call is made.
//!
//! ## Bounded and leak-free by construction
//!
//! `deliver` performs no registry or WinRT work at all when `ui.notifications` is
//! `"none"`. The `"toast"` path adds no threads, timers or retries: a slow WinRT call
//! is the operating system's bound to own, not a reason to spawn a watcher. Failure
//! is returned as [`NotifyError`], which names the failing call and carries its
//! Win32/WinRT code — and, by design, has **no variant that can hold the title or
//! body**, so neither a `Debug` print nor an error log can ever echo message content.

use std::iter::once;

use windows::core::{Error as WinRtError, HSTRING, PCWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_CREATED_NEW_KEY, REG_CREATE_KEY_DISPOSITION, REG_OPENED_EXISTING_KEY,
    REG_OPEN_CREATE_OPTIONS, REG_SZ, RRF_RT_REG_SZ,
};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

/// The registry location every function in this module reads or writes, relative to
/// `HKCU`. Windows resolves the AppUserModelId a toast is attributed to here for an
/// un-packaged app; the trailing `\` is part of the prefix, so `key_path` needs no
/// separator of its own.
const AUMID_KEY_PREFIX: &str = r"Software\Classes\AppUserModelId\";

/// The only value `register` writes. An AUMID key with just a `DisplayName` is the
/// Windows-documented minimal registration; writing more (icon, launch path) would
/// be the first step toward the executable activation this module deliberately lacks.
const DISPLAY_NAME_VALUE: &str = "DisplayName";

/// Upper bound on a toast title, in characters.
///
/// A title is a goal stem or an application name; a couple hundred characters is far
/// beyond any real one. The bound exists so a runaway or hostile string produces a
/// documented *refusal* rather than a silently truncated toast — the difference being
/// whether the operator is told the message was cut.
pub const MAX_TOAST_TITLE_CHARS: usize = 200;

/// Upper bound on a toast body, in characters. See [`MAX_TOAST_TITLE_CHARS`].
pub const MAX_TOAST_BODY_CHARS: usize = 2_000;

/// Why a delivery could not proceed, naming the call that failed and the code it
/// returned.
///
/// The variants split along one line the caller cares about: a
/// [`NotifyError::Registry`] or [`NotifyError::WinRt`] means a Win32/WinRT call the
/// module made came back non-success (and always carries that code), whereas the
/// remaining variants mean the inputs were refused *before* any call. Crucially, no
/// variant holds a title or body, so the requirement that message content never reach
/// a debug log is a property of the type, not of every call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyError {
    /// A registry call failed. `code` is the `WIN32_ERROR` the function returned.
    Registry { call: &'static str, code: u32 },
    /// A WinRT call failed. `code` is the `HRESULT` the projection surfaced.
    WinRt { call: &'static str, code: u32 },
    /// The `app_user_model_id` could not be a legal single registry key name.
    InvalidAumid(&'static str),
    /// The display name could not be stored as a `REG_SZ` value.
    InvalidDisplayName(&'static str),
    /// `ui.notifications` held a value the config validator should already have
    /// rejected. The mode string, not a message body, is echoed because "toast
    /// refused to send" is the wrong diagnosis for "this setting was never valid".
    UnknownNotificationMode(String),
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry { call, code } => write!(
                f,
                "the registry call `{call}` failed with Win32 error {code:#010x} ({code})"
            ),
            Self::WinRt { call, code } => {
                write!(
                    f,
                    "the WinRT call `{call}` failed with HRESULT {code:#010x}"
                )
            }
            Self::InvalidAumid(reason) => write!(f, "invalid AppUserModelId: {reason}"),
            Self::InvalidDisplayName(reason) => write!(f, "invalid display name: {reason}"),
            Self::UnknownNotificationMode(mode) => write!(
                f,
                "`ui.notifications` was {mode:?}, which is neither `none` nor `toast`"
            ),
        }
    }
}

impl std::error::Error for NotifyError {}

/// Whether one attempted delivery actually produced a toast.
///
/// [`Delivered::Shown`] carries the precise meaning stated in the module docs: the
/// API accepted the notification, which is the strongest claim a display-only path
/// can make without reading the screen back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivered {
    /// `ui.notifications` was `"none"`; no registry or WinRT work was performed.
    Disabled,
    /// The toast was accepted by the WinRT notification API. See the module docs —
    /// this is *accepted*, not *seen*.
    Shown,
    /// The `"toast"` path was chosen but this particular content was refused before
    /// delivery, with the reason. Distinct from [`NotifyError`]: nothing the module
    /// *called* failed; the input was over a documented bound.
    Failed(String),
}

/// Map a failed WinRT projection onto [`NotifyError::WinRt`], keeping the call name.
fn winrt_error(call: &'static str, error: WinRtError) -> NotifyError {
    NotifyError::WinRt {
        call,
        code: error.code().0 as u32,
    }
}

/// The reason an AUMID cannot be used, or `None` when it can.
///
/// Checked identically by `register`, `deliver` and `is_registered` so the three
/// never disagree about what a valid identity is; `is_registered` answers `false`
/// rather than `Err` because its contract is a plain boolean.
fn aumid_reject_reason(app_user_model_id: &str) -> Option<&'static str> {
    if app_user_model_id.is_empty() {
        Some("it is empty")
    } else if app_user_model_id.contains('\0') {
        Some("it contains a NUL, which is not a legal registry key-name character")
    } else if app_user_model_id.contains('\\') {
        Some("it contains `\\`, which would create sub-keys instead of one AppUserModelId")
    } else {
        None
    }
}

/// A UTF-16 buffer with the terminating null, the shape every `*W` Win32 call wants.
///
/// Mirrors the same helper in [`crate::ipc`]: `encode_utf16` plus one trailing zero.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(once(0)).collect()
}

/// Whether a code point is one XML 1.0 does not allow inside a text node.
///
/// Tab, newline and carriage return are legal in content and are kept; every other
/// C0 control and DEL are illegal *even escaped*, so an unescaped one is the
/// malformed-document failure this module must not turn into a crash or a silent
/// no-op. They are dropped rather than escaped because, unlike `<` or `&`, they carry
/// no displayable text, so removing them truncates no message.
fn xml_illegal(code: char) -> bool {
    matches!(
        code,
        '\0'..='\u{8}' | '\u{0B}' | '\u{0C}' | '\u{0E}'..='\u{1F}' | '\u{7F}'
    )
}

/// Escape text for placement inside an XML element, dropping XML-illegal controls.
///
/// Title and body are untrusted — a goal string or an application name can contain
/// `<`, `&`, `"` and control characters. `<`, `>` and `&` are mandatory escapes in a
/// text node; `"` and `'` are escaped defensively even though a text node tolerates
/// them literally, so the result is safe to reuse anywhere the payload is assembled.
/// `&` is emitted first for every character, so no inserted entity is re-escaped.
///
/// This is `pub` so the payload-building rules are unit-testable without a desktop;
/// it has no side effects and performs no API call.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for code in text.chars() {
        match code {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if xml_illegal(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Assemble the `ToastGeneric` payload for one title and body, escaped.
///
/// The template is the documented two-`<text>` generic toast. Only the two text
/// elements carry untrusted input, and both go through [`xml_escape`], so a value
/// containing `<` produces a well-formed document rather than the malformed one that
/// would otherwise arrive at `XmlDocument::LoadXml`.
///
/// `pub` for the same reason as [`xml_escape`]: it is a pure string and belongs to the
/// part of this feature that can be tested on a machine with no desktop.
pub fn build_toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body),
    )
}

/// Could this build deliver a toast at all — with no side effects?
///
/// Reads for a `DisplayName` value at the AUMID key under `HKCU` and reports whether
/// it is there. It creates nothing, and answers `false` on *any* failure, including
/// `ERROR_FILE_NOT_FOUND` (never registered), a value of the wrong type, or an access
/// denial: for a yes/no capability probe, "not registered" is the only useful answer
/// to give a caller when registration cannot be confirmed.
///
/// Note the deliberate gap this cannot close: the key being present does not promise
/// the OS will *render* an un-packaged app's toast (see the module docs). `true`
/// means "the identity is registered", which is a necessary not sufficient condition.
#[must_use]
pub fn is_registered(app_user_model_id: &str) -> bool {
    if aumid_reject_reason(app_user_model_id).is_some() {
        return false;
    }
    let subkey = format!("{AUMID_KEY_PREFIX}{app_user_model_id}");
    let subkey_w = wide(&subkey);
    let value_w = wide(DISPLAY_NAME_VALUE);

    // A single RegGetValueW with no output buffers is the documented existence-and-
    // type check: it opens the value under the RRF_RT_REG_SZ filter and returns
    // ERROR_SUCCESS only when a REG_SZ by that name is present. Nothing is allocated
    // and nothing is written.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    };
    code.0 == 0
}

/// Write the AUMID into `HKCU\Software\Classes\AppUserModelId\<id>` as `DisplayName`.
///
/// Idempotent: `RegCreateKeyExW` opens an existing key rather than failing, so running
/// this twice leaves the same single value. On success it returns a human-readable
/// account of what happened (whether the key was created or already present, and the
/// name it was set to) for the CLI to print verbatim.
///
/// It refuses an illegal `app_user_model_id` before opening anything, and refuses a
/// display name containing a NUL (which a `REG_SZ` cannot store). The handle returned
/// by `RegCreateKeyExW` is closed on every path out of the success branch, so a
/// failed value write cannot leak the key handle.
pub fn register(app_user_model_id: &str, display_name: &str) -> Result<String, NotifyError> {
    if let Some(reason) = aumid_reject_reason(app_user_model_id) {
        return Err(NotifyError::InvalidAumid(reason));
    }
    if display_name.contains('\0') {
        return Err(NotifyError::InvalidDisplayName(
            "it contains a NUL, which a REG_SZ value cannot store",
        ));
    }

    let subkey = format!("{AUMID_KEY_PREFIX}{app_user_model_id}");
    let subkey_w = wide(&subkey);
    let value_w = wide(DISPLAY_NAME_VALUE);
    // REG_SZ data is null-terminated UTF-16, so the bytes handed to RegSetValueExW
    // are the UTF-16 units (including the terminator) little-endian packed.
    let value_bytes: Vec<u8> = display_name
        .encode_utf16()
        .chain(once(0))
        .flat_map(u16::to_le_bytes)
        .collect();

    let mut handle = HKEY::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION(0);
    let create = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_w.as_ptr()),
            Some(0),
            // lpclass: no class is set — registration is the key's existence plus its
            // DisplayName, not a type annotation.
            PCWSTR::null(),
            // No REG_OPTION_BACKUP_RESTORE, no volatility: a plain per-user key.
            REG_OPEN_CREATE_OPTIONS(0),
            // KEY_SET_VALUE is the narrowest right that lets the very next call write
            // the value; KEY_WRITE would ask for owner/DACL rights we never use.
            KEY_SET_VALUE,
            None,
            &mut handle,
            Some(&mut disposition),
        )
    };
    if create.0 != 0 {
        return Err(NotifyError::Registry {
            call: "RegCreateKeyExW",
            code: create.0,
        });
    }

    // From here the handle is ours and must be closed whatever the value write does.
    let set = unsafe {
        RegSetValueExW(
            handle,
            PCWSTR(value_w.as_ptr()),
            Some(0),
            REG_SZ,
            Some(&value_bytes),
        )
    };
    let close = unsafe { RegCloseKey(handle) };
    if set.0 != 0 {
        return Err(NotifyError::Registry {
            call: "RegSetValueExW",
            code: set.0,
        });
    }
    // A non-success RegCloseKey on a handle we still own is not a registration
    // failure; the value is written and the kernel reclaims the handle regardless, so
    // it is deliberately not surfaced as an error but is left in the trace.
    if close.0 != 0 {
        tracing::debug!(
            code = close.0,
            "RegCloseKey reported an error after a successful DisplayName write"
        );
    }

    let outcome = if disposition.0 == REG_CREATED_NEW_KEY.0 {
        "created"
    } else if disposition.0 == REG_OPENED_EXISTING_KEY.0 {
        "already existed and was reopened"
    } else {
        "reopened"
    };
    Ok(format!(
        "Registered the notification identity `{app_user_model_id}` at \
         HKCU\\{subkey} ({outcome}); DisplayName set to `{display_name}`. Toasts under \
         this identity are display-only: no click action, activation argument, or Start \
         Menu shortcut was created."
    ))
}

/// Deliver one toast, honouring `ui.notifications`.
///
/// `"none"` performs **no work** — no registry read, no WinRT call — and returns
/// [`Delivered::Disabled`]. `"toast"` goes through the raw WinRT path and returns
/// [`Delivered::Shown`] or [`Delivered::Failed`], or an [`NotifyError`] when a WinRT
/// call itself fails (with the call name and HRESULT). Any other value is a config
/// that escaped validation and is reported as such rather than guessed at.
///
/// The call is bounded by construction: it adds no threads, timers, or retries, so
/// the only time it can spend is what the OS's WinRT projection decides to give it.
/// It never panics and never writes outside HKCU (which it does not write here at
/// all) and the toast API.
pub fn deliver(
    config: &jean_protocol::config::UiConfig,
    title: &str,
    body: &str,
) -> Result<Delivered, NotifyError> {
    match config.notifications.as_str() {
        "none" => Ok(Delivered::Disabled),
        "toast" => deliver_toast(config, title, body),
        other => Err(NotifyError::UnknownNotificationMode(other.to_owned())),
    }
}

/// The `"toast"` branch, kept separate so `deliver`'s "none means zero work" promise
/// is readable at a glance rather than buried under the WinRT sequence.
fn deliver_toast(
    config: &jean_protocol::config::UiConfig,
    title: &str,
    body: &str,
) -> Result<Delivered, NotifyError> {
    if let Some(reason) = aumid_reject_reason(&config.app_user_model_id) {
        return Err(NotifyError::InvalidAumid(reason));
    }

    // Bounds are enforced here, before any WinRT call, so an over-long message is a
    // stated refusal and never a silent truncation. Empty title or body is allowed
    // (a title-only toast is a legitimate shape); only size is bounded.
    if title.chars().count() > MAX_TOAST_TITLE_CHARS {
        return Ok(Delivered::Failed(format!(
            "the title exceeds {MAX_TOAST_TITLE_CHARS} characters; refused rather than truncated"
        )));
    }
    if body.chars().count() > MAX_TOAST_BODY_CHARS {
        return Ok(Delivered::Failed(format!(
            "the body exceeds {MAX_TOAST_BODY_CHARS} characters; refused rather than truncated"
        )));
    }

    let payload = build_toast_xml(title, body);

    let document = XmlDocument::new().map_err(|e| winrt_error("XmlDocument::new", e))?;
    document
        .LoadXml(&HSTRING::from(payload.as_str()))
        .map_err(|e| winrt_error("XmlDocument::LoadXml", e))?;
    let toast = ToastNotification::CreateToastNotification(&document)
        .map_err(|e| winrt_error("ToastNotification::CreateToastNotification", e))?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
        config.app_user_model_id.as_str(),
    ))
    .map_err(|e| winrt_error("ToastNotificationManager::CreateToastNotifierWithId", e))?;

    // On an un-packaged app the OS may fail here with a class-not-registered HRESULT;
    // that arrives as NotifyError::WinRt with the code, which is the honest answer —
    // not `Shown`, and not a silent `Disabled`.
    notifier
        .Show(&toast)
        .map_err(|e| winrt_error("ToastNotifier::Show", e))?;

    Ok(Delivered::Shown)
}
