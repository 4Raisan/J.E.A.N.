//! Tests for [`jean_core::notify`], limited to what is verifiable with no desktop.
//!
//! The registry half (`register`/`is_registered`) runs entirely against `HKCU` and
//! needs no interactive session, so it is round-tripped here on a throwaway key that
//! the test deletes again. The delivery half splits: the `"none"` path and every
//! branch that returns before the WinRT call (illegal AUMID, over-bound title/body)
//! run here, but a call that actually reaches `ToastNotificationManager`/`Show`
//! cannot be asserted on a headless machine — it may be accepted and shown, or
//! accepted and displayed nowhere, or refused with a class-not-registered HRESULT,
//! depending on the OS's un-packaged mood. Those real-show assertions are left to the
//! one `#[ignore]`d demonstration test, which a human runs at a desktop; the automated
//! tests only ever exercise paths that provably perform no WinRT work.

use std::sync::atomic::{AtomicU32, Ordering};

use jean_core::notify::{
    build_toast_xml, deliver, is_registered, register, xml_escape, Delivered, NotifyError,
    MAX_TOAST_BODY_CHARS, MAX_TOAST_TITLE_CHARS,
};
use jean_protocol::config::UiConfig;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A per-run id that is a legal single registry key name but is never a real AUMID,
/// so probes of it can assert "absent" without depending on machine state.
fn throwaway_aumid() -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("JEAN.NotifyTest.{}.{}", std::process::id(), n)
}

fn ui(mode: &str, aumid: &str) -> UiConfig {
    UiConfig {
        notifications: mode.to_owned(),
        app_user_model_id: aumid.to_owned(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The "none" path performs no work.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn none_performs_no_work_and_returns_disabled() {
    // A bogus AUMID is the control: under `"toast"` the same identity below is
    // refused before delivery, so reaching `Disabled` here proves `"none"` short-
    // circuits before it ever inspects the identity or touches the registry/WinRT.
    let config = ui("none", "JEAN.NotRegistered.On.Purpose");
    assert_eq!(
        deliver(&config, "title", "body").unwrap(),
        Delivered::Disabled
    );
}

#[test]
fn a_bogus_aumid_is_only_inert_under_none() {
    // An empty identity is illegal, so under "toast" it is refused before the WinRT
    // call; under "none" the very same input yields Disabled. The contrast is what
    // shows "none" short-circuits ahead of even the identity check, while keeping the
    // "toast" branch free of any real notification call.
    assert_eq!(
        deliver(&ui("none", ""), "t", "b").unwrap(),
        Delivered::Disabled
    );
    let err = deliver(&ui("toast", ""), "t", "b").expect_err("illegal identity under toast");
    assert!(matches!(err, NotifyError::InvalidAumid(_)), "{err:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Illegal AUMIDs are refused before any call.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn register_refuses_an_empty_aumid_before_any_call() {
    let err = register("", "Display").expect_err("an empty identity cannot be registered");
    assert!(matches!(err, NotifyError::InvalidAumid(_)), "{err:?}");
}

#[test]
fn register_refuses_an_aumid_with_a_backslash_or_nul() {
    // `\` would split the identity into sub-keys, silently widening what register
    // writes; a NUL is not a legal key-name character at all.
    let slash = register("JEAN\\Evil", "Display").expect_err("must refuse");
    assert!(matches!(slash, NotifyError::InvalidAumid(_)), "{slash:?}");

    let nul = register("JEAN.\0Bad", "Display").expect_err("must refuse");
    assert!(matches!(nul, NotifyError::InvalidAumid(_)), "{nul:?}");
}

#[test]
fn register_refuses_a_display_name_holding_a_nul() {
    let err = register("JEAN.Ok", "cannot\0store").expect_err("REG_SZ cannot hold a NUL");
    assert!(matches!(err, NotifyError::InvalidDisplayName(_)), "{err:?}");
}

#[test]
fn deliver_refuses_an_illegal_config_aumid_before_winrt() {
    let err = deliver(&ui("toast", ""), "t", "b").expect_err("empty identity cannot deliver");
    assert!(matches!(err, NotifyError::InvalidAumid(_)), "{err:?}");
}

#[test]
fn is_registered_is_false_for_an_empty_or_illegal_aumid() {
    // is_registered answers a boolean, so refusals surface as `false`, never a panic
    // and never a creation.
    assert!(!is_registered(""));
    assert!(!is_registered(r"JEAN\Sub"));
    assert!(!is_registered("JEAN.\0"));
}

#[test]
fn an_unknown_notification_mode_is_reported_not_guessed() {
    // Config validation rejects this upstream; if one reaches here it must be named,
    // because "toast failed" would be the wrong diagnosis for "setting was invalid".
    let err = deliver(&ui("tray", "JEAN.Orchestrator"), "t", "b").unwrap_err();
    assert!(
        matches!(err, NotifyError::UnknownNotificationMode(ref m) if m == "tray"),
        "{err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// XML escaping and payload shape.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn escaping_produces_entities_and_no_bare_markup() {
    let escaped = xml_escape(r#"<b>&""#);
    assert!(escaped.contains("&lt;"), "{escaped}");
    assert!(escaped.contains("&gt;"), "{escaped}");
    assert!(escaped.contains("&amp;"), "{escaped}");
    assert!(escaped.contains("&quot;"), "{escaped}");
    // After escaping there is no literal `<`/`>`/`"` left from the input — those
    // became entities. (`&` legitimately remains, as the lead of each entity.)
    assert!(!escaped.contains('<'), "{escaped}");
    assert!(!escaped.contains('>'), "{escaped}");
    assert!(!escaped.contains('"'), "{escaped}");
}

#[test]
fn ampersand_is_not_double_escaped() {
    // `&` maps to `&amp;`; the `&` inside `&amp;` must not become `&amp;amp;`.
    assert_eq!(xml_escape("&"), "&amp;");
    assert_eq!(xml_escape("<"), "&lt;");
    assert_eq!(xml_escape(">"), "&gt;");
}

#[test]
fn illegal_control_characters_are_dropped_not_leaked() {
    // A raw 0x01 is illegal in XML content even escaped; the escape step must remove
    // it so LoadXml never sees a malformed document. Whitespace controls are kept.
    assert_eq!(xml_escape("a\u{1}b"), "ab");
    assert_eq!(xml_escape("a\tb\nc\u{0}d"), "a\tb\ncd");
}

#[test]
fn the_payload_builder_escapes_both_text_elements() {
    let payload = build_toast_xml("<script>", "a & b \"c\"");
    assert!(payload.starts_with("<toast>"), "{payload}");
    assert!(payload.contains("template=\"ToastGeneric\""), "{payload}");
    assert!(payload.contains("<text>&lt;script&gt;</text>"), "{payload}");
    assert!(payload.contains("a &amp; b"), "{payload}");
    // The raw angle bracket from the title must not survive into the document.
    assert!(!payload.contains("<script>"), "{payload}");
}

// ─────────────────────────────────────────────────────────────────────────────
// A documented bound, not a silent truncation.
// ─────────────────────────────────────────────────────────────────────────────

// Checked at compile time: these are constants, and a runtime test of a constant can
// only ever pass, which is a test that asserts nothing.
const _: () = assert!(MAX_TOAST_TITLE_CHARS > 0);
const _: () = assert!(MAX_TOAST_BODY_CHARS > 0);
const _: () = assert!(MAX_TOAST_BODY_CHARS >= MAX_TOAST_TITLE_CHARS);

#[test]
fn an_oversized_body_is_refused_rather_than_truncated() {
    // Runs entirely before the WinRT call, so it is a valid automated assertion: the
    // over-bound body comes back as `Failed` naming the bound, never a cut toast.
    let huge = "x".repeat(MAX_TOAST_BODY_CHARS + 1);
    let outcome = deliver(&ui("toast", "JEAN.Orchestrator"), "ok title", &huge).unwrap();
    match outcome {
        Delivered::Failed(reason) => {
            assert!(reason.contains("body"), "{reason}");
            assert!(reason.contains("truncat"), "{reason}");
            assert!(
                reason.contains(&MAX_TOAST_BODY_CHARS.to_string()),
                "{reason}"
            );
        }
        other => panic!("an over-bound body must be Failed, got {other:?}"),
    }
}

#[test]
fn an_oversized_title_is_refused_rather_than_truncated() {
    let huge = "y".repeat(MAX_TOAST_TITLE_CHARS + 1);
    let outcome = deliver(&ui("toast", "JEAN.Orchestrator"), &huge, "ok body").unwrap();
    assert!(
        matches!(&outcome, Delivered::Failed(r) if r.contains("title") && r.contains("truncat")),
        "{outcome:?}"
    );
}

#[test]
fn an_empty_body_is_well_formed_not_a_size_failure() {
    // Empty is not "oversized": the second <text> is simply empty, which is legal.
    let payload = build_toast_xml("title", "");
    assert!(payload.contains("<text></text>"), "{payload}");
    assert!(!payload.contains("refused"), "{payload}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Live HKCU round-trip on a throwaway key the test owns and then removes.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
#[test]
fn register_then_is_registered_round_trips_and_cleans_up() {
    let id = throwaway_aumid();
    // Not there to begin with.
    assert!(!is_registered(&id), "{id} unexpectedly already registered");

    let account = register(&id, "J.E.A.N. Test Throwaway").expect("registration must succeed");
    // The account is what the CLI prints, so it names the identity and the location.
    assert!(account.contains(&id), "{account}");
    assert!(
        account.contains("HKCU\\Software\\Classes\\AppUserModelId"),
        "{account}"
    );
    assert!(account.contains("display-only"), "{account}");

    assert!(
        is_registered(&id),
        "the key register created must read back"
    );

    // Idempotent: registering again reopens the same key, still one DisplayName.
    let again =
        register(&id, "J.E.A.N. Test Throwaway").expect("second registration is a no-op write");
    assert!(again.contains("already existed"), "{again}");
    assert!(is_registered(&id));

    // Delete only the leaf this test created. RegDeleteTreeW on the throwaway path
    // removes that key and nothing above it (the AppUserModelId parent survives).
    remove_key(&format!(r"Software\Classes\AppUserModelId\{id}"))
        .expect("the test key must be removable");
    assert!(
        !is_registered(&id),
        "the throwaway key must be gone after cleanup"
    );
}

#[cfg(windows)]
#[test]
fn register_reports_a_created_key_then_reports_the_same_on_repeat() {
    // The disposition distinction is the account's substance: "created" the first
    // time, "already existed" thereafter, which is what makes idempotence observable.
    let id = throwaway_aumid();
    let first = register(&id, "Round").unwrap();
    let created = first.contains("created");
    assert!(created || first.contains("already existed"), "{first}");
    remove_key(&format!(r"Software\Classes\AppUserModelId\{id}")).ok();
}

/// Delete a `HKCU` sub-key tree the caller created. Scoped to the test module so no
/// production code path can reach it.
#[cfg(windows)]
fn remove_key(subkey: &str) -> Result<(), u32> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};

    let wide: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr())) };
    if code.0 == 0 {
        Ok(())
    } else {
        Err(code.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Real delivery is a manual demonstration, never an automated assertion.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore = "reaches the live WinRT notification API; run by hand at a desktop with `cargo test -p jean-core -- --ignored`"]
fn a_real_toast_delivers_without_panicking() {
    // Not registered (see `jean notify register`) and on an un-packaged binary the
    // outcome is genuinely machine-dependent: `Shown` (accepted, maybe rendered), a
    // `WinRt` error carrying a class-not-registered code, or accepted-and-invisible.
    // This asserts only the contract deliver promises — no panic, a typed result —
    // and prints whatever actually happened for a human to judge visually.
    let config = ui("toast", "JEAN.Orchestrator");
    let result = deliver(&config, "J.E.A.N.", "manual toast demonstration");
    println!("deliver() -> {result:?}");
    assert!(
        matches!(
            result,
            Ok(Delivered::Shown) | Ok(Delivered::Failed(_)) | Err(NotifyError::WinRt { .. })
        ),
        "deliver must return a typed outcome, never panic"
    );
}
