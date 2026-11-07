//! Phase 3's headline workflow, run once end to end against a live ACP agent.
//!
//! The agent here is a Node script — installed on this machine, started as a child, speaking the real
//! protocol over real pipes — because a coding-agent vendor's CLI cannot be exercised in a test
//! without authenticating to it and spending somebody's quota. What is *not* faked is the part this
//! project owns: `jean-process` spawns and bounds it, `jean-acp` speaks the protocol, `jean-policy`
//! decides its requests, `jean-core` records progress, computes the changed-file summary, and then
//! runs **its own** check. The result is the whole §3 sequence:
//!
//! ```text
//! goal → session/new → agent_message_chunk → fs/write_text_file (inside the workspace, covered by
//! the delegation, so no prompt) → end_turn → JEAN's diff → JEAN's verification → `succeeded`
//! ```
//!
//! A second scenario has the same agent reach outside the workspace it was pointed at. That ask is
//! refused before the policy engine is consulted, the file is not written, and J.E.A.N.'s own check —
//! which asks for that file — fails, so the delegation reports `failed` no matter that the agent
//! finished its turn saying it had written it.
//!
//! If this machine has no `node` or no `git`, both scenarios say so and pass without asserting: that
//! is the honest shape of a machine-dependent check. The workspace has to be a repository because the
//! changed-file summary is a `git status` reading, and a delegation without a summary is a delegation
//! whose middle step cannot be shown.
//!
//! ## What the live run proved
//!
//! These scenarios ran first as an investigation, and what they found is the reason §38 and risk R-27
//! exist. Recorded here because it is the most useful evidence in this file:
//!
//! * **The protocol works.** The agent starts, `initialize` and `session/new` complete, the prompt is
//!   sent, and `session/update` arrives as a recorded progress event:
//!   `worker_started … worker_progress: #1 agent_message: writing the file now`.
//! * **The authority works.** The agent's `fs/write_text_file` ask is answered from the delegation's
//!   own scope — `answered allow_once (allow_once)` — with no prompt, and **J.E.A.N. performs the
//!   write**: the file exists, with the agent's bytes.
//! * **The boundary works, and is now on the record.** An agent that asks for a file above the
//!   workspace is refused before the policy engine can be talked into it, the file is not written, and
//!   the refusal is audited as `outside_scope`. It used to be refused inside the connector instead, in
//!   silence: the assertions below passed and the audit file said nothing had happened at all, which is
//!   the worst kind of no.
//! * **What did not finish, and now does:** after the write answer, the session used to never report
//!   settlement — 221 s against a 90 s session, with the idle and permission timers never firing. Both
//!   scenarios now settle in 3.8 s wall-to-wall. The cause was the transport, not the protocol: see
//!   `jean-acp`'s `frames::SharedPipe` and IMPLEMENTATION_STATUS.md §38.
//! * **What the run did fix, on the way:** `resolve_workspace` used to hand `std::fs::canonicalize`'s
//!   verbatim path (`\\?\C:\…`) to the worker and to the check. `cmd.exe` reads that as a UNC path,
//!   prints a warning, and starts in `C:\Windows` instead — which silently turns a verification into
//!   a check run in the wrong tree. See [`plain_windows_path`](jean_core::delegation).
//!
//! ## Configuration
//!
//! `JEAN_DATA_DIR` is process-global, so each scenario holds `ENV_LOCK` while its context lives.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use jean_core::delegation::{delegate, DelegateRequest};
use jean_core::Context;
use jean_policy::ScriptedPrompt;
use jean_protocol::EventKind;
use jean_protocol::TaskState;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn toml_path(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "\\\\"))
}

/// Forward slashes, because the path ends up inside a JavaScript string literal.
fn posix(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jean-acp-live-{}-{}-{}",
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

fn on_path(command: &str) -> Option<PathBuf> {
    jean_core::resolve::resolve_executable(
        command,
        std::env::var("PATH").ok().as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
    )
    .ok()
}

/// An ACP agent in about thirty lines: handshake, one streamed message, one file write, one answer.
///
/// `argv[2]` is the path it will ask J.E.A.N. to write. Nothing in the script decides anything — every
/// question it asks is answered by the host, and it exits when the transport closes.
const AGENT_SCRIPT: &str = r#"
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (obj) => process.stdout.write(JSON.stringify(obj) + String.fromCharCode(10));
const target = process.argv[2];
let promptId = null;

rl.on('line', (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch (e) { return; }

  if (msg.method === 'initialize') {
    send({ jsonrpc: '2.0', id: msg.id, result: {
      protocolVersion: 1,
      agentCapabilities: {},
      agentInfo: { name: 'jean-fixture-acp', version: '1.0.0' } } });
  } else if (msg.method === 'session/new') {
    send({ jsonrpc: '2.0', id: msg.id, result: { sessionId: 'fixture-1' } });
  } else if (msg.method === 'session/prompt') {
    promptId = msg.id;
    send({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'fixture-1',
      update: { sessionUpdate: 'agent_message_chunk',
                content: { type: 'text', text: 'writing the file now' } } } });
    // The agent asks the client to perform the write. That is ACP's own shape: the client decides.
    send({ jsonrpc: '2.0', id: 'fs-1', method: 'fs/write_text_file', params: {
      sessionId: 'fixture-1', path: target, content: 'written by the agent\n' } });
  } else if (msg.id === 'fs-1') {
    if (msg.error) {
      send({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'fixture-1',
        update: { sessionUpdate: 'agent_message_chunk',
                  content: { type: 'text', text: 'the client refused my write' } } } });
    }
    send({ jsonrpc: '2.0', id: promptId, result: { stopReason: 'end_turn' } });
  }
});
"#;

/// One scenario: a git workspace whose `acp` worker is the Node agent, pointed either inside that
/// workspace (`escape = false`) or at the data directory above it.
///
/// Returns `None` when `node` or `git` is missing, or when the scratch repository cannot be made.
fn scenario(
    tag: &str,
    escape: bool,
) -> Option<(PathBuf, PathBuf, PathBuf, Context, MutexGuard<'static, ()>)> {
    let runtime = on_path("node")?;
    let git = on_path("git")?;
    let dir = temp_dir(tag);
    let workspace = dir.join("work");
    std::fs::create_dir_all(&workspace).expect("the workspace must be creatable");
    std::fs::write(workspace.join("already.txt"), "the operator's own file\n").unwrap();
    std::fs::write(dir.join("agent-acp.js"), AGENT_SCRIPT)
        .expect("the agent fixture must be writable");
    std::fs::write(
        dir.join("tools.toml"),
        "config_version = 1\n\n[[tool]]\nid = \"echo.tool\"\nkind = \"shell\"\ncommand = \"cmd.exe\"\
         \nverify = { method = \"none\" }\n",
    )
    .unwrap();

    // A real repository with a clean tree, so the changed-file summary has a baseline to diff against.
    let staged = |args: &[&str]| -> bool {
        std::process::Command::new(&git)
            .args(args)
            .current_dir(&workspace)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    if !staged(&["init", "-q"])
        || !staged(&["add", "already.txt"])
        || !staged(&[
            "-c",
            "user.email=jean@test",
            "-c",
            "user.name=jean",
            "commit",
            "-qm",
            "base",
        ])
    {
        cleanup(&dir);
        return None;
    }

    let target = if escape {
        dir.join("escape.txt")
    } else {
        workspace.join("agent-note.txt")
    };
    let check = format!(
        "process.exit(require('fs').existsSync('{}')?0:1)",
        posix(&target)
    );
    std::fs::write(
        dir.join("jean.toml"),
        format!(
            "[workspace]\nroot = {}\n\n[delegation]\n\
             enabled = true\ndefault_worker = \"fixture\"\n\
             verify_command = {}\nverify_args = [\"-e\", \"{}\"]\n\
             verify_timeout_secs = 60\nsession_timeout_secs = 90\nidle_timeout_secs = 45\n\
             permission_timeout_secs = 15\nshutdown_grace_ms = 500\nmax_event_queue = 64\n\
             max_output_bytes = 262144\nisolate_in_worktree = false\nrequire_clean_baseline = false\n\
             \n[[delegation.workers]]\nname = \"fixture\"\nkind = \"acp\"\ncommand = {}\n\
             args = [{}, {}, \"the agent says it wrote it\"]\ncapability = \"agent.delegate\"\n\
             sandbox = \"workspace-write\"\n",
            toml_path(&workspace),
            toml_path(&runtime),
            check,
            toml_path(&runtime),
            toml_path(&dir.join("agent-acp.js")),
            toml_path(&target),
        ),
    )
    .expect("the configuration must be writable");

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("JEAN_DATA_DIR", &dir);
    let ctx = Context::load(None).expect("a valid data directory must load");
    assert!(
        ctx.config.delegation.enabled,
        "the fixture must be in the worker configuration"
    );
    Some((dir, workspace, target, ctx, guard))
}

#[test]
fn a_live_acp_agent_writes_inside_its_workspace_and_jeans_check_decides() {
    let Some((dir, workspace, target, mut ctx, _guard)) = scenario("inside", false) else {
        eprintln!("skipped: this machine has no node + git pair, so no live ACP agent could run");
        return;
    };
    let report = delegate(
        &mut ctx,
        &DelegateRequest {
            goal: "put a note in the workspace".to_owned(),
            workspace: workspace.clone(),
            worker: None,
        },
        &mut ScriptedPrompt::default(),
    )
    .expect("a delegation that runs to settlement");

    // The workflow, in the order the objective states it.
    assert_eq!(report.worker, "fixture", "the live ACP session ran");
    assert_eq!(
        report.stop_reason.as_str(),
        "end_turn",
        "the agent's own turn ended: {:?}",
        report.notes
    );
    assert!(
        report.events >= 3,
        "started, progress and settlement are all recorded: {}",
        report.events
    );
    assert!(
        report
            .changed_files
            .iter()
            .any(|file| file.ends_with("agent-note.txt")),
        "the file the agent asked for is in the summary JEAN computed: {:?}",
        report.changed_files
    );
    assert!(
        target.is_file(),
        "JEAN performed the write itself, after deciding"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "written by the agent\n",
        "with the agent's content, not a placeholder"
    );
    // Covered by the delegation the operator already approved — so the prompter was never asked.
    assert!(
        report.covered_asks >= 1,
        "an in-workspace edit must not re-ask: {:?}",
        report.notes
    );
    assert_eq!(report.prompted_asks, 0);

    // The verdict is J.E.A.N.'s check, not the agent's sentence.
    assert_eq!(
        report
            .verification
            .as_ref()
            .map(|verdict| verdict.label())
            .unwrap_or("missing"),
        "proven"
    );
    assert_eq!(
        report.result.as_str(),
        "succeeded",
        "claim {:?}, verification {:?}",
        report.claim,
        report.verification
    );
    assert_eq!(report.result.exit_status(), 0);
    assert_eq!(
        ctx.store.require_task(&report.task_id).unwrap().state,
        TaskState::Succeeded
    );
    // The task board keeps the delegation's own word — which folds the check, the stop reason and any
    // connector failure into one thing the operator can act on — while the verification label is a
    // `verification_recorded` event below. A tool run's outcome is its verdict; a delegation's is its
    // result, because "cancelled" and "not proven" are different answers to different questions.
    assert_eq!(
        ctx.store
            .require_task(&report.task_id)
            .unwrap()
            .outcome
            .as_deref(),
        Some("succeeded")
    );
    let events = ctx.store.events(&report.task_id).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.kind == EventKind::VerificationRecorded),
        "the independent check is a task event: {:?}",
        events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>()
    );
    cleanup(&dir);
}

#[test]
fn a_live_agent_that_reaches_outside_its_workspace_is_refused_and_the_result_says_so() {
    let Some((dir, workspace, target, mut ctx, _guard)) = scenario("outside", true) else {
        eprintln!("skipped: this machine has no node + git pair, so no live ACP agent could run");
        return;
    };
    // An empty script: a prompt that reaches the operator would come back as `ScriptExhausted`, and the
    // recorded `asked` list is how this test can tell "refused at the boundary" from "refused by a yes
    // or no somebody typed".
    let mut prompter = ScriptedPrompt::default();
    let report = delegate(
        &mut ctx,
        &DelegateRequest {
            goal: "put a note somewhere else entirely".to_owned(),
            workspace: workspace.clone(),
            worker: None,
        },
        &mut prompter,
    )
    .expect("a refusal inside a session is still a settled delegation");
    assert!(
        prompter.asked.is_empty(),
        "the operator is never asked about a thing they cannot approve by saying yes: {:?}",
        prompter.asked
    );

    assert!(
        !target.exists(),
        "an ask outside the workspace the operator pointed at does not happen"
    );
    // Nothing in the repository changed either, which is what the summary is for.
    assert!(
        report.changed_files.is_empty(),
        "{:?}",
        report.changed_files
    );
    // The agent finished its turn — and J.E.A.N.'s own check is what decides the result.
    assert_eq!(report.stop_reason.as_str(), "end_turn");
    assert_eq!(
        report
            .verification
            .as_ref()
            .map(|verdict| verdict.label())
            .unwrap_or("missing"),
        "not_proven",
        "the claim is not the verdict: {:?}",
        report.claim
    );
    assert_eq!(report.result.as_str(), "failed");
    assert_eq!(
        ctx.store.require_task(&report.task_id).unwrap().state,
        TaskState::Failed
    );
    assert_eq!(
        report.prompted_asks, 1,
        "an escape is refused on the boundary, and the ask is on the record rather than swallowed \
         by the connector"
    );
    assert_eq!(
        report.covered_asks, 0,
        "nothing outside the tree is ever covered by the delegation"
    );
    let audit = std::fs::read_to_string(&ctx.paths.audit).unwrap_or_default();
    assert!(
        audit.contains("outside_scope"),
        "the refusal is recorded as a boundary: {audit}"
    );
    cleanup(&dir);
}
