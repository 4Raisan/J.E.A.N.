//! `jean mcp serve`, end to end: a real client on the other end of a real pipe pair.
//!
//! The other Phase-3 files script one side. This one spawns the **real `jean.exe`** as an MCP server
//! and talks to it with hand-written newline-delimited JSON-RPC on its stdin, reading stdout for the
//! answers. That is the arrangement a client actually uses, and it is the only way to show the four
//! things this file asserts:
//!
//! * the handshake completes and `tools/list` returns only what `[mcp] expose` named;
//! * a call runs through the ordinary pipeline — policy, the child, the declared verification — and
//!   the answer J.E.A.N. gives is what **it observed**, including the captured output;
//! * a tool that was not published is refused by name, and the host is never reached;
//! * nothing that belongs to a person's terminal ever appears on stdout: the human-facing lines
//!   J.E.A.N. prints (warnings, the session summary) are on stderr, because a log line in the
//!   transport is a malformed frame.
//!
//! Each test uses a throwaway `%TEMP%` directory and starts nothing outside it.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const TOOLS: &str = r#"
config_version = 1

[[tool]]
id = "demo.echo"
kind = "shell"
display_name = "Demo echo"
capabilities = ["shell"]
command = "cmd.exe"
args = ["/c", "echo JEAN_MCP_OK"]
tier = "read_only"
timeout_secs = 30
verify = { method = "output_contains", expected = 0, needle = "JEAN_MCP_OK" }

[[tool]]
id = "demo.hidden"
kind = "shell"
display_name = "Never published"
capabilities = ["shell"]
command = "cmd.exe"
args = ["/c", "echo MUST_NOT_RUN"]
tier = "read_only"
timeout_secs = 30
verify = { method = "exit_code", expected = 0 }
"#;

/// A live `jean mcp serve`, with its transport pipes open.
struct Session {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    /// Every line the server writes, as it arrives.
    lines: mpsc::Receiver<String>,
    dir: PathBuf,
}

impl Session {
    /// A scratch data directory whose `jean.toml` carries `extra` (another table, written before
    /// `[mcp]`), `mcp` (the `[mcp]` keys this scenario overrides) and the published tool list.
    fn start(tag: &str, mcp: &str, extra: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("jean-cli-mcp-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch data directory");
        fs::write(dir.join("tools.toml"), TOOLS).expect("write tools.toml");
        fs::write(
            dir.join("jean.toml"),
            format!(
                "config_version = 1\n\n[core]\ndata_dir = \"{}\"\n\n[tools]\nregistry_path = \"{}\"\
                 \n\n{extra}[mcp]\nenabled = true\nrequest_timeout_secs = 20\n{mcp}\n",
                dir.display().to_string().replace('\\', "/"),
                dir.join("tools.toml").display().to_string().replace('\\', "/"),
            ),
        )
        .expect("write jean.toml");

        let mut child = Command::new(env!("CARGO_BIN_EXE_jean"))
            .current_dir(&dir)
            .env("JEAN_DATA_DIR", &dir)
            .args(["mcp", "serve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("`jean.exe` must be spawnable — cargo just built it");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        // A reader thread, so a server that never answers is a failing test rather than a hanging one.
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            while let Ok(read) = reader.read_line(&mut line) {
                if read == 0 {
                    break;
                }
                let _ = sender.send(line.trim().to_owned());
                line.clear();
            }
        });

        Self {
            child: Some(child),
            stdin: Some(stdin),
            lines: receiver,
            dir,
        }
    }

    fn send(&mut self, frame: &str) {
        let stdin = self.stdin.as_mut().expect("the transport is still open");
        writeln!(stdin, "{frame}").expect("write to the transport");
        stdin.flush().expect("flush the transport");
    }

    /// The next frame that answers `id`, within the deadline.
    fn answer(&self, id: u32) -> String {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(60))
                .expect("the server must answer within the deadline");
            // Anything else on stdout is a bug, and asserting it here is how that bug becomes a
            // sentence instead of a timeout.
            assert!(
                line.contains("\"jsonrpc\""),
                "stdout carried something that is not an MCP frame: {line}"
            );
            if line.contains(&format!("\"id\":{id}")) {
                return line;
            }
        }
    }

    /// Initialize, and return the handshake frame.
    fn handshake(&mut self) -> String {
        self.send(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test-client","version":"1"}}}"#,
        );
        let reply = self.answer(0);
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        reply
    }

    /// Close the transport, as a real client does when it disconnects, and collect stderr.
    ///
    /// `&mut`, not `self`: the audit trail and the database are read *after* the session ends, and
    /// they live in the directory this handle owns.
    fn close(&mut self) -> (i32, String) {
        let stdin = self.stdin.take();
        drop(stdin);
        let child = self.child.take().expect("a session is closed once");
        let output = child.wait_with_output().expect("wait for jean.exe");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn audit(&self) -> String {
        fs::read_to_string(self.dir.join("audit.jsonl")).unwrap_or_default()
    }

    /// Open the database the server wrote, from outside its own process.
    fn store(&self) -> jean_store::Store {
        jean_store::Store::open(
            &self.dir.join("jean.db"),
            "wal",
            Duration::from_millis(2000),
        )
        .expect("open the database the server wrote")
    }
}

fn call(id: u32, tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}}}}"#
    )
}

#[test]
fn a_client_that_completes_the_handshake_is_offered_only_what_was_published() {
    let mut session = Session::start("list", r#"expose = ["demo.echo"]"#, "");
    let handshake = session.handshake();
    assert!(handshake.contains("\"serverInfo\""), "{handshake}");
    assert!(handshake.contains("jean"), "{handshake}");

    session.send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#);
    let listed = session.answer(1);
    assert!(listed.contains("demo.echo"), "{listed}");
    assert!(
        !listed.contains("demo.hidden"),
        "a tool nobody published must not be advertised: {listed}"
    );
    // The schema every exposed tool advertises is the one J.E.A.N. can actually honour.
    assert!(listed.contains("\"args\""), "{listed}");

    let (code, stderr) = session.close();
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stderr.contains("session closed"),
        "the operator's line belongs on stderr, not in the transport: {stderr}"
    );
}

#[test]
fn an_exposed_tool_runs_through_the_pipeline_and_answers_with_what_jean_observed() {
    let mut session = Session::start("call", r#"expose = ["demo.echo"]"#, "");
    session.handshake();
    session.send(&call(2, "demo.echo", "{}"));
    let answer = session.answer(2);

    assert!(
        !answer.contains("\"isError\":true"),
        "the call was allowed and the tool succeeded: {answer}"
    );
    // Two separate facts, both J.E.A.N.'s own: the verdict its declared verification produced, and
    // the output it captured. A client reading this gets an observation, not the tool's claim.
    assert!(answer.contains("proven"), "{answer}");
    assert!(answer.contains("JEAN_MCP_OK"), "{answer}");

    let (code, stderr) = session.close();
    assert_eq!(code, 0, "stderr: {stderr}");
    let audit = session.audit();
    assert!(audit.contains("run_command"), "{audit}");
    assert!(audit.contains("\"decision\":\"allowed\""), "{audit}");

    // The task and the child it started are in the database, which is the point of running the call
    // through the ordinary pipeline rather than around it.
    let tasks = session
        .store()
        .list_tasks(jean_store::TaskFilter::All, 10)
        .expect("list tasks");
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    assert_eq!(tasks[0].tool_id.as_deref(), Some("demo.echo"));
    let runs = session.store().tool_runs(&tasks[0].id).expect("tool runs");
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0].child_pid.is_some(),
        "the child J.E.A.N. started is recorded"
    );
}

#[test]
fn a_tool_that_was_not_published_is_refused_by_name_and_never_starts() {
    let mut session = Session::start("unpublished", r#"expose = ["demo.echo"]"#, "");
    session.handshake();
    session.send(&call(3, "demo.hidden", "{}"));
    let refusal = session.answer(3);

    assert!(refusal.contains("\"isError\":true"), "{refusal}");
    assert!(refusal.contains("not exposed"), "{refusal}");
    let (code, stderr) = session.close();
    assert_eq!(code, 0, "stderr: {stderr}");
    // Refused by the router, so nothing ran and nothing was recorded as a task.
    assert_eq!(session.store().count("tasks").expect("count"), 0);
    assert!(
        !session.audit().contains("MUST_NOT_RUN"),
        "the refusal must not have reached the tool: {}",
        session.audit()
    );
}

#[test]
fn arguments_that_are_not_the_published_shape_are_refused_before_the_tool() {
    // The schema says `args`. A client that sends `{"path": "x"}` would otherwise have its payload
    // quietly dropped, and a tool that ran with the wrong arguments looks exactly like one that
    // worked.
    let mut session = Session::start("shape", r#"expose = ["demo.echo"]"#, "");
    session.handshake();
    session.send(&call(4, "demo.echo", r#"{"path":"README.md"}"#));
    let refusal = session.answer(4);
    assert!(refusal.contains("\"isError\":true"), "{refusal}");
    assert!(refusal.contains("unknown argument key"), "{refusal}");
    assert_eq!(session.store().count("tasks").expect("count"), 0);
    let (code, stderr) = session.close();
    assert_eq!(code, 0, "stderr: {stderr}");
}

#[test]
fn a_call_that_would_need_a_person_is_refused_and_says_what_would_help() {
    // `jean mcp serve` has no terminal: stdin and stdout *are* the transport, so a prompt could not
    // be answered without corrupting the session it belongs to. Refusing is the only honest answer,
    // and the refusal has to tell the operator how to make it an allow.
    let mut session = Session::start(
        "no-terminal",
        r#"expose = ["demo.echo"]"#,
        // `shell` is what `demo.echo` declares, so every call on it now needs a person.
        "[policy]\nrequire_approval = [\"shell\"]\n",
    );
    session.handshake();
    session.send(&call(6, "demo.echo", "{}"));
    let refusal = session.answer(6);

    assert!(refusal.contains("\"isError\":true"), "{refusal}");
    assert!(
        refusal.contains("no one was asked"),
        "the refusal must explain itself: {refusal}"
    );
    assert!(
        !refusal.contains("JEAN_MCP_OK"),
        "a capability that needs a person does not run anyway: {refusal}"
    );
    let (code, stderr) = session.close();
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stderr.contains("an approval was needed"),
        "the question is written down, in the space the protocol never uses: {stderr}"
    );
    assert!(
        session
            .audit()
            .contains("\"denial_reason\":\"unanswerable\""),
        "the ledger says why: {}",
        session.audit()
    );
}
