//! The ACP connector: an external coding agent in a session JEAN owns.
//!
//! ## Shape of a session
//!
//! [`jean_process::piped`] starts the agent inside a job object with three pipes; this module puts the
//! agent's stdout and stdin behind [`crate::frames`] caps and hands them to the official ACP Rust SDK
//! as a [`ByteStreams`] transport. The SDK speaks the protocol; JEAN does the spawning, the bounding,
//! the deciding, and the killing.
//!
//! Everything the agent can ask for arrives on the **session** stream — `session/update`
//! notifications and the requests `session/request_permission`, `fs/read_text_file`,
//! `fs/write_text_file` — because `build_session` installs a route for the session's traffic; a
//! builder-level handler would never see them, and an unanswered request is how a session stalls
//! until the deadline. So [`handle_message`] is the one place where a question becomes a
//! [`PermissionRequest`] and gets an answer.
//!
//! ## Authority, stated precisely
//!
//! * The **host** decides. [`PermissionHost`] is implemented in `jean-core` over `jean-policy`, with
//!   the run's grant consulted in the same position it occupies for a plan step. A covered action
//!   produces no prompt here because it produced none there.
//! * The **agent cannot widen its own authority**: an answer naming an option the agent did not offer
//!   is turned into a rejection, and a rejection is carried by the agent's own `reject_once` (then
//!   `reject_always`) option, or by `Cancelled` when it offered neither — never by an allow.
//! * `fs/write_text_file` is performed **by JEAN**, after approval, inside the delegation's workspace.
//!   The capability is advertised only when the worker's configured sandbox permits writing, and the
//!   path is resolved and checked against the workspace, so `..` and an outside absolute path are
//!   refused whatever the host answered.
//!
//! ## Why an async runtime appears here at all
//!
//! The SDK's connection entry point takes an `AsyncFnOnce`; it has no synchronous form. Hand-writing
//! ACP to avoid a runtime would mean a second, worse implementation of a protocol that already has an
//! SDK and a conformance suite. So the runtime is adopted deliberately, for the two crates that need
//! it: the session runs on a current-thread runtime owned by [`run`], on this thread, for the length
//! of one delegation. Phases 0-2 gain no futures, and no thread outlives the call.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{AsyncRead, AsyncWrite};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::v1::schema::{
    CancelNotification, ClientCapabilities, ContentBlock, FileSystemCapabilities, Implementation,
    InitializeRequest, PermissionOptionKind, ReadTextFileRequest, ReadTextFileResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason, ToolCall,
    ToolCallUpdate, ToolKind, WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::v1::{ConnectionTo, SessionMessage, SessionSetup};
use agent_client_protocol::{Agent, ByteStreams, Client, Dispatch};

use crate::contract::{
    Bounds, EventSink, PermissionAnswer, PermissionHost, PermissionOffer, PermissionRequest,
    RunSpec, WorkerAction, WorkerEvent, WorkerFailureKind, WorkerOutcome, STOP_CANCELLED,
    STOP_END_TURN, STOP_FAILED, STOP_MAX_TOKENS, STOP_MAX_TURN_REQUESTS, STOP_REFUSED,
    STOP_TIMEOUT,
};
use crate::error::{Result, WorkerError};
use crate::frames::{BoundedRead, BoundedWrite, FrameCount, Heartbeat, SharedPipe, WireTrace};
use crate::watchdog::{Signal, Watchdog};

#[cfg(test)]
mod wire_tests;

/// The agent's own final text, capped: a report, not a megabyte.
const CLAIM_CHARS: usize = 4000;
/// One progress line's worth of agent output.
const CHUNK_CHARS: usize = 600;

/// Run one ACP worker to settlement.
pub fn run(
    spec: &RunSpec<'_>,
    host: &mut dyn PermissionHost,
    sink: &mut dyn EventSink,
) -> Result<WorkerOutcome> {
    let bounds = spec.bounds;
    let counted = FrameCount::new();
    let heartbeat = Heartbeat::new();

    let child = jean_process::piped::spawn(spec.launch).map_err(WorkerError::Process)?;
    let control = child.control();
    let events = 1usize;
    if !sink.emit(WorkerEvent::Started {
        worker: spec.worker.name.clone(),
        kind: spec.worker.kind.clone(),
        pid: control.pid(),
    }) {
        let _ = control.terminate();
        return Ok(outcome(
            STOP_FAILED.to_owned(),
            None,
            None,
            0,
            &counted,
            bounds,
            Some("the event sink closed before the worker started".to_owned()),
            None,
            None,
        ));
    }

    let stderr = crate::cli::stderr_capture(&child, bounds)?;
    let (tx, signals) = crate::watchdog::session_channel();
    let watchdog = Watchdog::arm(
        control.clone(),
        spec.cancel.clone(),
        bounds.session_deadline,
        bounds.idle_timeout,
        heartbeat.clone(),
        bounds.shutdown_grace,
        Some(tx),
    );

    // The capped pipes, not the SDK's `Stdio` transport: that one would speak over *J.E.A.N.'s*
    // standard streams, and the worker is a child whose streams JEAN created.
    let stdin = child
        .stdin_file()
        .map_err(|e| output_error("worker stdin pipe", e))?;
    let stdout = child
        .stdout_file()
        .map_err(|e| output_error("worker stdout pipe", e))?;
    // From here the transport's duplicate is the only writer this process has on the worker's stdin, so
    // dropping the transport is what tells the agent the session is over. Left open, this handle would
    // keep the pipe alive after the transport was gone and the worker would wait for a prompt forever.
    let _ = child.close_stdin();
    // Neither half may join a thread when the transport drops: see [`SharedPipe`]. These two cells are
    // why `run` still owns the pipes after the session ends.
    let (write_src, stdin_cell) = SharedPipe::share(blocking::Unblock::new(stdin));
    let (read_src, stdout_cell) = SharedPipe::share(blocking::Unblock::new(stdout));
    // One trace for both directions, so the cap is per session. Off unless `JEAN_ACP_TRACE=1`, and
    // even then it records shape — direction, length, `method`, `id` — never a payload.
    let trace = WireTrace::from_env();
    let outgoing = BoundedWrite::new(write_src, bounds.max_frame_bytes);
    let incoming = BoundedRead::new(read_src, bounds.max_frame_bytes, counted.clone())
        // The SDK's reader is the only thing that knows the agent is talking, so the heartbeat lives here
        // or the idle timeout kills a busy worker.
        .with_heartbeat(heartbeat);
    let (outgoing, incoming) = match &trace {
        Some(trace) => (
            outgoing.with_trace(trace.clone()),
            incoming.with_trace(trace.clone()),
        ),
        None => (outgoing, incoming),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|e| WorkerError::Runtime {
            reason: e.to_string(),
        })?;

    // Stated before the session runs, because after it the arithmetic is a lie: the deadline has
    // passed, and "it waited 0 s" would be reported as the budget.
    let budget = bounds
        .session_deadline
        .saturating_duration_since(Instant::now());

    let ended = runtime.block_on(session(
        ByteStreams::new(outgoing, incoming),
        Rc::new(Mutex::new(HostRef(host))),
        Rc::new(Mutex::new(SinkRef(sink))),
        spec,
        signals,
        trace,
    ));

    // Two kinds of non-settlement, kept apart on purpose. A session that ended because the transport or
    // the agent misbehaved is reported with the SDK's own sentence; a session that hit its deadline is
    // reported as the deadline it hit, and the worker is terminated rather than left running. Neither
    // one produces a claim, and neither one is allowed to look like the agent finished.
    if matches!(ended, Err(SessionEnd::Expired)) {
        let _ = control.terminate_on_timeout();
    }

    // The order here is the fix for §38, and every step is a precondition for the next.
    //
    // A settled session still has a live agent: it closed its turn, not its stdin. Now that the
    // transport is gone it has no writer left in this process, so a well-behaved one exits on its own
    // and reports a real exit code; one that does not is terminated, because a worker with no session
    // is an orphan in the making and nothing else is going to answer it. Only then may the pipes be
    // joined: `blocking::Unblock` reaps its helper thread on drop, and that thread is parked in
    // `ReadFile` until the process holding the other end is gone. Dropping the runtime comes first for
    // the same reason — the SDK can hold a transport half in a task that outlives the session future.
    let mut exit_code = wait_for_exit(&control, bounds.shutdown_grace);
    if control.is_running() {
        let _ = control.terminate();
        exit_code = wait_for_exit(&control, bounds.shutdown_grace)
            .or_else(|| control.exit_code_if_settled());
    }
    drop(runtime);
    drop(stdin_cell);
    drop(stdout_cell);
    watchdog.stop();
    crate::cli::join_capture(stderr);

    let session_events = match &ended {
        Ok(flow) => flow.events,
        Err(_) => 0,
    };
    let (stop_reason, claim, failure, failure_kind, remaining_work) = match ended {
        Ok(flow) => (
            flow.stop_reason,
            flow.claim,
            flow.failure,
            flow.failure_kind,
            flow.remaining_work,
        ),
        Err(SessionEnd::Abnormal(e)) => (
            // A session that ended abnormally says so. This is what separates "the agent did the work
            // and it did not pass" from "we never got an answer".
            abnormal_stop_reason(&spec.cancel).to_owned(),
            None,
            Some(format!("the ACP session ended abnormally: {e}")),
            None,
            None,
        ),
        Err(SessionEnd::Expired) => (
            if spec.cancel.load(Ordering::Relaxed) {
                STOP_CANCELLED
            } else {
                STOP_TIMEOUT
            }
            .to_owned(),
            None,
            Some(format!(
                "the ACP session did not settle within its {} s deadline; the worker was terminated \
                 and nothing about its turn was observed",
                budget.as_secs().max(1)
            )),
            None,
            None,
        ),
    };

    Ok(outcome(
        stop_reason,
        claim,
        exit_code,
        events + session_events,
        &counted,
        bounds,
        failure,
        failure_kind,
        remaining_work,
    ))
}

fn output_error(what: &str, e: jean_process::ProcessError) -> WorkerError {
    WorkerError::Output {
        path: what.to_owned(),
        reason: e.to_string(),
    }
}

/// How an abnormal end is named: the operator's cancel outranks a transport error, because it is the
/// more specific fact about why the session stopped.
fn abnormal_stop_reason(cancel: &AtomicBool) -> &'static str {
    if cancel.load(Ordering::Relaxed) {
        STOP_CANCELLED
    } else {
        STOP_FAILED
    }
}

/// `&mut dyn Trait` does not itself implement `Trait`, and the SDK's handlers hold a type rather than
/// a borrow of a trait object. These two wrappers are one method wide, and they are the only place the
/// connector's `&mut dyn` arguments become something an `Rc<Mutex<_>>` can own.
struct HostRef<'a>(&'a mut dyn PermissionHost);

impl PermissionHost for HostRef<'_> {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionAnswer {
        self.0.decide(request)
    }
}

struct SinkRef<'a>(&'a mut dyn EventSink);

impl EventSink for SinkRef<'_> {
    fn emit(&mut self, event: WorkerEvent) -> bool {
        self.0.emit(event)
    }
}

/// What one ACP session produced.
#[derive(Debug, Default)]
struct Flow {
    stop_reason: String,
    claim: Option<String>,
    events: usize,
    failure: Option<String>,
    failure_kind: Option<WorkerFailureKind>,
    remaining_work: Option<String>,
}

/// How a session that never produced a turn-report ended.
#[derive(Debug)]
enum SessionEnd {
    /// The transport broke, the agent answered with an error at the protocol level, or the connection
    /// future failed. Carried as a sentence rather than the SDK's error type, because the abandoned
    /// connection case has to say the same thing in the same field.
    Abnormal(String),
    /// The delegation's absolute deadline passed while the session was waiting — for a frame, for a
    /// handler, or for an agent that had stopped listening. Nothing after this point may claim to have
    /// observed the turn.
    Expired,
}

/// initialize → `session/new` → prompt → updates, over any byte transport, under one deadline.
///
/// The deadline is applied **here** rather than by the caller, for two reasons. It bounds the whole
/// session — handshake, prompt, handlers and the idle wait between frames — and it is only reachable
/// from a test that drives this function, which is the only way "a stalled session is stopped by its
/// deadline" becomes a checked statement instead of a hope. Without it, a session whose agent answers
/// the client's own request and then falls silent waits for as long as the process lives: measured at
/// 221 s against a 90 s session in §38, with three timers configured and none of them reaching it.
///
/// Generic over the transport so a test can put an ACP agent on the other end of an in-memory duplex;
/// production calls it with the capped pipes of a spawned worker.
async fn session<OB, IB, H, S>(
    transport: ByteStreams<OB, IB>,
    host: Rc<Mutex<H>>,
    sink: Rc<Mutex<S>>,
    spec: &RunSpec<'_>,
    signals: tokio::sync::mpsc::UnboundedReceiver<Signal>,
    trace: Option<Arc<WireTrace>>,
) -> std::result::Result<Flow, SessionEnd>
where
    OB: AsyncWrite + Send + 'static,
    IB: AsyncRead + Send + 'static,
    H: PermissionHost + ?Sized,
    S: EventSink + ?Sized,
{
    let state = Rc::new(State {
        host,
        sink,
        worker: spec.worker.name.clone(),
        workspace: spec.request.workspace.clone(),
        writes_allowed: spec.worker.sandbox != "read-only",
        seq: AtomicU64::new(0),
        bounds: spec.bounds.clone(),
        events: Mutex::new(0usize),
        message: Mutex::new(String::new()),
        trace,
    });

    // The turn's report, stashed where the caller can reach it whether or not the connection ever
    // finishes. This is the shape of the hang §38 measured: the session loop *did* end — the trace shows
    // the dispatch handled, the answer written, the signal received — and `connect_with` still did not
    // return, because its transport was parked writing to a peer that had stopped reading. A deadline
    // that only bounded our own loop would not have noticed, and before this there was no bound at all.
    let settled: Rc<RefCell<Option<std::result::Result<Flow, String>>>> =
        Rc::new(RefCell::new(None));
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let stash = settled.clone();

    let raced = async move {
        let connect = Client
            .v1()
            .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                let turn = run_turn(state, cx, spec, signals).await;
                *stash.borrow_mut() = Some(turn.map_err(|error| error.to_string()));
                let _ = done_tx.send(());
                Ok(Flow::default())
            });
        // Either the connection ends on its own terms, or the turn is over and the report is taken and
        // the connection left behind. Dropping a wedged connection here is deliberate: its transport is
        // the worker's pipe, and `run` terminates that worker the moment this returns.
        tokio::select! {
            result = connect => result,
            _ = done_rx => Ok(Flow::default()),
        }
    };

    match tokio::time::timeout_at(
        tokio::time::Instant::from_std(spec.bounds.session_deadline),
        raced,
    )
    .await
    {
        Err(_elapsed) => Err(SessionEnd::Expired),
        Ok(_connection) => match settled.borrow_mut().take() {
            Some(Ok(flow)) => Ok(flow),
            Some(Err(reason)) => Err(SessionEnd::Abnormal(reason)),
            // The connection returned without the turn reaching its end. There is no report to make,
            // and inventing one would be the exact failure this connector exists to avoid.
            None => Err(SessionEnd::Abnormal(
                "the ACP connection ended before the session reported anything".to_owned(),
            )),
        },
    }
}

/// The protocol itself, from handshake to the agent's final report.
///
/// Split out of [`session`] because the two have different jobs: this one speaks ACP and decides when
/// the turn is over; `session` decides whether it is allowed to keep waiting.
async fn run_turn<H, S>(
    state: Rc<State<H, S>>,
    cx: ConnectionTo<Agent>,
    spec: &RunSpec<'_>,
    mut signals: tokio::sync::mpsc::UnboundedReceiver<Signal>,
) -> std::result::Result<Flow, agent_client_protocol::Error>
where
    H: PermissionHost + ?Sized,
    S: EventSink + ?Sized,
{
    if spec.worker.kind == "acp"
        && spec.worker.name.eq_ignore_ascii_case("opencode")
        && spec.worker.model.trim().is_empty()
    {
        return Err(refusal(
            "OpenCode ACP requires an exact model ID before a task prompt can be sent; set [delegation.workers].model in jean.toml (for example, opencode/beam-free).",
        ));
    }

    let mut initialize = InitializeRequest::new(ProtocolVersion::V1);
    // Built from `Default` and then set, because both capability structs are `#[non_exhaustive]`:
    // a schema field added later is a compile error here rather than a silent omission.
    let mut fs = FileSystemCapabilities::default();
    // A `read-only` worker is not offered a way in: advertising `writeTextFile` is how an agent
    // decides it may ask, so the offer and the answer have to come from the same policy.
    fs.read_text_file = true;
    fs.write_text_file = state.writes_allowed;
    let mut capabilities = ClientCapabilities::default();
    capabilities.fs = fs;
    capabilities.terminal = false;
    initialize.client_capabilities = capabilities;
    initialize.client_info = Some(Implementation::new(
        "jean",
        env!("CARGO_PKG_VERSION").to_owned(),
    ));
    let _agent_info = cx.send_request(initialize).block_task().await?;

    cx.build_session(spec.request.workspace.as_path())
        .block_task()
        .run_until(async |SessionSetup { mut session, response, .. }| {
            use agent_client_protocol::v1::schema::{
                SessionConfigKind, SessionConfigOptionCategory, SetSessionConfigOptionRequest,
            };

            let requested = spec.worker.model.as_str();
            if !requested.is_empty() {
                let option = response.config_options.as_deref().unwrap_or_default().iter()
                    .find(|option| option.category == Some(SessionConfigOptionCategory::Model))
                    .ok_or_else(|| refusal("ACP agent does not expose a model selector. Use an agent that supports ACP session configuration."))?;
                let selected = cx.send_request(SetSessionConfigOptionRequest::new(
                    session.session_id().clone(), option.id.clone(), requested,
                )).block_task().await?;
                let mut matching = selected.config_options.iter()
                    .filter(|confirmed| confirmed.id == option.id);
                let confirmed = matching.next().and_then(|confirmed| match &confirmed.kind {
                    SessionConfigKind::Select(select) => Some(select.current_value.0.as_ref()),
                    _ => None,
                });
                if confirmed != Some(requested) || matching.next().is_some() {
                    return Err(refusal(&format!(
                        "ACP did not confirm requested model {requested:?} (reported {confirmed:?}). Check the worker's model ID and ACP support."
                    )));
                }
                state.emit(WorkerEvent::ModelConfirmed { model: requested.to_owned() });
                tracing::info!(model = requested, "ACP worker confirmed the requested model");
            }
            session.send_prompt(&spec.request.prompt)?;
            let mut flow = Flow::default();
            loop {
                tokio::select! {
                    message = session.read_update() => {
                    state.note("update");
                    match message? {
                        SessionMessage::PromptResponse(response) => {
                            match response {
                                Ok(prompt) => {
                                    flow.stop_reason = map_stop(prompt.stop_reason);
                                    if flow.stop_reason == STOP_REFUSED {
                                        flow.failure = Some("the agent refused to continue".to_owned());
                                    }
                                }
                                Err(error) => {
                                    flow.stop_reason = STOP_FAILED.to_owned();
                                    let (failure_kind, remaining_work) =
                                        classify_recovery_error(&error.data);
                                    flow.failure_kind = failure_kind;
                                    flow.remaining_work = remaining_work;
                                    flow.failure = Some(format!(
                                        "the agent answered the prompt with an error: {error}"
                                    ));
                                }
                            }
                            break;
                        }
                        SessionMessage::Message(dispatch) => {
                            state.note("dispatch");
                            // A handler is a callback — into the policy host, the prompter, the
                            // filesystem — and a callback is where a session stalls invisibly: this
                            // loop cannot see the update stream or the signal channel while it is
                            // running, so a handler that never returns looks exactly like a worker
                            // that never answers. Racing it against both the deadline for one
                            // handler and the signal channel is what keeps cancel responsive
                            // *during* a callback rather than only between them.
                            let budget = tokio::time::sleep(state.bounds.permission_timeout);
                            tokio::pin!(budget);
                            tokio::select! {
                                result = handle_message(&state, dispatch) => {
                                    result?;
                                }
                                _ = budget => {
                                    flow.stop_reason = STOP_FAILED.to_owned();
                                    flow.failure = Some(format!(
                                        "a request handler waited longer than the {} s permission \
                                         window and was abandoned; the agent's request was left \
                                         unanswered on purpose, because the session is ending",
                                        state.bounds.permission_timeout.as_secs().max(1)
                                    ));
                                    break;
                                }
                                signal = signals.recv() => {
                                    state.note("cancel during handler");
                                    // The session is going away mid-handler, so the agent's request
                                    // will not be answered — that is the definition of ending it,
                                    // and the job object, not the protocol, is what makes it true.
                                    let _ = cx.send_notification(CancelNotification::new(
                                        session.session_id().clone(),
                                    ));
                                    apply_signal(&mut flow, signal, &spec.cancel);
                                    break;
                                }
                            }
                        }
                        // `SessionMessage` is `#[non_exhaustive]`: an update kind this build has
                        // not seen is passed over rather than ending the session.
                        _ => continue,
                    }
                        },
                    signal = signals.recv() => {
                        state.note("signal while waiting");
                        // Told before killed: `session/cancel` is the protocol's polite ending,
                        // and the watchdog closing the job is what happens if it is ignored.
                        let _ = cx.send_notification(CancelNotification::new(
                            session.session_id().clone(),
                        ));
                        apply_signal(&mut flow, signal, &spec.cancel);
                        break;
                    }
                }
            }
            flow.claim = state.take_claim();
            flow.events = state.event_count();
            Ok(flow)
        })
        .await
}

/// What a watchdog signal does to the session's report.
///
/// A closed channel is a cancel, not a mystery: the only way it closes is the connector going away.
/// A timeout keeps its own name, because the operator's `--json` and exit status distinguish "ran out
/// of time" from "the agent broke", and an idle worker is reported as a stalled one rather than a slow
/// one — that distinction is the reason the two limits are separate settings.
fn apply_signal(flow: &mut Flow, signal: Option<Signal>, cancel: &AtomicBool) {
    match signal {
        None | Some(Signal::Cancelled) => flow.stop_reason = STOP_CANCELLED.to_owned(),
        Some(Signal::TimedOut) => flow.stop_reason = STOP_TIMEOUT.to_owned(),
        Some(Signal::Idle) => {
            flow.stop_reason = STOP_FAILED.to_owned();
            flow.failure = Some("the worker went silent past its idle timeout".to_owned());
        }
    }
    if cancel.load(Ordering::Relaxed) {
        flow.stop_reason = STOP_CANCELLED.to_owned();
    }
}

/// The handlers' shared state: everything a reply needs that is not the message itself.
struct State<H: ?Sized, S: ?Sized> {
    host: Rc<Mutex<H>>,
    sink: Rc<Mutex<S>>,
    worker: String,
    workspace: PathBuf,
    writes_allowed: bool,
    seq: AtomicU64,
    bounds: Bounds,
    events: Mutex<usize>,
    /// The agent's streamed text for this turn, joined as it arrives.
    message: Mutex<String>,
    /// Where to record the session's own progress, when the operator asked to see it.
    trace: Option<Arc<WireTrace>>,
}

impl<H: PermissionHost + ?Sized, S: EventSink + ?Sized> State<H, S> {
    /// Where the session is, when the operator asked to be told.
    fn note(&self, stage: &str) {
        if let Some(trace) = &self.trace {
            trace.note(stage);
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// One event out, and whether the sink is still there to receive the next one.
    fn emit(&self, event: WorkerEvent) -> bool {
        let ok = self
            .sink
            .lock()
            .map(|mut sink| sink.emit(event))
            .unwrap_or(false);
        if ok {
            if let Ok(mut events) = self.events.lock() {
                *events += 1;
            }
        }
        ok
    }

    fn event_count(&self) -> usize {
        self.events.lock().map(|count| *count).unwrap_or_default()
    }

    fn decide(&self, request: &PermissionRequest) -> PermissionAnswer {
        match self.host.lock() {
            Ok(mut host) => host.decide(request),
            Err(_) => PermissionAnswer::Denied {
                reason: "the policy host's lock was poisoned".to_owned(),
            },
        }
    }

    fn append_message(&self, text: &str) {
        if let Ok(mut buffer) = self.message.lock() {
            if buffer.len() < CLAIM_CHARS * 4 {
                buffer.push_str(text);
            }
        }
    }

    fn take_claim(&self) -> Option<String> {
        let text = self
            .message
            .lock()
            .map(|buffer| buffer.clone())
            .unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(clip(text, CLAIM_CHARS))
        }
    }
}

/// One session message: progress recorded, questions answered, anything else refused explicitly.
async fn handle_message<H, S>(
    state: &State<H, S>,
    dispatch: agent_client_protocol::v1::Dispatch,
) -> std::result::Result<(), agent_client_protocol::Error>
where
    H: PermissionHost + ?Sized,
    S: EventSink + ?Sized,
{
    MatchDispatch::new(dispatch.into_raw())
        .if_request(async |request: RequestPermissionRequest, responder| {
            let answer = ask(state, &request);
            let outcome = permission_outcome(&request, &answer);
            tracing::debug!(
                arm = "request_permission",
                answered = answer.is_yes(),
                "ACP handler"
            );
            responder.respond(RequestPermissionResponse::new(outcome))?;
            Ok(())
        })
        .await
        .if_request(async |request: WriteTextFileRequest, responder| {
            let wrote = write_text_file(state, &request);
            tracing::debug!(
                arm = "fs/write_text_file",
                written = wrote.is_ok(),
                "ACP handler"
            );
            match wrote {
                Ok(()) => responder.respond(WriteTextFileResponse::new())?,
                Err(reason) => responder.respond_with_error(refusal(&reason))?,
            }
            Ok(())
        })
        .await
        .if_request(async |request: ReadTextFileRequest, responder| {
            let read = read_text_file(state, &request);
            tracing::debug!(
                arm = "fs/read_text_file",
                read = read.is_ok(),
                "ACP handler"
            );
            match read {
                Ok(content) => responder.respond(ReadTextFileResponse::new(content))?,
                Err(reason) => responder.respond_with_error(refusal(&reason))?,
            }
            Ok(())
        })
        .await
        .if_notification(async |notification: SessionNotification| {
            observe_update(state, notification);
            Ok(())
        })
        .await
        // A request for something this connector does not implement is answered, not dropped: a
        // silent client is what makes an agent wait for a reply that will never come.
        .otherwise(async |message| {
            if let Dispatch::Request(_, responder) = message {
                let _ =
                    responder.respond_with_error(agent_client_protocol::Error::method_not_found());
            }
            Ok(())
        })
        .await
        .map(|_handled| ())
}

/// The question, the policy's answer, and the record of both.
fn ask<H, S>(state: &State<H, S>, request: &RequestPermissionRequest) -> PermissionAnswer
where
    H: PermissionHost + ?Sized,
    S: EventSink + ?Sized,
{
    let offers: Vec<PermissionOffer> = request
        .options
        .iter()
        .map(|option| PermissionOffer {
            option_id: option.option_id.0.to_string(),
            name: option.name.clone(),
            kind: kind_name(option.kind).to_owned(),
        })
        .collect();
    let permission = PermissionRequest {
        worker: state.worker.clone(),
        tool_call_id: request.tool_call.tool_call_id.0.to_string(),
        title: request
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| describe_tool_update(&request.tool_call)),
        action: classify(&request.tool_call),
        offers,
    };
    let answer = state.decide(&permission);
    let _ = state.emit(WorkerEvent::PermissionAsked {
        request: permission,
        answer: answer.clone(),
    });
    answer
}

/// The frame that carries the answer back.
fn permission_outcome(
    request: &RequestPermissionRequest,
    answer: &PermissionAnswer,
) -> RequestPermissionOutcome {
    match answer {
        PermissionAnswer::Cancelled => RequestPermissionOutcome::Cancelled,
        PermissionAnswer::Selected { option_id }
            if request
                .options
                .iter()
                .any(|option| option.option_id.0.as_ref() == option_id.as_str()) =>
        {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.clone()))
        }
        // Either the host refused, or it named an option the agent never offered. The second case is
        // treated as a refusal: sending a made-up id would be JEAN inventing authority mid-session.
        _ => reject_option(request).unwrap_or(RequestPermissionOutcome::Cancelled),
    }
}

/// The agent's own no. `reject_once` before `reject_always`: a permanent prohibition the operator
/// never discussed is a second decision, and this module does not make decisions.
fn reject_option(request: &RequestPermissionRequest) -> Option<RequestPermissionOutcome> {
    let pick = |kind: PermissionOptionKind| {
        request
            .options
            .iter()
            .find(|option| option.kind == kind)
            .map(|option| option.option_id.0.to_string())
    };
    pick(PermissionOptionKind::RejectOnce)
        .or_else(|| pick(PermissionOptionKind::RejectAlways))
        .map(|id| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)))
}

/// What the agent wants, in the vocabulary the policy engine already decides on.
///
/// Takes the ACP `ToolCallUpdate` — the shape `session/request_permission` carries, whose fields are
/// all optional because the agent may update one call across several messages.
fn classify(update: &ToolCallUpdate) -> WorkerAction {
    let fields = &update.fields;
    let title = fields
        .title
        .clone()
        .unwrap_or_else(|| format!("tool {}", update.tool_call_id.0));
    let path = fields
        .locations
        .as_ref()
        .and_then(|locations| locations.first())
        .map(|location| location.path.to_string_lossy().into_owned());
    match fields.kind.unwrap_or_default() {
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move => match path {
            Some(path) => WorkerAction::EditFile { path },
            None => WorkerAction::Unclassified {
                text: format!("edit: {title}"),
            },
        },
        ToolKind::Read => match path {
            Some(path) => WorkerAction::ReadFile { path },
            None => WorkerAction::Unclassified {
                text: format!("read: {title}"),
            },
        },
        ToolKind::Fetch => WorkerAction::Network {
            target: raw_string(&fields.raw_input, &["url", "uri"])
                .or(path)
                .unwrap_or(title),
        },
        // A tool that runs a command gives us its command line as one string. It stays one argv
        // element: splitting it would mean interpreting a shell, and JEAN does not run this command —
        // the agent does, inside the sandbox chosen before the session began. A digest over the whole
        // string therefore covers that string and nothing else, which is the conservative direction.
        ToolKind::Execute => WorkerAction::RunCommand {
            argv: vec![raw_string(&fields.raw_input, &["command"]).unwrap_or_else(|| title.clone())],
            cwd: raw_string(&fields.raw_input, &["cwd"]),
        },
        other => WorkerAction::Unclassified {
            text: format!("{other:?}: {title}"),
        },
    }
}

fn raw_string(value: &Option<serde_json::Value>, keys: &[&str]) -> Option<String> {
    value
        .as_ref()
        .and_then(|record| {
            keys.iter()
                .find_map(|key| record.get(*key).and_then(|v| v.as_str()))
        })
        .map(str::to_owned)
}

/// Progress, in the operator's terms, from one `session/update`.
fn observe_update<H, S>(state: &State<H, S>, notification: SessionNotification)
where
    H: PermissionHost + ?Sized,
    S: EventSink + ?Sized,
{
    let (source, text) = match notification.update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let text = text_of(&chunk.content);
            if let Some(text) = &text {
                state.append_message(text);
            }
            ("agent_message", text.unwrap_or_default())
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            ("thought", text_of(&chunk.content).unwrap_or_default())
        }
        SessionUpdate::ToolCall(call) => ("tool_call", describe_tool(&call)),
        SessionUpdate::ToolCallUpdate(update) => ("tool_call", describe_tool_update(&update)),
        SessionUpdate::Plan(plan) => ("plan", format!("{} step(s)", plan.entries.len())),
        other => ("status", format!("{other:?}")),
    };
    let text = clip(&text, CHUNK_CHARS);
    if text.trim().is_empty() {
        return;
    }
    let _ = state.emit(WorkerEvent::Progress {
        seq: state.next_seq(),
        source: source.to_owned(),
        text,
    });
}

fn text_of(content: &ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    }
}

fn describe_tool(call: &ToolCall) -> String {
    format!("{} [{:?}]", call.title, call.status)
}

fn describe_tool_update(update: &ToolCallUpdate) -> String {
    let title = update
        .fields
        .title
        .clone()
        .unwrap_or_else(|| format!("tool {}", update.tool_call_id.0));
    match &update.fields.status {
        Some(status) => format!("{title} [{status:?}]"),
        None => title,
    }
}

fn refusal(reason: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_request().data(reason.to_owned())
}

/// JEAN performs the write, so the agent never holds the file handle — and never holds a path outside
/// the tree either.
fn write_text_file<H, S>(
    state: &State<H, S>,
    request: &WriteTextFileRequest,
) -> std::result::Result<(), String>
where
    H: PermissionHost + ?Sized,
    S: EventSink + ?Sized,
{
    if !state.writes_allowed {
        return Err(
            "this worker's sandbox is read-only, so J.E.A.N. writes nothing on its behalf"
                .to_owned(),
        );
    }
    let (path, inside) = resolve_target(&state.workspace, &request.path).ok_or_else(|| {
        format!(
            "{} could not be resolved inside the delegation workspace",
            request.path.display()
        )
    })?;
    if request.content.len() as u64 > state.bounds.max_output_bytes {
        return Err(format!(
            "a write of {} bytes is over the {}-byte output ceiling",
            request.content.len(),
            state.bounds.max_output_bytes
        ));
    }
    let permission = fs_permission(state, &path, true);
    let answer = state.decide(&permission);
    let _ = state.emit(WorkerEvent::PermissionAsked {
        request: permission,
        answer: answer.clone(),
    });
    match answer {
        PermissionAnswer::Selected { .. } => {
            if !inside {
                // The host is the one that audits, and a host that is not workspace-aware must not
                // become the way out: an answer widens a grant, never a boundary.
                return Err(format!(
                    "{} is outside the delegation workspace, which is not something an answer lifts",
                    path.display()
                ));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
            }
            std::fs::write(&path, request.content.as_bytes())
                .map_err(|e| format!("could not write {}: {e}", path.display()))
        }
        PermissionAnswer::Denied { reason } => Err(format!("J.E.A.N. refused the write: {reason}")),
        PermissionAnswer::Cancelled => Err("the session is ending".to_owned()),
    }
}

fn read_text_file<H, S>(
    state: &State<H, S>,
    request: &ReadTextFileRequest,
) -> std::result::Result<String, String>
where
    H: PermissionHost + ?Sized,
    S: EventSink + ?Sized,
{
    let (path, inside) = resolve_target(&state.workspace, &request.path).ok_or_else(|| {
        format!(
            "{} could not be resolved inside the delegation workspace",
            request.path.display()
        )
    })?;
    let permission = fs_permission(state, &path, false);
    let answer = state.decide(&permission);
    let _ = state.emit(WorkerEvent::PermissionAsked {
        request: permission,
        answer: answer.clone(),
    });
    if !answer.is_yes() {
        return Err(match answer {
            PermissionAnswer::Denied { reason } => format!("J.E.A.N. refused the read: {reason}"),
            PermissionAnswer::Cancelled => "the session is ending".to_owned(),
            PermissionAnswer::Selected { .. } => unreachable!("is_yes was false"),
        });
    }
    if !inside {
        return Err(format!(
            "{} is outside the delegation workspace, which is not something an answer lifts",
            path.display()
        ));
    }
    let bytes =
        std::fs::read(&path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    if bytes.len() as u64 > state.bounds.max_output_bytes {
        return Err(format!(
            "{} is {} bytes, over the {}-byte ceiling",
            path.display(),
            bytes.len(),
            state.bounds.max_output_bytes
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))
}

/// An `fs/*` request as a permission request.
///
/// The options are JEAN's own, not the agent's: this method has no option list in the protocol, so
/// the one answer that exists is `allow_once`. `allow_always` is never offered here — a file read
/// cannot grant a future, different file.
fn fs_permission<H, S>(state: &State<H, S>, path: &Path, write: bool) -> PermissionRequest
where
    H: PermissionHost + ?Sized,
    S: EventSink + ?Sized,
{
    let display = path.display().to_string();
    PermissionRequest {
        worker: state.worker.clone(),
        tool_call_id: format!("fs:{}:{display}", if write { "write" } else { "read" }),
        title: format!("{} {display}", if write { "write" } else { "read" }),
        action: if write {
            WorkerAction::EditFile { path: display }
        } else {
            WorkerAction::ReadFile { path: display }
        },
        offers: vec![PermissionOffer {
            option_id: "allow_once".to_owned(),
            name: if write {
                "Allow this write once".to_owned()
            } else {
                "Allow this read once".to_owned()
            },
            kind: "allow_once".to_owned(),
        }],
    }
}

/// Canonical containment: the target's resolved path, and whether it is inside the workspace.
///
/// Not a policy question — a path outside the tree is something no run grant can have covered,
/// because the operator approved *this* directory. It is still *asked of the host*, so that a refusal
/// is recorded where the operator can see it rather than disappearing into the connector; the
/// `inside` flag stays the connector's own gate whatever the answer says.
fn resolve_target(workspace: &Path, target: &Path) -> Option<(PathBuf, bool)> {
    let root = workspace.canonicalize().ok()?;
    let candidate = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    };
    let resolved = resolve_existing_ancestor(&candidate)?;
    let inside = resolved.starts_with(&root);
    Some((resolved, inside))
}

/// Canonicalize the deepest ancestor that exists and re-attach the rest.
///
/// Two cases this covers that `Path::canonicalize` cannot: a write to a file that does not exist yet,
/// in a directory that does not exist yet, which is the ordinary shape of an agent creating a module —
/// and a symlinked directory, which is caught because the part that exists is resolved through the
/// link before the containment test runs.
fn resolve_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(existing) = cursor.canonicalize() {
            let mut resolved = existing;
            for name in tail.iter().rev() {
                resolved.push(name);
            }
            return Some(resolved);
        }
        // `file_name` is `None` for a path ending in `..`, and that is a refusal: a traversal that
        // cannot even be named is not a file inside the workspace.
        let name = cursor.file_name()?.to_owned();
        tail.push(name);
        cursor = cursor.parent()?.to_path_buf();
    }
}

fn map_stop(reason: StopReason) -> String {
    match reason {
        StopReason::EndTurn => STOP_END_TURN.to_owned(),
        StopReason::Refusal => STOP_REFUSED.to_owned(),
        StopReason::Cancelled => STOP_CANCELLED.to_owned(),
        StopReason::MaxTokens => STOP_MAX_TOKENS.to_owned(),
        StopReason::MaxTurnRequests => STOP_MAX_TURN_REQUESTS.to_owned(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

fn kind_name(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}

/// Wait for the worker to leave, then report its code, or `None` if it is still running.
fn wait_for_exit(control: &jean_process::piped::Control, grace: Duration) -> Option<i32> {
    let until = Instant::now() + grace.max(Duration::from_millis(500));
    loop {
        if let Some(code) = control.exit_code_if_settled() {
            return Some(code);
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[allow(clippy::too_many_arguments)]
fn outcome(
    stop_reason: String,
    claim: Option<String>,
    exit_code: Option<i32>,
    events: usize,
    counted: &FrameCount,
    bounds: &Bounds,
    failure: Option<String>,
    failure_kind: Option<WorkerFailureKind>,
    remaining_work: Option<String>,
) -> WorkerOutcome {
    WorkerOutcome {
        stop_reason,
        claim,
        exit_code,
        events,
        stdout_bytes: counted.get(),
        output_dir: bounds.output_dir.clone(),
        failure,
        failure_kind,
        remaining_work,
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}… (+{} chars)", count - max_chars)
}

fn classify_recovery_error(
    data: &Option<serde_json::Value>,
) -> (Option<WorkerFailureKind>, Option<String>) {
    let Some(data) = data.as_ref() else {
        return (None, None);
    };
    let kind = match data.get("kind").and_then(serde_json::Value::as_str) {
        Some("quota_exhausted") => WorkerFailureKind::QuotaExhausted,
        Some("context_exhausted") => WorkerFailureKind::ContextExhausted,
        Some("sign_in_expired") => WorkerFailureKind::SignInExpired,
        Some("network_unavailable") => WorkerFailureKind::NetworkUnavailable,
        _ => return (None, None),
    };
    let remaining = data
        .get("remaining_work")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 16 * 1024)
        .map(str::to_owned);
    if matches!(
        kind,
        WorkerFailureKind::QuotaExhausted | WorkerFailureKind::ContextExhausted
    ) && remaining.is_none()
    {
        return (None, None);
    }
    (Some(kind), remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{
        PermissionOption, ToolCallId, ToolCallLocation, ToolCallUpdateFields,
    };
    use std::path::Path;

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        // Owned on purpose: the typed ids are `Arc<str>`, so a borrowed test string would have to
        // outlive the request it is building.
        PermissionOption::new(id.to_owned(), id.to_owned(), kind)
    }

    /// `ToolCallUpdateFields` is `#[non_exhaustive]`, so it is built from its default and then set —
    /// the same reason the capability structs are.
    fn update(
        kind: Option<ToolKind>,
        title: Option<String>,
        raw_input: Option<serde_json::Value>,
        locations: Option<Vec<ToolCallLocation>>,
    ) -> ToolCallUpdate {
        let mut fields = ToolCallUpdateFields::default();
        fields.kind = kind;
        fields.title = title;
        fields.raw_input = raw_input;
        fields.locations = locations;
        ToolCallUpdate::new(ToolCallId::new("call-1"), fields)
    }

    fn request_with(options: Vec<PermissionOption>) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            "session-1",
            update(
                Some(ToolKind::Execute),
                Some("run the tests".to_owned()),
                None,
                None,
            ),
            options,
        )
    }

    #[test]
    fn an_allow_the_agent_offered_is_the_option_sent_back() {
        let request = request_with(vec![option("once", PermissionOptionKind::AllowOnce)]);
        let outcome = permission_outcome(
            &request,
            &PermissionAnswer::Selected {
                option_id: "once".to_owned(),
            },
        );
        match outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id.0.as_ref(), "once");
            }
            other => panic!("expected a selected outcome, got {other:?}"),
        }
    }

    #[test]
    fn an_option_the_agent_never_offered_is_not_sent_and_becomes_a_rejection() {
        // The agent listed one way to say yes. A host answer naming any other id is a bug or an attack
        // on the protocol, and either way it must not reach the agent as a grant.
        let request = request_with(vec![
            option("once", PermissionOptionKind::AllowOnce),
            option("no", PermissionOptionKind::RejectOnce),
        ]);
        let outcome = permission_outcome(
            &request,
            &PermissionAnswer::Selected {
                option_id: "always-everything".to_owned(),
            },
        );
        match outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id.0.as_ref(), "no");
            }
            other => panic!("expected the agent's own rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_prefers_reject_once_and_only_uses_reject_always_if_it_must() {
        let both = request_with(vec![
            option("always-no", PermissionOptionKind::RejectAlways),
            option("once-no", PermissionOptionKind::RejectOnce),
        ]);
        match permission_outcome(&both, &PermissionAnswer::Denied { reason: "x".into() }) {
            RequestPermissionOutcome::Selected(s) => assert_eq!(s.option_id.0.as_ref(), "once-no"),
            other => panic!("{other:?}"),
        }
        // A refusal that also permanently forbids something the operator never discussed would be a
        // second decision, made here.
        let only_always = request_with(vec![option(
            "always-no",
            PermissionOptionKind::RejectAlways,
        )]);
        match permission_outcome(
            &only_always,
            &PermissionAnswer::Denied { reason: "x".into() },
        ) {
            RequestPermissionOutcome::Selected(s) => {
                assert_eq!(s.option_id.0.as_ref(), "always-no")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_agent_that_offered_no_rejection_is_answered_cancelled_not_allowed() {
        let yes_only = request_with(vec![option("yes", PermissionOptionKind::AllowOnce)]);
        let chosen = permission_outcome(
            &yes_only,
            &PermissionAnswer::Denied {
                reason: "no".into(),
            },
        );
        assert!(matches!(chosen, RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn a_command_ask_is_one_argv_element_and_never_a_split_shell_string() {
        let update = update(
            Some(ToolKind::Execute),
            None,
            Some(serde_json::json!({"command": "cargo test --offline && rm -f x"})),
            None,
        );
        match classify(&update) {
            WorkerAction::RunCommand { argv, .. } => {
                assert_eq!(argv.len(), 1, "the command line is not interpreted");
                assert_eq!(argv[0], "cargo test --offline && rm -f x");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_edit_ask_becomes_a_write_of_the_path_the_agent_named() {
        let update = update(
            Some(ToolKind::Edit),
            None,
            None,
            Some(vec![ToolCallLocation::new("src/lib.rs")]),
        );
        let action = classify(&update);
        match &action {
            WorkerAction::EditFile { path } => {
                // The separator is the platform's; the assertion is about the file the agent named.
                assert!(path.ends_with("lib.rs"), "{path}");
                assert!(path.contains("src"), "{path}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_kind_the_connector_has_never_seen_is_reported_as_text_not_dropped() {
        let update = update(
            Some(ToolKind::Other),
            Some("do the thing".to_owned()),
            None,
            None,
        );
        match classify(&update) {
            WorkerAction::Unclassified { text } => assert!(text.contains("do the thing"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    /// What `write_text_file` and `read_text_file` ask `resolve_target` for, in the shape the older
    /// assertions read better: the path, if it is inside.
    fn inside_workspace(workspace: &Path, target: &Path) -> Option<PathBuf> {
        let (path, inside) = resolve_target(workspace, target)?;
        inside.then_some(path)
    }

    #[test]
    fn paths_outside_the_workspace_are_refused_by_the_connector_whatever_the_host_answers() {
        let root = std::env::temp_dir().join(format!("jean-acp-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        assert!(inside_workspace(&root, Path::new("src/lib.rs")).is_some());
        // A brand-new module in a brand-new directory is the ordinary case for an agent, and neither
        // part of it exists on disk yet.
        let created = inside_workspace(&root, Path::new("crates/new-mod/src/main.rs"))
            .expect("a path below the workspace stays inside it even when nothing exists yet");
        assert!(created
            .to_string_lossy()
            .replace('\\', "/")
            .contains("new-mod/src/main.rs"));
        if let Some(path) = inside_workspace(&root, Path::new("../../Windows/win.ini")) {
            panic!("escaped the workspace as {}", path.display());
        }
        assert!(inside_workspace(&root, Path::new("C:/Windows/win.ini")).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stop_reasons_are_normalised_and_a_refusal_keeps_its_own_name() {
        assert_eq!(map_stop(StopReason::EndTurn), STOP_END_TURN);
        assert_eq!(map_stop(StopReason::Refusal), STOP_REFUSED);
        assert_eq!(map_stop(StopReason::Cancelled), STOP_CANCELLED);
        assert_eq!(map_stop(StopReason::MaxTokens), STOP_MAX_TOKENS);
    }

    #[test]
    fn a_permission_option_kind_is_named_in_one_vocabulary_for_every_connector() {
        assert_eq!(kind_name(PermissionOptionKind::AllowOnce), "allow_once");
        assert_eq!(
            kind_name(PermissionOptionKind::RejectAlways),
            "reject_always"
        );
    }

    #[test]
    fn quota_failure_requires_exact_structured_data_not_error_text() {
        let quota = Some(serde_json::json!({
            "kind": "quota_exhausted",
            "remaining_work": "finish the second file"
        }));
        assert_eq!(
            classify_recovery_error(&quota),
            (
                Some(WorkerFailureKind::QuotaExhausted),
                Some("finish the second file".to_owned())
            )
        );
        let context = Some(serde_json::json!({
            "kind": "context_exhausted",
            "remaining_work": "finish the second file"
        }));
        assert_eq!(
            classify_recovery_error(&context).0,
            Some(WorkerFailureKind::ContextExhausted)
        );
        let sign_in = Some(serde_json::json!({"kind": "sign_in_expired"}));
        assert_eq!(
            classify_recovery_error(&sign_in).0,
            Some(WorkerFailureKind::SignInExpired)
        );
        let network = Some(serde_json::json!({"kind": "network_unavailable"}));
        assert_eq!(
            classify_recovery_error(&network).0,
            Some(WorkerFailureKind::NetworkUnavailable)
        );
        let unknown = Some(serde_json::json!({
            "message": "UnknownError: Unexpected server error"
        }));
        assert_eq!(classify_recovery_error(&unknown), (None, None));
        let missing = Some(serde_json::json!({"kind": "quota_exhausted"}));
        assert_eq!(classify_recovery_error(&missing), (None, None));
    }

    #[test]
    fn the_answer_travels_as_a_typed_sdk_response_not_a_handbuilt_frame() {
        let response = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Cancelled
        ));
    }
}
