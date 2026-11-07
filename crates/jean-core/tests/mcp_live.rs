//! Phase 3, MCP over real pipes: a live peer, started by J.E.A.N., answers.
//!
//! [`tests/mcp.rs`](mcp.rs) scripts the peer, which is what makes the *accounting* testable — but it
//! proves nothing about the protocol. This file starts a real process (Node, which is installed on
//! this machine; nothing is downloaded, installed or authenticated), speaks to it with the real
//! `rmcp` client over the pipes `jean-process` made, and checks what came back and what was written
//! down. The handshake, the newline framing, the line ceiling, the result cap and the teardown are
//! the production ones on both ends.
//!
//! If this machine has no `node`, every test here says so out loud and passes without asserting.
//! That is the honest shape of a machine-dependent check: it reports what it could not try rather
//! than pretending the round trip worked.
//!
//! ## Configuration
//!
//! `JEAN_DATA_DIR` is process-global and cargo runs tests in threads, so each scenario holds
//! `ENV_LOCK` for as long as its context lives.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use jean_core::mcp::{call_tool, list_tools};
use jean_core::Context;
use jean_policy::ScriptedPrompt;
use jean_protocol::{EventKind, TaskState, Termination};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn toml_path(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "\\\\"))
}

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jean-mcp-live-{}-{}-{}",
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

/// `node`, if this machine has it on PATH.
fn node() -> Option<PathBuf> {
    jean_core::resolve::resolve_executable(
        "node",
        std::env::var("PATH").ok().as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
    )
    .ok()
}

/// A minimal MCP server: the three calls this file needs, and nothing else.
///
/// Single quotes throughout, because the whole script is one argument on a Windows command line and
/// a TOML basic string besides, and every escape it does not contain is one fewer way to be wrong.
const FIXTURE_SERVER: &str = r#"
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (obj) => process.stdout.write(JSON.stringify(obj) + String.fromCharCode(10));
rl.on('line', (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch (e) { return; }
  if (msg.id === undefined) { return; }
  if (msg.method === 'initialize') {
    send({ jsonrpc: '2.0', id: msg.id, result: {
      protocolVersion: msg.params.protocolVersion,
      capabilities: { tools: {} },
      serverInfo: { name: 'jean-fixture-mcp', version: '1.0.0' } } });
  } else if (msg.method === 'shutdown') {
    send({ jsonrpc: '2.0', id: msg.id, result: {} });
  } else if (msg.method === 'tools/list') {
    const tool = (name, description) => ({ name, description, inputSchema: { type: 'object' } });
    send({ jsonrpc: '2.0', id: msg.id, result: { tools: [
      tool('greet', 'Greets, once.'),
      tool('loud', 'Returns far more text than the configured result cap.'),
      tool('refuse', 'Answers with a tool-level failure.') ] } });
  } else if (msg.method === 'tools/call') {
    const name = msg.params.name;
    if (name === 'greet') {
      send({ jsonrpc: '2.0', id: msg.id, result:
        { content: [{ type: 'text', text: 'hello from the fixture' }] } });
    } else if (name === 'loud') {
      send({ jsonrpc: '2.0', id: msg.id, result:
        { content: [{ type: 'text', text: 'x'.repeat(9000) }] } });
    } else {
      send({ jsonrpc: '2.0', id: msg.id, result:
        { isError: true, content: [{ type: 'text', text: 'the fixture refuses' }] } });
    }
  } else {
    send({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'method not found' } });
  }
});
"#;

/// One scenario: a data directory whose only MCP server is a Node child running `script`.
///
/// Returns `None` when there is no Node to run it, which every test below reports and skips.
fn live(tag: &str, script: &str, mcp: &str) -> Option<(PathBuf, Context, MutexGuard<'static, ()>)> {
    let runtime = node()?;
    let dir = temp_dir(tag);
    std::fs::create_dir_all(dir.join("work")).expect("the data directory must be creatable");
    std::fs::write(
        dir.join("tools.toml"),
        "config_version = 1\n\n[[tool]]\nid = \"echo.tool\"\nkind = \"shell\"\ncommand = \"cmd.exe\"\
         \nverify = { method = \"none\" }\n",
    )
    .unwrap();
    std::fs::write(dir.join("fixture-mcp.js"), FIXTURE_SERVER)
        .expect("the fixture server must be writable");
    let script_path = dir.join(script);

    // `[mcp]` and the one server, written the way the shipped configuration says them.
    let mut mcp_block = String::from(
        "[mcp]\n\
         enabled = true\n\
         request_timeout_secs = 20\n\
         max_message_bytes = 65536\n\
         max_result_bytes = 4096\n\
         shutdown_grace_ms = 1000\n\
         expose = []\n",
    );
    for line in mcp.lines() {
        let name = line.split('=').next().unwrap_or("").trim();
        mcp_block = mcp_block
            .lines()
            .filter(|existing| !existing.starts_with(&format!("{name} =")))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        mcp_block.push_str(line);
        mcp_block.push('\n');
    }

    std::fs::write(
        dir.join("jean.toml"),
        format!(
            "[workspace]\nroot = {}\n{mcp_block}\n[[mcp.servers]]\nname = \"fixture\"\n\
             command = {}\nargs = [{}]\nenabled = true\n\
             allow_tools = [\"greet\", \"loud\", \"refuse\"]\ncapability = \"mcp.call\"\n",
            toml_path(&dir.join("work")),
            toml_path(&runtime),
            toml_path(&script_path)
        ),
    )
    .expect("the configuration must be writable");

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("JEAN_DATA_DIR", &dir);
    let ctx = Context::load(None).expect("a valid data directory must load");
    assert_eq!(ctx.config.mcp.servers.len(), 1);
    Some((dir, ctx, guard))
}

#[test]
fn a_real_handshake_and_call_leave_the_records_the_workflow_promises() {
    let Some((dir, mut ctx, _guard)) = live("roundtrip", "fixture-mcp.js", "") else {
        eprintln!("skipped: no `node` on PATH, so no live MCP peer could be started here");
        return;
    };
    let report = call_tool(
        &mut ctx,
        "fixture",
        "greet",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
    )
    .expect("a configured, allowlisted call on a live server");

    assert_eq!(report.text, "hello from the fixture");
    assert!(!report.tool_error);
    assert_eq!(report.state, TaskState::Succeeded);
    assert_eq!(report.exit_status(), 0);

    let runs = ctx.store.tool_runs(&report.task_id).unwrap();
    assert_eq!(runs.len(), 1);
    let pid = runs[0]
        .child_pid
        .expect("the peer's pid is recorded on the run row");
    assert_ne!(
        pid,
        std::process::id(),
        "that is the server's process, not ours"
    );
    assert_eq!(
        runs[0].termination,
        Some(Termination::Exited),
        "a session J.E.A.N. closed politely leaves a child that exited by itself: {:?}",
        runs[0].error
    );
    let events = ctx.store.events(&report.task_id).unwrap();
    assert!(
        events.iter().any(|event| {
            event.kind == EventKind::McpToolCalled && event.detail.contains("greet")
        }),
        "{:?}",
        events
            .iter()
            .map(|event| event.detail.clone())
            .collect::<Vec<_>>()
    );
    cleanup(&dir);
}

#[test]
fn a_result_bigger_than_the_cap_is_cut_on_the_way_in() {
    let Some((dir, mut ctx, _guard)) = live("cap", "fixture-mcp.js", "") else {
        eprintln!("skipped: no `node` on PATH");
        return;
    };
    let report = call_tool(
        &mut ctx,
        "fixture",
        "loud",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
    )
    .expect("the call succeeded; only its size is a problem");

    // The peer's own byte count is kept, because "it returned 9000 and you saw 4096" is the sentence
    // an operator needs; the cut length is the one a reader of the terminal sees.
    assert_eq!(report.bytes, 9000);
    assert!(report.truncated);
    assert_eq!(report.text.chars().count(), 4096);
    assert_eq!(ctx.config.mcp.max_result_bytes, 4096);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("max_result_bytes")),
        "{:?}",
        report.notes
    );
    cleanup(&dir);
}

#[test]
fn a_live_server_that_refuses_is_recorded_as_its_refusal() {
    let Some((dir, mut ctx, _guard)) = live("refusal", "fixture-mcp.js", "") else {
        eprintln!("skipped: no `node` on PATH");
        return;
    };
    let report = call_tool(
        &mut ctx,
        "fixture",
        "refuse",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
    )
    .expect("a refusal from the tool is an answer, not a transport fault");

    assert!(report.tool_error);
    assert_eq!(report.text, "the fixture refuses");
    assert_eq!(report.state, TaskState::Failed);
    assert_eq!(report.exit_status(), 1);
    let task = ctx.store.require_task(&report.task_id).unwrap();
    assert_eq!(task.outcome.as_deref(), Some("not_proven"));
    cleanup(&dir);
}

#[test]
fn a_live_server_can_be_asked_what_it_offers() {
    let Some((dir, mut ctx, _guard)) = live("list", "fixture-mcp.js", "") else {
        eprintln!("skipped: no `node` on PATH");
        return;
    };
    let listing = list_tools(&mut ctx, "fixture", &mut ScriptedPrompt::default())
        .expect("tools/list on a live server");

    assert_eq!(listing.tools.len(), 3, "{:?}", listing.tools);
    assert!(
        listing.tools.iter().all(|tool| tool.allowed),
        "the fixture publishes exactly what the operator allowed"
    );
    assert!(listing.tools.iter().any(|t| t.name == "greet"));
    assert_eq!(listing.state, TaskState::Succeeded);
    cleanup(&dir);
}

#[test]
fn a_tool_the_operator_did_not_allow_is_refused_without_a_session() {
    // Across the wire this is the same refusal as the scripted one, and it is checked locally: the
    // peer is never started, so a name outside the list cannot even be *offered* to a live server.
    let Some((dir, mut ctx, _guard)) = live("deny-name", "fixture-mcp.js", "") else {
        eprintln!("skipped: no `node` on PATH");
        return;
    };
    let error = call_tool(
        &mut ctx,
        "fixture",
        "delete_everything",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
    )
    .expect_err("not on the allowlist");
    assert!(error.to_string().contains("allow_tools"), "{error}");
    assert_eq!(ctx.store.count("tasks").unwrap(), 0);
    cleanup(&dir);
}

#[test]
fn a_frame_bigger_than_the_line_ceiling_ends_the_session() {
    // `[mcp] max_message_bytes` is handed to the transport, not left at the SDK's 16 MiB default. A
    // peer that answers with a 20 KiB line against a 2 KiB ceiling must be refused, and the child
    // must not be left running because of it.
    let Some((dir, mut ctx, _guard)) = live(
        "oversize",
        "fixture-mcp.js",
        "max_message_bytes = 2048\nrequest_timeout_secs = 10",
    ) else {
        eprintln!("skipped: no `node` on PATH");
        return;
    };
    // The fixture answers normally; the ceiling is what is being tested, so the answer that matters
    // is that *something* over 2048 bytes is refused rather than buffered.
    let report = call_tool(
        &mut ctx,
        "fixture",
        "loud",
        serde_json::json!({}),
        &mut ScriptedPrompt::default(),
    );
    // The strict reading: a 9 KiB answer against a 2 KiB ceiling is not a thing J.E.A.N. accepts, so
    // the command ends with a failure that names the server and a record of the attempt. If this
    // assertion ever starts failing because the call *succeeded*, the configured ceiling has stopped
    // being applied — which is the bug this test exists to catch.
    let error = report.expect_err("a frame over the line ceiling is not an answer");
    let text = error.to_string();
    assert!(text.contains("fixture"), "{text}");
    let task = ctx
        .store
        .list_tasks(jean_store::TaskFilter::All, 10)
        .unwrap()
        .remove(0);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.outcome.as_deref(), Some("error"));
    let runs = ctx.store.tool_runs(&task.id).unwrap();
    assert!(
        runs[0].error.is_some(),
        "the run row says the session was stopped, not that it is still running"
    );
    cleanup(&dir);
}
