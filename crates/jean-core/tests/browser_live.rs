//! Phase 3's browser workflow, run against a real installed Chrome over a real CDP session.
//!
//! [`browser.rs`](browser.rs) checks the gates, which is what makes them testable without a browser:
//! a disabled subsystem, an origin outside the allowlist, and a missing executable all refuse before a
//! process exists. This file does the opposite job. It starts the browser J.E.A.N. configures — the one
//! already installed, as a child, in a profile J.E.A.N. created, on a debugging endpoint J.E.A.N. chose
//! — opens a page J.E.A.N. wrote itself, and checks the four things the phase owes: that the page was
//! navigated, read, acted upon with a real input event, and **judged by a second read the page had to
//! satisfy**.
//!
//! The fixture is deliberately the least interesting page imaginable: one file, no network, a button
//! that writes text into a paragraph, and a guard that refuses to say anything was sent when the input
//! was left empty. That guard is what makes the second scenario mean something: the page can decline,
//! and J.E.A.N. reports the decline as `not_proven` rather than reporting that its own click worked.
//!
//! If this machine has no installed browser, every test here says so and passes without asserting —
//! detection is a blocker reported by the command, not a browser downloaded by a test.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use jean_core::browser::{run, BrowserRequest};
use jean_core::Context;
use jean_policy::ScriptedPrompt;
use jean_protocol::TaskState;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn toml_path(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "\\\\"))
}

fn posix(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// A page whose final text can only be produced by typing and then clicking.
const FIXTURE: &str = r#"<!doctype html>
<html lang="en">
  <head><meta charset="utf-8" /><title>J.E.A.N. browser fixture</title></head>
  <body>
    <h1 id="heading">Local fixture, nothing on the network</h1>
    <p>Status: <strong id="status">not sent</strong></p>
    <label for="name">Message</label>
    <input id="name" type="text" />
    <button id="send" type="button">Send</button>
    <script>
      document.getElementById("send").addEventListener("click", function () {
        var value = document.getElementById("name").value;
        document.getElementById("status").textContent = value.length
          ? "sent: " + value
          : "refused: nothing to send";
      });
    </script>
  </body>
</html>
"#;

/// One scenario: a data directory with `[browser] enabled`, a fixture page, and nothing else on it.
///
/// Returns `None` when no browser can be found, which the tests report rather than pretend around.
fn scenario(tag: &str) -> Option<(PathBuf, String, Context, MutexGuard<'static, ()>)> {
    let found = jean_core::browser::detected()?;
    let dir = std::env::temp_dir().join(format!(
        "jean-browser-live-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("the data directory must be creatable");
    let page = dir.join("fixture.html");
    std::fs::write(&page, FIXTURE).expect("the fixture page must be writable");
    std::fs::write(
        dir.join("tools.toml"),
        "config_version = 1\n\n[[tool]]\nid = \"echo.tool\"\nkind = \"shell\"\ncommand = \"cmd.exe\"\
         \nverify = { method = \"none\" }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("jean.toml"),
        format!(
            "config_version = 1\n\n[workspace]\nroot = {}\n\n[browser]\nenabled = true\n\
             headless = true\nstartup_timeout_secs = 40\naction_timeout_secs = 30\n\
             max_page_bytes = 65536\nbrowser_path = {}\n\
             allowed_origins = [\"file\", \"http://127.0.0.1\"]\n",
            toml_path(&dir),
            toml_path(&found),
        ),
    )
    .expect("the configuration must be writable");

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("JEAN_DATA_DIR", &dir);
    let ctx = Context::load(None).expect("a valid data directory must load");
    assert!(
        ctx.config.browser.enabled,
        "the fixture must be in the browser configuration"
    );
    let url = format!("file:///{}", posix(&page));
    Some((dir, url, ctx, guard))
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_page_is_navigated_read_acted_on_and_then_proven_by_a_second_read() {
    let Some((dir, url, mut ctx, _guard)) = scenario("proven") else {
        eprintln!("skipped: this machine has no installed browser for J.E.A.N. to own");
        return;
    };
    let typed = "hello from jean";
    let report = run(
        &mut ctx,
        &BrowserRequest {
            url: url.clone(),
            inspect: Some("#status".to_owned()),
            r#type: Some(("#name".to_owned(), typed.to_owned())),
            click: Some("#send".to_owned()),
            expect: Some(("#status".to_owned(), format!("sent: {typed}"))),
        },
        &mut ScriptedPrompt::default(),
    )
    .expect("a workflow that completes is a report, not an error");

    // Navigate.
    assert_eq!(report.url, url);
    let pid = report.pid.expect("a browser process J.E.A.N. started");
    assert!(pid > 0);
    // Inspect: the baseline was read *before* anything was done to the page.
    assert_eq!(
        report.before.as_deref(),
        Some("not sent"),
        "the page said this before the interaction: {:?}",
        report.steps
    );
    assert!(!report.excerpt.is_empty(), "the page's own markup was read");
    assert!(!report.truncated, "a fixture this size is not capped");
    assert_eq!(report.title, "J.E.A.N. browser fixture");
    // Interact, then verify from a separate read.
    assert_eq!(
        report.after.as_deref(),
        Some(format!("sent: {typed}").as_str()),
        "the element was read back after the click"
    );
    assert_eq!(
        report.verdict.label(),
        "proven",
        "{}",
        report.verdict.detail()
    );
    assert_eq!(report.state, TaskState::Succeeded);
    // The second witness: the rendered page, kept where the operator can look at it.
    let shot = report.screenshot.clone().expect("a screenshot was taken");
    assert!(shot.is_file(), "{} was not written", shot.display());
    assert!(
        std::fs::metadata(&shot)
            .map(|m| m.len() > 1024)
            .unwrap_or(false),
        "a screenshot of a page with text in it is not tiny"
    );

    // Recorded, not merely returned.
    let events = ctx
        .store
        .events(&report.task_id)
        .expect("the task's events");
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"browser_action"),
        "each step is a task event: {kinds:?}"
    );
    assert!(
        kinds.contains(&"verification_recorded"),
        "the check is a task event: {kinds:?}"
    );
    let task = ctx.store.require_task(&report.task_id).unwrap();
    assert_eq!(task.state, TaskState::Succeeded);
    assert_eq!(task.outcome.as_deref(), Some("proven"));
    let audit = std::fs::read_to_string(&ctx.paths.audit).unwrap_or_default();
    assert!(
        audit.contains("drive_browser"),
        "the workflow is audited as its own kind of act: {audit}"
    );

    // The profile J.E.A.N. made is disposed of, and the artifact directory kept only the screenshot.
    let artifacts = dir.join("artifacts");
    let mut profiles = Vec::new();
    for entry in std::fs::read_dir(&artifacts).expect("the artifact directory exists") {
        let run = entry.expect("one artifact directory").path();
        if run.join("profile").exists() {
            profiles.push(run);
        }
    }
    assert!(
        profiles.is_empty(),
        "a disposable profile was left behind: {profiles:?}"
    );
    cleanup(&dir);
}

#[test]
fn a_page_that_declines_produces_not_proven_and_a_failed_task() {
    // The click is dispatched and the page *answers* it — by refusing, because nothing was typed. A
    // workflow that reported success because its own action returned OK would be worthless; this is the
    // case that shows the verdict comes from the page.
    let Some((dir, url, mut ctx, _guard)) = scenario("not-proven") else {
        eprintln!("skipped: this machine has no installed browser for J.E.A.N. to own");
        return;
    };
    let report = run(
        &mut ctx,
        &BrowserRequest {
            url,
            inspect: None,
            r#type: None,
            click: Some("#send".to_owned()),
            expect: Some(("#status".to_owned(), "sent: anything".to_owned())),
        },
        &mut ScriptedPrompt::default(),
    )
    .expect("a declined page is still a settled workflow");

    assert_eq!(
        report.verdict.label(),
        "not_proven",
        "{}",
        report.verdict.detail()
    );
    assert_eq!(report.after.as_deref(), Some("refused: nothing to send"));
    assert_eq!(report.state, TaskState::Failed);
    let task = ctx.store.require_task(&report.task_id).unwrap();
    assert_eq!(task.outcome.as_deref(), Some("not_proven"));
    assert_eq!(task.state, TaskState::Failed);
    cleanup(&dir);
}

#[test]
fn a_workflow_with_no_expectation_is_unverifiable_and_says_so() {
    // Reading a page is not proving a thing. The honest word is `unverifiable`, and the task needs
    // attention rather than passing.
    let Some((dir, url, mut ctx, _guard)) = scenario("no-check") else {
        eprintln!("skipped: this machine has no installed browser for J.E.A.N. to own");
        return;
    };
    let report = run(
        &mut ctx,
        &BrowserRequest {
            url,
            inspect: Some("#status".to_owned()),
            r#type: None,
            click: None,
            expect: None,
        },
        &mut ScriptedPrompt::default(),
    )
    .expect("a read-only workflow is a settled workflow");

    assert_eq!(report.verdict.label(), "unverifiable");
    assert_eq!(report.before.as_deref(), Some("not sent"));
    assert_eq!(report.state, TaskState::NeedsAttention);
    cleanup(&dir);
}

#[test]
fn a_page_outside_the_allowed_origins_is_refused_before_a_browser_exists() {
    let Some((dir, _url, mut ctx, _guard)) = scenario("origin") else {
        eprintln!("skipped: this machine has no installed browser for J.E.A.N. to own");
        return;
    };
    let error = run(
        &mut ctx,
        &BrowserRequest {
            url: "https://example.com/".to_owned(),
            inspect: None,
            r#type: None,
            click: None,
            expect: None,
        },
        &mut ScriptedPrompt::default(),
    )
    .expect_err("the allowlist is not advisory");
    let text = error.to_string();
    assert!(text.contains("allowed_origins"), "{text}");

    // Nothing was written down, because nothing was decided: the gates run before the task row.
    let tasks = ctx
        .store
        .list_tasks(jean_store::TaskFilter::All, 10)
        .unwrap();
    assert!(
        tasks.is_empty(),
        "a refusal at the origin gate starts no task: {} row(s)",
        tasks.len()
    );
    cleanup(&dir);
}
