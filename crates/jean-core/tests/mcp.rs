//! Phase 3, MCP: a configured server is asked for a tool, and J.E.A.N. decides first.
//!
//! No server process is started by these tests. Every session is a [`ScriptedPeer`] behind the same
//! [`Peer`] seam the production opener fills, so the only variable is what **J.E.A.N.** did: whether
//! the engine was consulted, what the task row, the run row, the events and the audit trail say, and
//! whether the operator's boundary was respected. The wire itself — a real `rmcp` handshake against a
//! real child, its malformed frames and its timeouts — is covered by `crates/jean-mcp`'s own tests.
//!
//! ## What is asserted, and why that is the right place
//!
//! * A tool the server's `allow_tools` does not name is refused before an executable is resolved, a
//!   task row exists, or a process could possibly start.
//! * A capability on `auto_deny` never reaches the prompter and never reaches the peer.
//! * An allowed call leaves one task row, one run row with the child's pid, and a `mcp_tool_called`
//!   event that names the tool and the size of the answer — not the arguments, which can carry a
//!   credential.
//! * A remembered approval covers the arguments it was granted for and nothing else.
//! * A server that answers with a tool-level failure is recorded as *its* answer, in a failed task,
//!   with the text kept; a session that stalls is closed and reported as a fault. The two read
//!   differently, because they mean different things.
//! * `jean mcp serve` refuses to publish a name the registry cannot back, and refuses to start with
//!   nothing to publish.
//!
//! ## Configuration
//!
//! `JEAN_DATA_DIR` is process-global and cargo runs these tests in threads, so every scenario holds
//! `ENV_LOCK` for as long as its context lives, as `tests/delegation.rs` and `tests/intents.rs` do.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use jean_core::mcp::{call_tool_with, exposed, list_tools_with, serve, Opener, Peer};
use jean_core::{Context, CoreError};
use jean_mcp::{Bounds, CallOutcome, McpError, ToolInfo};
use jean_policy::{Answer, ScriptedPrompt};
use jean_protocol::{EventKind, TaskState, Termination};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn system32(program: &str) -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:\\Windows"));
    Path::new(&root).join("System32").join(program)
}

fn toml_path(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "\\\\"))
}

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jean-mcp-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock must be past the epoch")
            .as_nanos()
    ))
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// `[mcp]` with one server named `fake` that allows `echo.tool`, plus one exposed registry tool.
///
/// `mcp` and `server` are `key = value` lines that **replace** the setting of the same name in their
/// own block. Appending a duplicate would fail to load: both tables refuse unknown and repeated keys.
fn mcp_section(mcp: &str, server: &str) -> String {
    fn apply(block: &str, mut lines: Vec<String>) -> Vec<String> {
        let block = block.trim();
        if !block.is_empty() {
            let redefined: Vec<String> = block
                .lines()
                .filter_map(|line| line.split('=').next())
                .map(|name| format!("{} =", name.trim()))
                .collect();
            lines.retain(|line| !redefined.iter().any(|prefix| line.starts_with(prefix)));
            lines.extend(block.lines().map(str::to_owned));
        }
        lines
    }

    let mcp_lines = apply(
        mcp,
        "[mcp]
enabled = true
request_timeout_secs = 5
max_message_bytes = 8192
max_result_bytes = 4096
shutdown_grace_ms = 200
expose = [\"echo.tool\"]"
            .lines()
            .map(str::to_owned)
            .collect(),
    );
    let server_lines = apply(
        server,
        vec![
            "[[mcp.servers]]".to_owned(),
            "name = \"fake\"".to_owned(),
            format!("command = {}", toml_path(&system32("cmd.exe"))),
            "args = [\"/c\", \"echo\", \"server\"]".to_owned(),
            "capability = \"mcp.call\"".to_owned(),
            "allow_tools = [\"echo.tool\"]".to_owned(),
        ],
    );

    let mut out = mcp_lines.join("\n");
    out.push('\n');
    out.push_str(&server_lines.join("\n"));
    out.push('\n');
    out
}

/// A data directory whose registry has `echo.tool` and a disabled `off.tool`.
fn scenario(
    tag: &str,
    policy: &str,
    mcp: &str,
    server: &str,
) -> (PathBuf, Context, MutexGuard<'static, ()>) {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(dir.join("work")).expect("the data directory must be creatable");
    std::fs::write(
        dir.join("tools.toml"),
        "config_version = 1\n\n[[tool]]\nid = \"echo.tool\"\nkind = \"shell\"\ncommand = \"cmd.exe\"\
         \nargs = [\"/c\", \"echo\", \"ran\"]\nverify = { method = \"exit_code\", expected = 0 }\n\n\
         [[tool]]\nid = \"off.tool\"\nkind = \"shell\"\ncommand = \"cmd.exe\"\n\
         verify = { method = \"none\" }\nenabled = false\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("jean.toml"),
        format!(
            "[workspace]\nroot = {}\n{policy}{}",
            toml_path(&dir.join("work")),
            mcp_section(mcp, server)
        ),
    )
    .expect("the configuration must be writable");

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("JEAN_DATA_DIR", &dir);
    let ctx = Context::load(None).expect("a valid data directory must load");
    assert!(
        ctx.tools.get("echo.tool").is_some(),
        "the fixture registry must load: {:?}",
        ctx.warnings
    );
    (dir, ctx, guard)
}

// ─────────────────────────────────────────────────────────────────────────────
// The scripted peer
// ─────────────────────────────────────────────────────────────────────────────

/// What the peer saw, counted from outside the seam.
#[derive(Default)]
struct Seen {
    opens: usize,
    calls: usize,
    closes: usize,
    arguments: Vec<serde_json::Value>,
}

/// A session whose whole behaviour is the fields below.
///
/// It is a [`Peer`], not a fake server: the seam `jean-core` calls *after* the policy decision, with
/// the same types a live `rmcp` session produces. `failure` makes the request fail the way a stalled
/// or vanished peer does, so the accounting around a failed session is the production accounting.
///
/// Cloneable, because a scenario that asks three questions needs three sessions with the same
/// script — and the `Arc` counter is shared, so the test still sees all three.
///
/// A session is one process: `an_allowed_call_…` asserts `opens == 1` rather than `calls == 1`,
/// because three calls in one session is a shape no stdio server has.
#[derive(Clone)]
struct ScriptedPeer {
    tools: Vec<ToolInfo>,
    text: String,
    /// The peer's own tool-level refusal — an answer, not a fault.
    tool_error: bool,
    /// `"timeout"` or anything else: the session produces no answer.
    failure: Option<String>,
    seen: Arc<Mutex<Seen>>,
}

impl ScriptedPeer {
    fn answering(text: &str) -> Self {
        Self {
            tools: Vec::new(),
            text: text.to_owned(),
            tool_error: false,
            failure: None,
            seen: Arc::new(Mutex::new(Seen::default())),
        }
    }

    fn publishing(mut self, tools: &[&str]) -> Self {
        self.tools = tools
            .iter()
            .map(|name| ToolInfo {
                name: (*name).to_owned(),
                description: format!("the {name} tool"),
                input_schema: serde_json::json!({"type": "object"}),
            })
            .collect();
        self
    }

    fn failing(mut self, kind: &str) -> Self {
        self.failure = Some(kind.to_owned());
        self
    }

    fn erroring(mut self) -> Self {
        self.tool_error = true;
        self
    }
}

impl Peer for ScriptedPeer {
    fn list(&mut self) -> std::result::Result<Vec<ToolInfo>, McpError> {
        if let Some(kind) = &self.failure {
            return Err(error(kind));
        }
        Ok(self.tools.clone())
    }

    fn call(
        &mut self,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> std::result::Result<CallOutcome, McpError> {
        if let Some(kind) = &self.failure {
            return Err(error(kind));
        }
        {
            let mut seen = self.seen.lock().unwrap();
            seen.calls += 1;
            seen.arguments.push(arguments.clone());
        }
        let text = self.text.clone();
        let bytes = text.len() as u64;
        Ok(CallOutcome {
            tool: tool.to_owned(),
            truncated: false,
            is_error: self.tool_error,
            text,
            bytes,
        })
    }

    fn pid(&self) -> u32 {
        // This process's own pid: a run row pointing at a pid that exists is closer to the truth than
        // one inventing a number no process ever had.
        std::process::id()
    }

    fn close(&mut self) -> Option<i32> {
        self.seen.lock().unwrap().closes += 1;
        Some(0)
    }
}

fn error(kind: &str) -> McpError {
    match kind {
        "timeout" => McpError::Timeout {
            server: "fake".to_owned(),
            request: "tools/call echo.tool".to_owned(),
            timeout: std::time::Duration::from_secs(5),
        },
        _ => McpError::Protocol {
            server: "fake".to_owned(),
            reason: "the peer vanished mid-request".to_owned(),
        },
    }
}

/// An opener that hands out a clone of one pre-built peer for each session it is asked for.
fn opener(peer: ScriptedPeer) -> (Arc<Mutex<Seen>>, Box<Opener>) {
    let seen = Arc::clone(&peer.seen);
    let counted = Arc::clone(&peer.seen);
    // The script is cloned per session, and the counters are shared, so a scenario with three calls
    // sees three opens and one tally.
    let template = peer;
    let factory = move |_server: &str, _launch: &jean_process::LaunchSpec, _bounds: &Bounds| {
        counted.lock().unwrap().opens += 1;
        Ok(Box::new(template.clone()) as Box<dyn Peer>)
    };
    (seen, Box::new(factory))
}

fn opens(seen: &Arc<Mutex<Seen>>) -> usize {
    seen.lock().unwrap().opens
}

fn counts(seen: &Arc<Mutex<Seen>>) -> (usize, usize) {
    let guard = seen.lock().unwrap();
    (guard.calls, guard.closes)
}

// ─────────────────────────────────────────────────────────────────────────────
// Refusals that happen before a process could exist
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_disabled_mcp_refuses_before_a_task_row_exists() {
    let (dir, mut ctx, _guard) = scenario("disabled", "", "enabled = false", "");
    let (seen, session) = opener(ScriptedPeer::answering("never"));
    let error = call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect_err("a disabled MCP must not start a server");
    assert!(error.to_string().contains("enabled = false"), "{error}");
    assert_eq!(opens(&seen), 0);
    assert_eq!(ctx.store.count("tasks").unwrap(), 0);
    cleanup(&dir);
}

#[test]
fn a_tool_outside_the_servers_allowlist_is_refused_before_anything_starts() {
    let (dir, mut ctx, _guard) = scenario("allowlist", "", "", "");
    let (seen, session) = opener(ScriptedPeer::answering("never"));
    let error = call_tool_with(
        &mut ctx,
        "fake",
        "delete_everything",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect_err("a name outside the allowlist is not callable");
    let text = error.to_string();
    assert!(text.contains("allow_tools"), "{text}");
    assert!(text.contains("echo.tool"), "{text}");
    assert_eq!(opens(&seen), 0, "the allowlist is checked before a session");
    assert_eq!(
        ctx.store.count("tasks").unwrap(),
        0,
        "a refusal of a name is not a task"
    );
    cleanup(&dir);
}

#[test]
fn an_empty_allowlist_means_nothing_is_callable() {
    // The configuration default, and the one that reads like "all tools" to anyone who guesses.
    let (dir, mut ctx, _guard) = scenario("empty-allow", "", "", "allow_tools = []");
    let (seen, session) = opener(ScriptedPeer::answering("never"));
    let error = call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect_err("an empty allowlist allows nothing");
    assert!(
        error.to_string().contains("calls nothing on it"),
        "{}",
        error
    );
    assert_eq!(opens(&seen), 0);
    cleanup(&dir);
}

#[test]
fn a_capability_on_the_auto_deny_list_never_reaches_the_prompter_or_the_peer() {
    // `auto_deny` is a boundary a person already drew: no prompt, no approval, no process.
    let (dir, mut ctx, _guard) = scenario(
        "auto-deny",
        "[policy]\nauto_deny = [\"mcp.call\"]\n",
        "",
        "",
    );
    let (seen, session) = opener(ScriptedPeer::answering("never"));
    let mut prompter = ScriptedPrompt::approving();
    let report = call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({}),
        &mut prompter,
        &session,
    )
    .expect("a refusal is an answer, and is recorded as one");

    assert_eq!(opens(&seen), 0);
    assert!(
        prompter.asked.is_empty(),
        "a denied capability is not asked about"
    );
    assert_eq!(report.state, TaskState::Failed);
    assert!(report.decision.to_string().contains("auto-deny"));
    let task = ctx.store.require_task(&report.task_id).unwrap();
    assert_eq!(task.outcome.as_deref(), Some("denied"));
    assert!(
        ctx.store.tool_runs(&report.task_id).unwrap().is_empty(),
        "nothing ran, so there is no run to finish"
    );
    let audit = std::fs::read_to_string(&ctx.paths.audit).expect("the audit log must exist");
    assert!(audit.contains("\"mcp_call\""), "{audit}");
    assert!(audit.contains("auto_denied"), "{audit}");
    cleanup(&dir);
}

#[test]
fn a_server_name_that_is_not_configured_is_reported_with_the_names_that_are() {
    let (dir, mut ctx, _guard) = scenario("unknown-server", "", "", "");
    let (_seen, session) = opener(ScriptedPeer::answering("never"));
    let error = call_tool_with(
        &mut ctx,
        "typo",
        "echo.tool",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect_err("there is no server called `typo`");
    assert!(error.to_string().contains("`fake`"), "{error}");
    assert_eq!(ctx.store.count("tasks").unwrap(), 0);
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// An allowed call, and what it leaves behind
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn an_allowed_call_records_the_result_and_returns_it() {
    let (dir, mut ctx, _guard) = scenario("allowed", "", "", "");
    let (seen, session) = opener(ScriptedPeer::answering("hello from the server"));
    let report = call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({"path": "src/main.rs"}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect("the call is allowed by configuration");

    assert_eq!(report.text, "hello from the server");
    assert!(!report.tool_error);
    assert_eq!(report.state, TaskState::Succeeded);
    assert_eq!(report.exit_status(), 0);
    assert_eq!(opens(&seen), 1);
    assert_eq!(counts(&seen), (1, 1), "one call, and the session is closed");
    assert_eq!(
        seen.lock().unwrap().arguments[0],
        serde_json::json!({"path": "src/main.rs"}),
        "the peer is given exactly what the operator typed"
    );

    let runs = ctx.store.tool_runs(&report.task_id).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].child_pid, Some(std::process::id()));
    assert_eq!(runs[0].termination, Some(Termination::Exited));
    assert_eq!(runs[0].exit_code, Some(0));
    assert_eq!(runs[0].stdout_bytes.max(0) as u64, report.bytes);

    let events = ctx.store.events(&report.task_id).unwrap();
    let called = events
        .iter()
        .find(|event| event.kind == EventKind::McpToolCalled)
        .expect("the call is a task event, not only an audit line");
    assert!(called.detail.contains("echo.tool"), "{}", called.detail);
    assert!(called.detail.contains("proven"), "{}", called.detail);

    let audit = std::fs::read_to_string(&ctx.paths.audit).expect("the audit log must exist");
    assert!(audit.contains("\"decision\":\"allowed\""), "{audit}");
    assert!(audit.contains("\"outcome\":\"proven\""), "{audit}");
    cleanup(&dir);
}

#[test]
fn an_event_says_what_was_called_and_never_what_was_passed() {
    // A tool argument is where a credential arrives on the way in. The digest needs the values; the
    // event stream, which `jean tasks` prints, does not.
    let (dir, mut ctx, _guard) = scenario("secret-event", "", "", "");
    let (_seen, session) = opener(ScriptedPeer::answering("ok"));
    let report = call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({"token": "hunter2-s3cr3t"}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect("an allowed call");

    let details = ctx
        .store
        .events(&report.task_id)
        .unwrap()
        .iter()
        .map(|event| event.detail.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !details.contains("hunter2"),
        "the event stream must not carry argument values:\n{details}"
    );
    let task = ctx.store.require_task(&report.task_id).unwrap();
    assert!(
        !task.request_json.contains("hunter2"),
        "the task row names the server and the request, not the payload: {}",
        task.request_json
    );
    cleanup(&dir);
}

#[test]
fn a_remembered_approval_covers_exactly_the_arguments_it_was_granted_for() {
    let (dir, mut ctx, _guard) = scenario(
        "approval",
        "[policy]\nrequire_approval = [\"mcp.call\"]\n",
        "",
        "",
    );
    let (_seen, session) = opener(ScriptedPeer::answering("ok"));
    // One answer per question the engine is allowed to ask: remember the first, allow the second.
    let mut prompter = ScriptedPrompt {
        answers: vec![Answer::ApproveAndRemember, Answer::ApproveOnce],
        asked: Vec::new(),
    };

    call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({"path": "a.txt"}),
        &mut prompter,
        &session,
    )
    .expect("the operator said yes");
    assert_eq!(prompter.asked.len(), 1, "{:?}", prompter.asked);
    // The question is asked in the operator's terms: what is called, and which keys are touched —
    // not a dump of a payload that might hold something somebody pasted in.
    assert!(
        prompter.asked[0].contains("echo.tool"),
        "{}",
        prompter.asked[0]
    );
    assert!(prompter.asked[0].contains("path"), "{}", prompter.asked[0]);

    call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({"path": "a.txt"}),
        &mut prompter,
        &session,
    )
    .expect("the remembered approval covers this exact call");
    assert_eq!(
        prompter.asked.len(),
        1,
        "a covered call must not ask again: {:?}",
        prompter.asked
    );

    // The same tool with a different argument is a different operation.
    call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({"path": "b.txt"}),
        &mut prompter,
        &session,
    )
    .expect("and the second answer covers it");
    assert_eq!(
        prompter.asked.len(),
        2,
        "an approval for `a.txt` must not authorise `b.txt`"
    );
    assert_eq!(ctx.store.list_approvals().unwrap().len(), 1);
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// A session that does not do what it was asked
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_tool_level_failure_is_the_servers_answer_and_keeps_its_text() {
    let (dir, mut ctx, _guard) = scenario("tool-error", "", "", "");
    let (_seen, session) = opener(ScriptedPeer::answering("the file is locked").erroring());
    let report = call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect("an error answer is still an answer");

    assert!(report.tool_error);
    assert_eq!(report.state, TaskState::Failed);
    assert_eq!(report.exit_status(), 1);
    assert_eq!(report.text, "the file is locked");
    let task = ctx.store.require_task(&report.task_id).unwrap();
    assert_eq!(task.outcome.as_deref(), Some("not_proven"));
    // Distinct from a broken session: the server replied, and what it said is evidence about the tool.
    let runs = ctx.store.tool_runs(&report.task_id).unwrap();
    assert_eq!(
        runs[0].error, None,
        "the transport worked; the tool did not"
    );
    cleanup(&dir);
}

#[test]
fn a_stalled_server_is_closed_and_the_run_row_says_why() {
    let (dir, mut ctx, _guard) = scenario("stalled", "", "", "");
    let (seen, session) = opener(ScriptedPeer::answering("").failing("timeout"));
    let error = call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect_err("a peer that never answers has no result");

    assert!(error.to_string().contains("did not answer"), "{error}");
    assert_eq!(
        counts(&seen).1,
        1,
        "a session that failed is still taken down, not left running"
    );
    assert_eq!(counts(&seen).0, 0);

    let task = ctx
        .store
        .list_tasks(jean_store::TaskFilter::All, 10)
        .unwrap()
        .remove(0);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.outcome.as_deref(), Some("error"));
    let runs = ctx.store.tool_runs(&task.id).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("did not answer"),
        "{:?}",
        runs[0].error
    );
    cleanup(&dir);
}

#[test]
fn a_server_that_cannot_be_opened_leaves_a_run_row_saying_so() {
    // The distinction the operator needs: "could not start" is a machine problem, "did not answer" is
    // a peer problem. These two errors must not read alike.
    let (dir, mut ctx, _guard) = scenario("no-spawn", "", "", "");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = Arc::clone(&seen);
    let session: Box<Opener> = Box::new(
        move |_server: &str, _launch: &jean_process::LaunchSpec, _bounds: &Bounds| {
            shared.lock().unwrap().opens += 1;
            Err(CoreError::Mcp(
                "the server process could not start".to_owned(),
            ))
        },
    );

    let error = call_tool_with(
        &mut ctx,
        "fake",
        "echo.tool",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
        &session,
    )
    .expect_err("an unopenable server is an error");
    assert!(error.to_string().contains("could not start"), "{error}");
    assert_eq!(opens(&seen), 1);

    let task = ctx
        .store
        .list_tasks(jean_store::TaskFilter::All, 10)
        .unwrap()
        .remove(0);
    assert_eq!(task.state, TaskState::Failed);
    let runs = ctx.store.tool_runs(&task.id).unwrap();
    assert_eq!(runs.len(), 1, "the attempt is a run row, not a whisper");
    assert_eq!(runs[0].termination, Some(Termination::LaunchFailed));
    assert!(runs[0]
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("could not start"));
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// tools/list
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn listing_marks_the_operators_allowlist_apart_from_the_servers_own_list() {
    let (dir, mut ctx, _guard) = scenario("listing", "", "", "");
    let (_seen, session) =
        opener(ScriptedPeer::answering("").publishing(&["echo.tool", "rm.tool"]));
    let listing = list_tools_with(&mut ctx, "fake", &mut ScriptedPrompt::default(), &session)
        .expect("a list is allowed by configuration");

    assert_eq!(listing.tools.len(), 2);
    assert!(
        listing
            .tools
            .iter()
            .any(|t| t.name == "echo.tool" && t.allowed),
        "{:?}",
        listing.tools
    );
    assert!(
        listing
            .tools
            .iter()
            .any(|t| t.name == "rm.tool" && !t.allowed),
        "what a server advertises is not what J.E.A.N. will call"
    );
    assert_eq!(listing.state, TaskState::Succeeded);
    assert!(listing.decision.is_allowed());
    let task = ctx.store.require_task(&listing.task_id).unwrap();
    assert_eq!(task.state, TaskState::Succeeded);
    cleanup(&dir);
}

#[test]
fn a_refused_list_is_a_record_with_a_reason() {
    let (dir, mut ctx, _guard) = scenario(
        "list-denied",
        "[policy]\nauto_deny = [\"mcp.call\"]\n",
        "",
        "",
    );
    let (seen, session) = opener(ScriptedPeer::answering("").publishing(&["echo.tool"]));
    let listing = list_tools_with(&mut ctx, "fake", &mut ScriptedPrompt::default(), &session)
        .expect("a refusal is still an answer with a task behind it");

    assert_eq!(opens(&seen), 0);
    assert!(listing.tools.is_empty());
    assert_eq!(listing.state, TaskState::Failed);
    assert!(
        listing.notes[0].contains("auto-deny"),
        "{:?}",
        listing.notes
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// The server role: what it will publish, and what it refuses to
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn exposing_a_name_the_registry_cannot_back_refuses_to_start_the_server() {
    // A client must never be offered a tool that resolves to nothing when it is called: the list is
    // checked against the registry before the transport exists.
    let (dir, mut ctx, _guard) = scenario("bad-expose", "", "expose = [\"not.a.tool\"]", "");
    let error = serve(&mut ctx).expect_err("an advertised tool must be a real one");
    let text = error.to_string();
    assert!(text.contains("not.a.tool"), "{text}");
    assert!(text.contains("tool registry"), "{text}");
    cleanup(&dir);
}

#[test]
fn exposing_a_disabled_tool_refuses_to_start_the_server() {
    let (dir, mut ctx, _guard) = scenario("disabled-expose", "", "expose = [\"off.tool\"]", "");
    let error = serve(&mut ctx).expect_err("a disabled tool is not publishable");
    assert!(error.to_string().contains("disabled"), "{error}");
    cleanup(&dir);
}

#[test]
fn a_server_with_nothing_to_publish_refuses_to_start() {
    let (dir, mut ctx, _guard) = scenario("empty-expose", "", "expose = []", "");
    let error = serve(&mut ctx).expect_err("publishing nothing is not a service");
    assert!(error.to_string().contains("expose"), "{error}");
    cleanup(&dir);
}

#[test]
fn the_expose_list_is_reported_with_what_the_registry_says_about_it() {
    // `jean mcp status` reads the same view, so an operator can see that a name they listed is
    // disabled — or is not in the registry at all — before a client discovers it.
    let (dir, ctx, _guard) = scenario(
        "exposed-view",
        "",
        "expose = [\"echo.tool\", \"off.tool\", \"nope\"]",
        "",
    );
    let views = exposed(&ctx);
    assert_eq!(views.len(), 3, "{views:?}");
    let echo = views.iter().find(|v| v.id == "echo.tool").unwrap();
    assert!(echo.in_registry && echo.enabled, "{echo:?}");
    let off = views.iter().find(|v| v.id == "off.tool").unwrap();
    assert!(off.in_registry && !off.enabled, "{off:?}");
    let nope = views.iter().find(|v| v.id == "nope").unwrap();
    assert!(!nope.in_registry, "{nope:?}");
    cleanup(&dir);
}
