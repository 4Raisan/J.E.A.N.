//! The ACP session loop, driven over real bytes with no process on either end.
//!
//! These are the tests §38 asked for. Before them, `session()` — the function that handshakes,
//! prompts, records updates, answers the agent's own `fs/write_text_file` and decides when the turn is
//! over — had never been run against *any* peer. The unit tests in `acp.rs` check the pure decisions
//! (`permission_outcome`, the workspace containment rules, `map_stop`) and the in-memory `Script` in
//! [`crate::fake`] calls the host and the sink directly, so the protocol between them was unproven, and
//! the one time it did run — against a real Node agent — it hung for 221 s on a 90 s session and
//! nothing in the suite noticed.
//!
//! What sits here is a byte-level ACP peer on the far side of a `tokio::io::duplex`, so a frame that
//! never arrives, a handler that never returns and a deadline that never fires become assertions
//! rather than an afternoon of `--nocapture`.
//!
//! The cases are the ones the brief named:
//!
//! * an agent that asks the client to write, gets the answer, and then **ends its turn** — settlement;
//! * that same ask pointed outside the workspace, refused by the connector although the host said yes;
//! * an agent that asks, is answered, and goes silent — the **deadline**, not a hope;
//! * a handler that cannot finish because the peer stopped reading, with a cancel arriving **during** it;
//! * the same stalled peer left to its own devices, which must end on its **idle limit**;
//! * a host whose own decision blocks past the deadline, which the **deadline** still outranks.
//!
//! Nothing here spawns a process, so "no orphaned descendants" is not this module's claim to make, and
//! neither is "the transport's own teardown": those belong to [`crate::watchdog`]'s in-source tests over
//! real job objects and to `tests/real_pipes.rs`, which starts a worker that never answers.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use agent_client_protocol::ByteStreams;

use crate::acp::{session, Flow, SessionEnd};
use crate::frames::{BoundedRead, BoundedWrite, FrameCount, Heartbeat, WireTrace};

use crate::contract::{
    Bounds, DelegateRequest, PermissionAnswer, PermissionHost, PermissionRequest, RunSpec, VecSink,
    WorkerEvent, STOP_CANCELLED, STOP_END_TURN, STOP_FAILED,
};
use crate::watchdog::{session_channel, Signal};
use jean_process::spec::{LaunchSpec, TerminalMode};
use jean_protocol::config::WorkerConfig;

/// The two hosts a session can be handed: one that answers at once, and one that takes its time.
///
/// The refusal paths are decided by `write_text_file`'s own checks and by `jean-core`'s authority, and
/// both are tested where they live, so the wire needs the *answered* branch. `Slow` exists for the
/// hazard that branch carries: [`PermissionHost::decide`] is synchronous, production implements it over
/// a console prompt, and a host that does not return takes the session's whole thread with it. That is
/// worth a test rather than a footnote, because it says when the deadline is honoured late.
enum Host {
    Allow,
    Slow(Duration),
}

impl PermissionHost for Host {
    fn decide(&mut self, _request: &PermissionRequest) -> PermissionAnswer {
        if let Self::Slow(long) = self {
            std::thread::sleep(*long);
        }
        PermissionAnswer::Selected {
            option_id: "allow_once".to_owned(),
        }
    }
}

/// A scratch workspace with the directory actually created.
///
/// `resolve_target` canonicalizes the root, so a root that does not exist is not a workspace and
/// every write would be refused for a reason that has nothing to do with the test.
struct Wire {
    root: PathBuf,
    worker_name: &'static str,
    model: String,
}

impl Wire {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("jean-acp-wire-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a scratch workspace must be creatable");
        Self {
            root,
            worker_name: "wire",
            model: String::new(),
        }
    }

    /// The absolute path a test expects JEAN to have written.
    fn written(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What one driven session left behind.
struct Observed {
    outcome: std::result::Result<Flow, SessionEnd>,
    events: Vec<WorkerEvent>,
    elapsed: Duration,
}

/// Run one session against `agent`, wired the way [`crate::acp::run`] wires it.
///
/// `capacity` is the duplex buffer. 64 KiB is invisible; 16 bytes is a peer that has stopped reading,
/// which is how a test makes *the transport itself* back up instead of pretending to.
///
/// `agent` is handed its side of the pipe and returns a future; it runs as a spawned task (`Send`, as
/// `tokio::spawn` requires) while the session is awaited in place. That asymmetry is not a shortcut:
/// the session's future owns `Rc`s and is deliberately not `Send`, exactly as in production, where it
/// lives on the worker's thread and nowhere else.
///
/// `cancel` is an optional signal fired at the watchdog's own delay, which is how a test asks whether
/// a cancel that arrives *while a handler is in flight* is honoured or merely queued.
#[allow(clippy::too_many_arguments)]
fn drive<A>(
    wire: &Wire,
    host: Host,
    idle_timeout: Duration,
    permission_window: Duration,
    deadline: Duration,
    capacity: usize,
    agent: A,
    cancel: Option<(Duration, Signal)>,
) -> Observed
where
    A: FnOnce(DuplexStream) -> futures::future::BoxFuture<'static, ()> + Send + 'static,
{
    let worker = WorkerConfig {
        name: wire.worker_name.to_owned(),
        kind: "acp".to_owned(),
        command: "unused".to_owned(),
        args: Vec::new(),
        enabled: true,
        model: wire.model.clone(),
        free_fallback: false,
        env_passthrough: Vec::new(),
        capability: "agent.delegate".to_owned(),
        sandbox: "workspace-write".to_owned(),
    };
    let launch = LaunchSpec {
        executable: PathBuf::from("unused"),
        args: Vec::new(),
        working_dir: None,
        env: Default::default(),
        timeout: None,
        output_cap_bytes: 4096,
        stdout_path: None,
        stderr_path: None,
        terminal: TerminalMode::Pipes,
    };
    let request = DelegateRequest {
        delegation_id: "d-wire".to_owned(),
        task_id: "t-wire".to_owned(),
        goal: "write the note".to_owned(),
        workspace: wire.root.clone(),
        prompt: "write the note".to_owned(),
    };
    let bounds = Bounds {
        max_frame_bytes: 64 * 1024,
        max_event_queue: 16,
        session_deadline: Instant::now() + deadline,
        // Deliberately far away: every case here must be ended by the *deadline* or the *permission
        // window*, and an idle timeout firing first would report a different failure for one stall.
        idle_timeout,
        permission_timeout: permission_window,
        shutdown_grace: Duration::from_millis(50),
        max_output_bytes: 64 * 1024,
        output_dir: None,
    };
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let spec = RunSpec {
        worker: &worker,
        launch: &launch,
        request: &request,
        bounds: &bounds,
        cancel: cancel_flag.clone(),
    };

    let (client, peer) = tokio::io::duplex(capacity);
    let (client_read, client_write) = tokio::io::split(client);
    // Wrapped exactly as `run` wraps a worker's pipes — frame cap, heartbeat and the optional trace.
    // A harness that skipped this would be testing the SDK against a socket, not J.E.A.N. against a
    // worker, and it would silently exempt the transport from the cap every real session enforces.
    let trace = WireTrace::from_env();
    let outgoing = BoundedWrite::new(client_write.compat_write(), bounds.max_frame_bytes);
    let heartbeat = Heartbeat::new();
    let incoming = BoundedRead::new(
        client_read.compat(),
        bounds.max_frame_bytes,
        FrameCount::new(),
    )
    .with_heartbeat(heartbeat);
    let (outgoing, incoming) = match &trace {
        Some(trace) => (
            outgoing.with_trace(trace.clone()),
            incoming.with_trace(trace.clone()),
        ),
        None => (outgoing, incoming),
    };
    let transport = ByteStreams::new(outgoing, incoming);
    let (tx, signals) = session_channel();
    let host = Rc::new(Mutex::new(host));
    let sink = Rc::new(Mutex::new(VecSink::default()));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a current-thread runtime must build");
    let session_sink = sink.clone();
    let started = Instant::now();
    let outcome = runtime.block_on(async move {
        let _agent = tokio::spawn(agent(peer));
        if let Some((after, signal)) = cancel {
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                let _ = tx.send(signal);
            });
        }
        session(transport, host, session_sink, &spec, signals, trace).await
    });
    let elapsed = started.elapsed();
    let events = sink
        .lock()
        .map(|sink| sink.events.clone())
        .unwrap_or_default();
    let _ = cancel_flag.load(Ordering::Relaxed);
    Observed {
        outcome,
        events,
        elapsed,
    }
}

/// The next frame from J.E.A.N., one byte at a time.
///
/// Byte-wise rather than `read_line` because a test must be able to say *which* frame it was waiting
/// for when the connector sent nothing: `read_line` returns an empty buffer at EOF and reads like a
/// timeout, and a suite that fails with "expected a frame" is a suite nobody can debug.
async fn recv(peer: &mut DuplexStream) -> serde_json::Value {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = peer
            .read(&mut byte)
            .await
            .expect("the transport must stay readable");
        if read == 0 {
            panic!(
                "J.E.A.N. closed the transport before sending another frame; read so far: \
                 {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    let text = String::from_utf8(bytes).expect("a UTF-8 frame");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not a JSON-RPC frame ({e}): {text}"))
}

/// One frame out, terminated. ACP frames are newline-delimited and the SDK reads them with
/// `BufReader::lines()`, so a frame without its terminator is a frame that was never sent.
async fn send(peer: &mut DuplexStream, frame: String) {
    peer.write_all(frame.as_bytes())
        .await
        .expect("the peer must accept a frame");
    peer.write_all(b"\n").await.expect("the terminator");
    peer.flush().await.expect("a flushed frame");
}

/// Handshake, open the session, and stop at the prompt — the part every case shares.
///
/// Returns the prompt's id, which is what a well-behaved agent must answer to settle the turn.
async fn handshake(peer: &mut DuplexStream) -> serde_json::Value {
    open_session(peer, serde_json::json!({"sessionId":"s-1"})).await;
    let prompt = recv(peer).await;
    assert_eq!(prompt["method"], "session/prompt");
    prompt["id"].clone()
}

async fn open_session(peer: &mut DuplexStream, result: serde_json::Value) {
    let initialize = recv(peer).await;
    assert_eq!(
        initialize["method"].as_str(),
        Some("initialize"),
        "the first frame must be the handshake"
    );
    send(
        peer,
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"protocolVersion\":1,\
             \"agentCapabilities\":{{}}}}}}",
            initialize["id"]
        ),
    )
    .await;

    let new_session = recv(peer).await;
    assert_eq!(
        new_session["method"].as_str(),
        Some("session/new"),
        "the second frame must open a session"
    );
    send(
        peer,
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{result}}}",
            new_session["id"],
        ),
    )
    .await;
}

#[test]
fn opencode_model_selection() {
    use serde_json::json;
    const MODEL: &str = "opencode/beam-free";
    for (tag, current) in [
        ("match", Some(MODEL)),
        ("different", Some("opencode/other")),
        ("missing", None),
        ("rejected", None),
    ] {
        let mut wire = Wire::new(tag);
        wire.worker_name = "opencode";
        wire.model = MODEL.into();
        let observed = drive(
            &wire,
            Host::Allow,
            Duration::from_secs(30),
            Duration::from_secs(1),
            Duration::from_secs(3),
            64 * 1024,
            move |mut peer| {
                Box::pin(async move {
                    let option = |value| {
                        json!({
                            "id":"model","name":"Model","category":"model","type":"select",
                            "currentValue":value,"options":[{"value":MODEL,"name":"Beam Free"}]
                        })
                    };
                    open_session(
                        &mut peer,
                        json!({
                            "sessionId":"s-1","configOptions":[option("opencode/other")]
                        }),
                    )
                    .await;
                    let request = recv(&mut peer).await;
                    assert_eq!(request["method"], "session/set_config_option");
                    assert_eq!(
                        request["params"],
                        json!({
                            "sessionId":"s-1","configId":"model","value":MODEL
                        })
                    );
                    let mut reply = json!({"jsonrpc":"2.0","id":request["id"]});
                    if tag == "rejected" {
                        reply["error"] = json!({"code":-32602,"message":"model rejected"});
                    } else {
                        reply["result"] = json!({"configOptions":current.map(option).into_iter().collect::<Vec<_>>()});
                    }
                    send(&mut peer, reply.to_string()).await;
                    // If the client sends a prompt in any case, settle it. A negative
                    // case would then report success (or expire), failing the assertion below.
                    let mut line = String::new();
                    use tokio::io::AsyncBufReadExt;
                    if tokio::io::BufReader::new(&mut peer)
                        .read_line(&mut line)
                        .await
                        .unwrap()
                        > 0
                    {
                        let prompt: serde_json::Value = serde_json::from_str(&line).unwrap();
                        assert_eq!(prompt["method"], "session/prompt");
                        send(
                            &mut peer,
                            json!({
                                "jsonrpc":"2.0","id":prompt["id"],"result":{"stopReason":"end_turn"}
                            })
                            .to_string(),
                        )
                        .await;
                        hold(&mut peer).await;
                    }
                })
            },
            None,
        );
        let confirmations: Vec<_> = observed
            .events
            .iter()
            .filter_map(|event| match event {
                WorkerEvent::ModelConfirmed { model } => Some(model.as_str()),
                _ => None,
            })
            .collect();
        if tag == "match" {
            assert_eq!(confirmations, [MODEL]);
            assert!(observed.outcome.is_ok(), "{:?}", observed.outcome);
        } else {
            assert!(confirmations.is_empty(), "{tag}: {confirmations:?}");
            let Err(SessionEnd::Abnormal(reason)) = observed.outcome else {
                panic!("{tag}: expected pre-prompt refusal");
            };
            assert!(
                reason.contains(if tag == "rejected" {
                    "model rejected"
                } else {
                    "did not confirm requested model"
                }),
                "{tag}: {reason}"
            );
        }
    }
}

#[test]
fn opencode_blank_model_is_refused_without_sending_any_acp_prompt() {
    for model in ["", " \t "] {
        let mut wire = Wire::new("blank-opencode-model");
        wire.worker_name = "opencode";
        wire.model = model.to_owned();
        let observed = drive(
            &wire,
            Host::Allow,
            Duration::from_secs(30),
            Duration::from_secs(1),
            Duration::from_secs(3),
            64 * 1024,
            |mut peer| {
                Box::pin(async move {
                    let mut line = String::new();
                    use tokio::io::AsyncBufReadExt;
                    let bytes = tokio::io::BufReader::new(&mut peer)
                        .read_line(&mut line)
                        .await
                        .unwrap();
                    assert_eq!(
                        bytes, 0,
                        "blank model must not start or prompt an ACP worker"
                    );
                })
            },
            None,
        );
        let Err(SessionEnd::Abnormal(reason)) = observed.outcome else {
            panic!("expected OpenCode model config refusal for {model:?}");
        };
        assert!(reason.contains("[delegation.workers].model"), "{reason}");
    }
}

/// The agent's streamed message and its `fs/write_text_file` ask, then J.E.A.N.'s answer to it.
async fn ask_for_a_write(peer: &mut DuplexStream, path: &Path, content: &str) -> serde_json::Value {
    send(
        peer,
        "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s-1\",\
         \"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\
         \"text\":\"writing the file now\"}}}}"
            .to_owned(),
    )
    .await;
    send(
        peer,
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"w-1\",\"method\":\"fs/write_text_file\",\"params\":\
             {{\"sessionId\":\"s-1\",\"path\":{:?},\"content\":{:?}}}}}",
            path.to_string_lossy(),
            content
        ),
    )
    .await;
    recv(peer).await
}

/// A peer that has finished speaking must not hang up: dropping its end closes the transport, and the
/// session would then end on an EOF rather than on the turn it was asked to end.
async fn hold(peer: &mut DuplexStream) {
    let mut sink = [0u8; 1];
    let _ = peer.read(&mut sink).await;
}

#[test]
fn an_agent_that_asks_the_client_to_write_can_then_end_its_turn() {
    // §38's missing case, and the one thing the live run proved by hand: the agent asks, the client
    // answers, and *then* the agent finishes. Settlement after a client-performed request had never
    // been exercised — not by `fake.rs`, which never sends one, and not by the ignored live test, which
    // hung.
    let wire = Wire::new("settle");
    let target = wire.written("agent-note.txt");
    let expect = target.clone();
    let observed = drive(
        &wire,
        Host::Allow,
        Duration::from_secs(600),
        Duration::from_secs(10),
        Duration::from_secs(30),
        64 * 1024,
        move |mut peer| {
            Box::pin(async move {
                let prompt_id = handshake(&mut peer).await;
                let answer = ask_for_a_write(&mut peer, &expect, "written by the agent\n").await;
                assert!(
                    answer.get("error").is_none(),
                    "the client must answer a permitted write, not error it: {answer}"
                );
                send(
                    &mut peer,
                    format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"stopReason\":\"end_turn\"}}}}",
                        prompt_id
                    ),
                )
                .await;
                hold(&mut peer).await;
            })
        },
        None,
    );

    let flow = observed
        .outcome
        .as_ref()
        .unwrap_or_else(|end| panic!("the session did not settle: {end:?}"));
    assert_eq!(flow.stop_reason, STOP_END_TURN, "{flow:?}");
    assert_eq!(
        flow.failure, None,
        "a settled turn carries no connector failure"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("J.E.A.N. performs the write itself"),
        "written by the agent\n"
    );
    assert!(
        observed
            .events
            .iter()
            .any(|event| matches!(event, WorkerEvent::Progress { .. })),
        "the agent's streamed message belongs on the task board"
    );
    assert!(
        observed
            .events
            .iter()
            .any(|event| matches!(event, WorkerEvent::PermissionAsked { .. })),
        "a client-performed write is decided, and the decision is recorded"
    );
}

#[test]
fn a_write_outside_the_workspace_is_refused_even_when_the_host_says_yes() {
    // The connector is the last gate. In production the authority refuses this ask itself and records
    // the boundary — `jean-core`'s `delegation_live` scenario checks that — but a host that answered
    // yes to everything must still not be the way out of the tree the operator pointed at, and the ask
    // has to *reach* the host, which is what makes the refusal auditable rather than invisible.
    let wire = Wire::new("outside");
    let escape = std::env::temp_dir().join(format!(
        "jean-acp-escape-{}-outside.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&escape);
    let expect = escape.clone();
    let observed = drive(
        &wire,
        Host::Allow,
        Duration::from_secs(600),
        Duration::from_secs(10),
        Duration::from_secs(30),
        64 * 1024,
        move |mut peer| {
            Box::pin(async move {
                let prompt_id = handshake(&mut peer).await;
                let answer =
                    ask_for_a_write(&mut peer, &expect, "written where it should not be\n").await;
                assert!(
                    answer.get("error").is_some(),
                    "an escape is answered with a typed error, not left hanging: {answer}"
                );
                send(
                    &mut peer,
                    format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"stopReason\":\"end_turn\"}}}}",
                        prompt_id
                    ),
                )
                .await;
                hold(&mut peer).await;
            })
        },
        None,
    );

    let flow = observed
        .outcome
        .as_ref()
        .unwrap_or_else(|end| panic!("a refusal inside a turn is still a settled turn: {end:?}"));
    assert_eq!(flow.stop_reason, STOP_END_TURN, "{flow:?}");
    assert!(
        !escape.exists(),
        "the refused write did not happen: {}",
        escape.display()
    );
    let asked = observed
        .events
        .iter()
        .find_map(|event| match event {
            WorkerEvent::PermissionAsked { answer, .. } => Some(answer.clone()),
            _ => None,
        })
        .expect("the escape reaches the host, so the refusal is somebody's decision on the record");
    assert!(
        asked.is_yes(),
        "this host said yes, and yes is not enough: {asked:?}"
    );
}

#[test]
fn a_session_that_goes_silent_after_an_answered_request_hits_its_deadline() {
    // The live run's exact shape: handshake, prompt, an ask the client answers, and then nothing. The
    // session must end on its deadline, not on the peer's goodwill — and it must not pretend the turn
    // was observed.
    let wire = Wire::new("deadline");
    let target = wire.written("agent-note.txt");
    let expect = target.clone();
    let observed = drive(
        &wire,
        Host::Allow,
        Duration::from_secs(600),
        Duration::from_secs(10),
        Duration::from_millis(1_200),
        64 * 1024,
        move |mut peer| {
            Box::pin(async move {
                let _prompt_id = handshake(&mut peer).await;
                ask_for_a_write(&mut peer, &expect, "written, and then I stopped talking\n").await;
                // Silent forever. This is the peer the 90-second session was waiting for.
                tokio::time::sleep(Duration::from_secs(60)).await;
            })
        },
        None,
    );

    assert!(
        matches!(observed.outcome, Err(SessionEnd::Expired)),
        "a silent peer must be ended by the deadline, not awaited: {:?}",
        observed.outcome
    );
    assert!(
        observed.elapsed < Duration::from_secs(6),
        "the deadline has to actually be the deadline: it waited {:?}",
        observed.elapsed
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("the write must still be readable"),
        "written, and then I stopped talking\n",
        "the write the agent asked for did happen, and is not undone by the timeout"
    );
}

#[test]
fn a_cancel_arrives_while_a_handler_is_still_running() {
    // A peer that stops reading backs the transport up, and the session has to be able to end while it
    // is still mid-answer. This is the cancellation-responsiveness case, and the 16-byte buffer is what
    // makes it a real one rather than a story about one.
    let wire = Wire::new("cancel-in-flight");
    let target = wire.written("note.txt");
    let expect = target.clone();
    let observed = drive(
        &wire,
        Host::Allow,
        Duration::from_secs(600),
        Duration::from_secs(30),
        Duration::from_secs(30),
        16,
        move |mut peer| {
            Box::pin(async move {
                let _prompt_id = handshake(&mut peer).await;
                // Send the ask, and never read the answer.
                let frame = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":\"w-1\",\"method\":\"fs/write_text_file\",\
                     \"params\":{{\"sessionId\":\"s-1\",\"path\":{:?},\"content\":{:?}}}}}\n",
                    expect.to_string_lossy(),
                    "cancel me while I am answering\n"
                );
                let _ = peer.write_all(frame.as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(60)).await;
            })
        },
        Some((Duration::from_millis(300), Signal::Cancelled)),
    );

    assert!(
        observed.outcome.is_ok(),
        "a cancel is a settlement, not a transport failure: {:?}",
        observed.outcome
    );
    let flow = observed.outcome.as_ref().expect("settled by the cancel");
    assert_eq!(flow.stop_reason, STOP_CANCELLED, "{flow:?}");
    assert!(
        observed.elapsed < Duration::from_secs(10),
        "the cancel was honoured while the session was waiting, not after it: {:?}",
        observed.elapsed
    );
}

#[test]
fn a_peer_that_stops_talking_is_ended_by_its_idle_limit() {
    // The answered request is behind us; what is left is a peer that will not speak again. In
    // production the watchdog notices the silence and sends [`Signal::Idle`] — it is tested over real
    // jobs in `jean-process`'s `tests/supervision.rs` — so the session's obligation here is to *end*
    // on that signal and to
    // report it as a stalled worker rather than a slow one. Those are different settings, and the
    // operator's report has to keep them apart.
    let wire = Wire::new("idle");
    let target = wire.written("note.txt");
    let expect = target.clone();
    let observed = drive(
        &wire,
        Host::Allow,
        Duration::from_millis(400),
        Duration::from_secs(30),
        Duration::from_secs(30),
        16,
        move |mut peer| {
            Box::pin(async move {
                let _prompt_id = handshake(&mut peer).await;
                let _ = ask_for_a_write(
                    &mut peer,
                    &expect,
                    "answered, and then I stopped
",
                )
                .await;
                tokio::time::sleep(Duration::from_secs(60)).await;
            })
        },
        Some((Duration::from_millis(400), Signal::Idle)),
    );

    let flow = observed
        .outcome
        .as_ref()
        .unwrap_or_else(|end| panic!("an idle signal ends the session as a report: {end:?}"));
    assert_eq!(flow.stop_reason, STOP_FAILED, "{flow:?}");
    assert!(
        flow.failure
            .as_deref()
            .unwrap_or_default()
            .contains("silent"),
        "the report names the stall: {:?}",
        flow.failure
    );
    assert!(
        observed.elapsed < Duration::from_secs(8),
        "it ended on the idle limit, not the deadline: {:?}",
        observed.elapsed
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("the answered write stands"),
        "answered, and then I stopped
"
    );
}

#[test]
fn a_host_that_blocks_past_the_deadline_is_still_bounded_by_it() {
    // The one bound this connector cannot impose on itself: `PermissionHost::decide` is synchronous, so
    // a host that parks the thread — in production, an operator who walks away from a prompt — parks the
    // session's whole runtime with it, and a timer cannot fire on a thread that is not at an await
    // point. `jean-core`'s authority refuses to ask when less than the permission window is left, which
    // is the real guarantee; what this test pins is what happens when something ignores that, because
    // "late, then bounded" is a different claim from "never bounded", and it used to be the latter.
    let wire = Wire::new("slow-host");
    let target = wire.written("note.txt");
    let expect = target.clone();
    let observed = drive(
        &wire,
        Host::Slow(Duration::from_millis(2_500)),
        Duration::from_secs(600),
        Duration::from_secs(30),
        Duration::from_millis(600),
        64 * 1024,
        move |mut peer| {
            Box::pin(async move {
                let _prompt_id = handshake(&mut peer).await;
                let _ = ask_for_a_write(
                    &mut peer,
                    &expect,
                    "the host is thinking
",
                )
                .await;
                // Silent while the host blocks, and after it: the deadline is the only way out.
                tokio::time::sleep(Duration::from_secs(60)).await;
            })
        },
        None,
    );

    assert!(
        matches!(observed.outcome, Err(SessionEnd::Expired)),
        "the blocked callback delayed the deadline; it did not cancel it: {:?}",
        observed.outcome
    );
    assert!(
        observed.elapsed >= Duration::from_millis(2_400),
        "the session could not be cut short while the host held the thread, and pretending otherwise
         would be the dishonest claim: it waited {:?}",
        observed.elapsed
    );
    assert!(
        observed.elapsed < Duration::from_secs(10),
        "and it did end: {:?}",
        observed.elapsed
    );
}
