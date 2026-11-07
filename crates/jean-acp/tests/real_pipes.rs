//! The ACP connector against a real worker process, over real pipes.
//!
//! [`crate::wire_tests`] proves the session loop's timing over a duplex stream, and `jean-core`'s
//! `tests/delegation_live.rs` proves the whole workflow against a Node agent that behaves. What neither
//! can prove is the two things that only a *process* can settle: that the session deadline ends a worker
//! which never answers, and that when it does, nothing of that worker is left running. A duplex stream
//! has no process id, and a well-behaved agent exits on its own — so the case that hung for 221 s in
//! §38, with a live worker still holding the far end of the pipe, needed a live worker to be tested at
//! all. This file is that case.
//!
//! The witness is a **grandchild**, started by the worker after the job object already contains it, and
//! given four seconds to write a file. `run` is allowed two. If the deadline bounds the session and the
//! terminate closes the tree, the file never appears; if either fails, the process that survives writes
//! it and the assertion says so.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jean_acp::contract::{
    Bounds, DelegateRequest, PermissionAnswer, PermissionHost, PermissionRequest, RunSpec, VecSink,
    STOP_TIMEOUT,
};
use jean_process::spec::{LaunchSpec, TerminalMode};
use jean_protocol::config::WorkerConfig;

/// A host with nothing to decide: this worker never gets as far as asking.
struct Unused;

impl PermissionHost for Unused {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionAnswer {
        panic!("a worker that never handshakes asks for nothing, saw {request:?}")
    }
}

/// Where `node` is, or `None` when this machine has none — the same shape as the other live-pipe
/// suites: say so out loud rather than reporting a round trip that never happened.
fn node() -> Option<PathBuf> {
    let out = std::process::Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jean-acp-pipes-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory must be creatable");
    dir
}

/// The minimum environment a Windows child needs to start at all.
///
/// An empty block makes `cmd.exe` fail with `ERROR_INVALID_PARAMETER`, and `node` is not more forgiving
/// about `SystemRoot`; the allowlist that production pins is `jean-core`'s business, and a connector
/// test that started a system binary would have to invent it anyway.
fn child_environment(node: &Path) -> std::collections::BTreeMap<String, String> {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:\\Windows"));
    let system_dir = Path::new(&root).join("System32");
    let mut env = std::collections::BTreeMap::new();
    env.insert("SystemRoot".to_owned(), root.clone());
    env.insert(
        "PATH".to_owned(),
        match node.parent() {
            Some(dir) => format!("{};{};{}", system_dir.display(), root, dir.display()),
            None => format!("{};{}", system_dir.display(), root),
        },
    );
    env.insert(
        "COMSPEC".to_owned(),
        system_dir.join("cmd.exe").to_string_lossy().into_owned(),
    );
    env
}

/// A worker that starts one descendant and then stays silent forever.
///
/// The grandchild — not the worker — is the orphan being watched for: it is created inside the job
/// object, which is exactly the case [`jean_process::piped`] promises to contain (`node` under `npx`,
/// a compiler under a shell). Four seconds is longer than this session's deadline and short enough that
/// a leak is visible before the test finishes waiting.
fn silent_worker_script(witness: &Path) -> String {
    let witness = witness.display().to_string().replace('\\', "/");
    format!(
        "const {{spawn}} = require('child_process');\n\
         const child = spawn(process.execPath, ['-e', 'setTimeout(() => \
         require(\"fs\").writeFileSync(\"{witness}\", \"survived\"), 4000)'], {{ stdio: 'ignore' }});\n\
         child.on('exit', () => process.exit(0));\n\
         setInterval(() => {{}}, 1000);\n"
    )
}

#[test]
fn a_worker_that_never_answers_is_ended_by_its_deadline_and_leaves_no_descendant_running() {
    let Some(runtime) = node() else {
        eprintln!("skipped: this machine has no node, so no real worker process could be started");
        return;
    };
    let dir = scratch("deadline");
    let witness = dir.join("witness.txt");
    let script = dir.join("silent-worker.js");
    std::fs::write(&script, silent_worker_script(&witness))
        .expect("the worker script must be writable");

    let worker = WorkerConfig {
        name: "silent".to_owned(),
        kind: "acp".to_owned(),
        command: runtime.display().to_string(),
        args: vec![script.display().to_string()],
        enabled: true,
        model: String::new(),
        env_passthrough: Vec::new(),
        capability: "agent.delegate".to_owned(),
        sandbox: "workspace-write".to_owned(),
    };
    let launch = LaunchSpec {
        executable: runtime.clone(),
        args: vec![script.display().to_string()],
        working_dir: Some(dir.clone()),
        env: child_environment(&runtime),
        timeout: None,
        output_cap_bytes: 64 * 1024,
        stdout_path: None,
        stderr_path: None,
        terminal: TerminalMode::Pipes,
    };
    let request = DelegateRequest {
        delegation_id: "d-pipes".to_owned(),
        task_id: "t-pipes".to_owned(),
        goal: "say nothing".to_owned(),
        workspace: dir.clone(),
        prompt: "say nothing".to_owned(),
    };
    let deadline = Duration::from_secs(2);
    let bounds = Bounds {
        max_frame_bytes: 64 * 1024,
        max_event_queue: 16,
        session_deadline: Instant::now() + deadline,
        // Far away on purpose: the *deadline* must be what ends this, and an idle signal first would
        // report a different bound for the same stall.
        idle_timeout: Duration::from_secs(60),
        permission_timeout: Duration::from_secs(5),
        shutdown_grace: Duration::from_millis(300),
        max_output_bytes: 64 * 1024,
        output_dir: None,
    };
    let spec = RunSpec {
        worker: &worker,
        launch: &launch,
        request: &request,
        bounds: &bounds,
        cancel: Arc::new(AtomicBool::new(false)),
    };

    let started = Instant::now();
    let mut host = Unused;
    let mut sink = VecSink::default();
    let outcome =
        jean_acp::run(&spec, &mut host, &mut sink).expect("a bounded session still reports");
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.stop_reason, STOP_TIMEOUT,
        "a worker that never handshakes is a timeout, not a mystery: {:?}",
        outcome.failure
    );
    let failure = outcome.failure.expect("a deadline is a reported failure");
    assert!(failure.contains("deadline"), "{failure}");
    assert!(
        outcome.claim.is_none(),
        "a session that was never observed produces no claim: {:?}",
        outcome.claim
    );
    // The bound has to be a bound, not a suggestion: §38 measured 221 s against a 90 s session.
    assert!(
        elapsed < deadline + Duration::from_secs(4),
        "the deadline was {deadline:?} and the session took {elapsed:?}"
    );

    // Long past the grandchild's own four-second timer, which only fires if something survived.
    std::thread::sleep(Duration::from_millis(4600));
    assert!(
        !witness.exists(),
        "an owned descendant outlived the session: {}",
        witness.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
