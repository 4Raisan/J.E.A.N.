use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use jean_process::{channel::Channel, LaunchSpec, TerminalMode};
use jean_protocol::config::VoiceConfig;
use jean_protocol::interaction::{
    self as wire, Envelope, OwnerCommand, OwnerReply, RequestGate, Role, Snapshot, TaskView,
    WorkerCommand as Command, WorkerEvent as Event,
};
use jean_protocol::TaskState;

use crate::Context;

pub struct Settings {
    pub voice: VoiceConfig,
    pub task_worker: LaunchSpec,
    pub voice_worker: LaunchSpec,
    pub task_timeout: Duration,
}

impl Settings {
    pub fn from_context(ctx: &Context) -> io::Result<Self> {
        let exe = std::env::current_exe()?;
        let dir = exe
            .parent()
            .ok_or_else(|| io::Error::other("executable has no directory"))?;
        let parent = std::env::vars().collect();
        let (mut task_env, _) = crate::env::build_child_env(
            &ctx.config.process.env_allowlist,
            &BTreeMap::new(),
            &parent,
        );
        task_env.insert(
            "JEAN_DATA_DIR".into(),
            ctx.paths.data_dir.to_string_lossy().into_owned(),
        );
        for name in [
            "LOCALAPPDATA",
            "APPDATA",
            "USERPROFILE",
            "JEAN_MODEL_PROVIDER",
            "JEAN_MODEL",
            "JEAN_LOG_LEVEL",
            ctx.config.model.api_key_env.as_str(),
        ] {
            if let Ok(value) = std::env::var(name) {
                task_env.insert(name.into(), value);
            }
        }
        let voice_env = ["SystemRoot", "PATH", "TEMP", "TMP"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|v| (key.into(), v)))
            .collect();
        let spec = |executable: PathBuf, env| LaunchSpec {
            executable,
            args: vec![],
            working_dir: None,
            env,
            timeout: None,
            output_cap_bytes: wire::MAX_FRAME as u64,
            stdout_path: None,
            stderr_path: None,
            terminal: TerminalMode::Pipes,
        };
        // A provisioned native worker is a different executable from the default one, and
        // the session that owns playback has to be able to name it: `cargo build
        // --workspace` rebuilds the sibling of `jean.exe` without the feature, so a session
        // hard-wired to that sibling could lose the operator's speech engine between two
        // builds without saying anything (§61.4). A path that is not a file stops here,
        // rather than arriving later as an `unavailable:` voice state to decipher.
        let voice_worker = match std::env::var_os("JEAN_VOICE_WORKER") {
            Some(path) => {
                let path = PathBuf::from(path);
                if !path.is_file() {
                    return Err(io::Error::other(format!(
                        "JEAN_VOICE_WORKER names `{}`, which is not a file",
                        path.display()
                    )));
                }
                path
            }
            None => dir.join("jean-voice.exe"),
        };
        Ok(Self {
            voice: ctx.config.voice.clone(),
            task_worker: spec(dir.join("jean-task-worker.exe"), task_env),
            voice_worker: spec(voice_worker, voice_env),
            task_timeout: ctx.default_timeout(),
        })
    }
}

struct Pending {
    command: Command,
    until: Instant,
    started: bool,
}
struct Peer {
    channel: Channel,
    session: String,
    role: Role,
    next: u64,
    pending: BTreeMap<u64, Pending>,
    stopping: Option<Instant>,
}

impl Peer {
    fn start(spec: &LaunchSpec, role: Role, timeout: Duration) -> io::Result<Self> {
        let session = jean_protocol::ipc::new_task_id();
        let mut spec = spec.clone();
        spec.args.extend(["--session".into(), session.clone()]);
        let channel = Channel::spawn(&spec)?;
        let mut peer = Self {
            channel,
            session,
            role,
            next: 0,
            pending: BTreeMap::new(),
            stopping: None,
        };
        peer.send(Command::Hello { role }, timeout)?;
        Ok(peer)
    }

    fn send(&mut self, command: Command, timeout: Duration) -> io::Result<u64> {
        if self.pending.len() >= wire::QUEUE_CAPACITY {
            return Err(io::Error::other("worker request limit reached"));
        }
        self.next += 1;
        let message = Envelope::new(
            self.session.clone(),
            self.next,
            wire::now_ms() + timeout.as_millis() as u64,
            command.clone(),
        );
        self.channel.send(wire::encode(&message)?)?;
        self.pending.insert(
            self.next,
            Pending {
                command,
                until: Instant::now() + timeout,
                started: false,
            },
        );
        Ok(self.next)
    }

    fn accept(&mut self, message: &Envelope<Event>) -> bool {
        if message.version != wire::VERSION || message.session != self.session {
            return false;
        }
        let Some(pending) = self.pending.get_mut(&message.request) else {
            return false;
        };
        if Instant::now() >= pending.until || wire::now_ms() >= message.deadline_ms {
            return false;
        }
        let starting = matches!(
            message.body,
            Event::TaskStarted { .. } | Event::SpeechStarted { .. }
        );
        if starting && pending.started {
            return false;
        }
        if matches!(message.body, Event::TaskFinished { state, .. } if !state.is_terminal() && state != TaskState::NeedsAttention)
        {
            return false;
        }
        let matched = matches!((&pending.command, &message.body),
            (Command::Hello { role: a }, Event::Ready { role: b }) if a == b)
            || matches!(
                (&pending.command, &message.body),
                (Command::ConfigureVoice { .. }, Event::Configured)
                    | (Command::Ping, Event::Pong)
                    | (Command::Shutdown, Event::Bye)
                    | (_, Event::Refused { .. })
            )
            || match (&pending.command, &message.body) {
                (
                    Command::Execute { task_id: a, .. },
                    Event::TaskStarted { task_id: b } | Event::TaskFinished { task_id: b, .. },
                ) => a == b,
                (
                    Command::Speak { utterance: a, .. },
                    Event::SpeechStarted { utterance: b } | Event::SpeechStopped { utterance: b },
                ) => a == b,
                (Command::Interrupt { utterance: a }, Event::SpeechStopped { utterance: b }) => {
                    a == b
                }
                (Command::BeginCapture { activation: a }, Event::Capturing { activation: b }) => {
                    a == b
                }
                (
                    Command::EndCapture { activation: a },
                    Event::Transcript { activation: b, .. },
                ) => a == b,
                _ => false,
            };
        if !matched {
            return false;
        }
        if starting {
            pending.started = true;
        } else {
            self.pending.remove(&message.request);
        }
        if let Event::SpeechStopped { utterance } = message.body {
            self.pending.retain(|_, p| !matches!(p.command, Command::Speak { utterance: u, .. } | Command::Interrupt { utterance: u } if u == utterance));
        }
        true
    }
}

struct Retired {
    peer: Peer,
    until: Instant,
}
struct Activation {
    id: u64,
    released: bool,
    transcript: Option<String>,
    /// Set when the worker answers `Capturing`, i.e. when a device stream really opened.
    /// Without it the owner would report "listening" from the instant it asked.
    opened: bool,
    until: Instant,
}

pub struct Controller {
    settings: Settings,
    session: String,
    gate: RequestGate,
    task: Option<Peer>,
    voice: Option<Peer>,
    task_view: Option<TaskView>,
    goal: Option<String>,
    voice_state: String,
    utterance: Option<u64>,
    activation: Option<Activation>,
    sequence: u64,
    retired: Vec<Retired>,
    shutting_down: bool,
    rejected_events: u64,
    teardown_failed: bool,
    announcements: crate::speech::Announcements,
}

impl Controller {
    pub fn new(settings: Settings) -> io::Result<Self> {
        settings.voice.validate().map_err(io::Error::other)?;
        let session = jean_protocol::ipc::new_task_id();
        let announcements = crate::speech::Announcements::new(settings.voice.announce_task_state);
        let mut owner = Self {
            settings,
            gate: RequestGate::new(session.clone()),
            session,
            task: None,
            voice: None,
            task_view: None,
            goal: None,
            voice_state: "disabled".into(),
            utterance: None,
            activation: None,
            sequence: 0,
            retired: Vec::new(),
            shutting_down: false,
            rejected_events: 0,
            teardown_failed: false,
            announcements,
        };
        if owner.settings.voice.enabled {
            if let Err(error) = owner.start_voice() {
                owner.voice_state = format!("unavailable: {error}");
            }
        }
        Ok(owner)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            session: self.session.clone(),
            task: self.task_view.clone(),
            voice: self.voice_state.clone(),
            voice_session: self.voice.as_ref().map(|p| p.session.clone()),
            voice_pid: self.voice.as_ref().map(|p| p.channel.control().pid()),
            utterance: self.utterance,
            activation: self.activation.as_ref().map(|a| a.id),
            capturing: self
                .activation
                .as_ref()
                .is_some_and(|a| a.opened && !a.released),
            transcript_ready: self
                .activation
                .as_ref()
                .is_some_and(|a| a.transcript.is_some()),
            transcript: self.activation.as_ref().and_then(|a| a.transcript.clone()),
            shutting_down: self.shutting_down,
            retiring: self.retired.len(),
            rejected_events: self.rejected_events,
        }
    }

    pub fn handle(&mut self, request: Envelope<OwnerCommand>) -> OwnerReply {
        self.tick();
        let result = self
            .gate
            .accept(&request)
            .map_err(io::Error::other)
            .and_then(|()| self.dispatch(request.body));
        let (accepted, detail) = match result {
            Ok(detail) => (true, detail),
            Err(error) => (false, error.to_string()),
        };
        OwnerReply {
            accepted,
            detail,
            snapshot: self.snapshot(),
        }
    }

    fn dispatch(&mut self, command: OwnerCommand) -> io::Result<String> {
        if self.shutting_down && !matches!(command, OwnerCommand::Status | OwnerCommand::Shutdown) {
            return Err(io::Error::other("JEAN is shutting down"));
        }
        match command {
            OwnerCommand::Status => Ok("current owner state".into()),
            OwnerCommand::Submit { goal } => self.submit(goal),
            OwnerCommand::CancelTask { task_id } => self.cancel_task(&task_id),
            OwnerCommand::StartVoice => {
                self.start_voice()?;
                Ok("voice startup requested".into())
            }
            OwnerCommand::StopVoice => {
                self.stop_voice()?;
                Ok("voice shutdown requested; task unchanged".into())
            }
            OwnerCommand::Speak { text } => {
                if !wire::text_valid(&text) {
                    return Err(io::Error::other("speech text must be 1..=4096 bytes"));
                }
                if self.utterance.is_some() || self.activation.is_some() {
                    return Err(io::Error::other(
                        "voice busy; interrupt or finish capture first",
                    ));
                }
                let id = self.next_id();
                let timeout = Duration::from_millis(
                    self.settings.voice.utterance_timeout_ms
                        + self.settings.voice.request_timeout_ms,
                );
                self.ready_voice()?.send(
                    Command::Speak {
                        utterance: id,
                        text,
                    },
                    timeout,
                )?;
                self.utterance = Some(id);
                Ok(format!("utterance {id} submitted"))
            }
            OwnerCommand::Interrupt { utterance } => {
                if self.utterance != Some(utterance) {
                    return Err(io::Error::other("stale utterance"));
                }
                let timeout = self.request_timeout();
                let peer = self.ready_voice()?;
                if peer.pending.values().any(
                    |p| matches!(p.command, Command::Interrupt { utterance: u } if u == utterance),
                ) {
                    return Ok("interruption already requested".into());
                }
                peer.send(Command::Interrupt { utterance }, timeout)?;
                Ok("speech interruption requested; task unchanged".into())
            }
            OwnerCommand::BeginCapture => {
                if self.activation.is_some() || self.utterance.is_some() {
                    return Err(io::Error::other(
                        "finish capture or interrupt playback before push-to-talk",
                    ));
                }
                let id = self.next_id();
                let timeout = self.request_timeout();
                self.ready_voice()?
                    .send(Command::BeginCapture { activation: id }, timeout)?;
                self.activation = Some(Activation {
                    id,
                    released: false,
                    transcript: None,
                    opened: false,
                    until: Instant::now()
                        + Duration::from_millis(self.settings.voice.capture_timeout_ms),
                });
                Ok(format!("push-to-talk activation {id}; no task submitted"))
            }
            OwnerCommand::EndCapture { activation } => {
                if !self
                    .activation
                    .as_ref()
                    .is_some_and(|a| a.id == activation && !a.released)
                {
                    return Err(io::Error::other("stale capture release"));
                }
                let timeout = self.request_timeout();
                self.ready_voice()?
                    .send(Command::EndCapture { activation }, timeout)?;
                self.activation.as_mut().unwrap().released = true;
                Ok("capture released; explicit transcript submission still required".into())
            }
            // The discard path a transcript-only tester needs: release the press so the
            // worker's push-to-talk state frees, then forget the capture, words and all.
            // `SubmitTranscript` stays the only way a spoken phrase becomes a task.
            OwnerCommand::ClearCapture { activation } => {
                if !self.activation.as_ref().is_some_and(|a| a.id == activation) {
                    return Err(io::Error::other("stale capture discard"));
                }
                if !self.activation.as_ref().is_some_and(|a| a.released) {
                    let timeout = self.request_timeout();
                    self.ready_voice()?
                        .send(Command::EndCapture { activation }, timeout)?;
                }
                self.activation = None;
                Ok("capture discarded; no transcript kept and nothing submitted".into())
            }
            OwnerCommand::SubmitTranscript { activation } => {
                let active = self
                    .activation
                    .as_ref()
                    .filter(|a| a.id == activation && a.released)
                    .ok_or_else(|| io::Error::other("stale capture submission"))?;
                let text = active.transcript.clone().ok_or_else(|| {
                    io::Error::other("no finalized transcript for this activation")
                })?;
                // A released-and-submitted transcript is the only place a spoken
                // phrase can become a control, and only while a task is active.
                if self.settings.voice.spoken_controls {
                    let task = self.task_view.as_ref().map(|t| t.id.clone());
                    match crate::speech::spoken_control(&text, task.is_some()) {
                        Some(crate::speech::SpokenControl::Status) => {
                            self.activation = None;
                            return Ok(match task {
                                Some(id) => format!("spoken status: task {id}"),
                                None => "spoken status: no task".into(),
                            });
                        }
                        Some(crate::speech::SpokenControl::CancelTask) => {
                            self.activation = None;
                            let id = task.expect("a control matched an active task");
                            return self.cancel_task(&id);
                        }
                        None => {}
                    }
                }
                let accepted = self.submit(text)?;
                self.activation = None;
                Ok(accepted)
            }
            OwnerCommand::Shutdown => {
                self.shutting_down = true;
                if let Some(id) = self
                    .task_view
                    .as_ref()
                    .filter(|_| self.task.is_some())
                    .map(|t| t.id.clone())
                {
                    let _ = self.cancel_task(&id);
                }
                let _ = self.stop_voice();
                Ok("shutdown requested".into())
            }
        }
    }

    fn next_id(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
    fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.settings.voice.request_timeout_ms)
    }
    fn shutdown_timeout(&self) -> Duration {
        Duration::from_millis(self.settings.voice.shutdown_timeout_ms)
    }
    fn role_retiring(&self, role: Role) -> bool {
        self.retired.iter().any(|r| r.peer.role == role)
    }

    fn submit(&mut self, goal: String) -> io::Result<String> {
        if !wire::text_valid(&goal) {
            return Err(io::Error::other("goal must be 1..=4096 bytes"));
        }
        if self.task.is_some() || self.role_retiring(Role::Task) || self.teardown_failed {
            return Err(io::Error::other(
                "one task at a time; previous worker must settle first",
            ));
        }
        let task_id = jean_protocol::ipc::new_task_id();
        let peer = Peer::start(
            &self.settings.task_worker,
            Role::Task,
            Duration::from_millis(self.settings.voice.startup_timeout_ms),
        )?;
        self.task_view = Some(TaskView {
            id: task_id.clone(),
            state: TaskState::Queued,
            pid: Some(peer.channel.control().pid()),
            cancelling: false,
        });
        self.goal = Some(goal);
        self.task = Some(peer);
        Ok(format!(
            "task {task_id} accepted exactly once; execution is not an utterance"
        ))
    }

    fn cancel_task(&mut self, id: &str) -> io::Result<String> {
        if !self.task_view.as_ref().is_some_and(|t| t.id == id) || self.task.is_none() {
            return Err(io::Error::other("stale or settled task cancellation"));
        }
        if self.task_view.as_ref().unwrap().cancelling {
            return Ok("cancellation already requested".into());
        }
        self.task_view.as_mut().unwrap().cancelling = true;
        if self.goal.is_some() {
            self.finish_task(TaskState::Cancelled);
        } else {
            let timeout = self.shutdown_timeout();
            let peer = self.task.as_mut().unwrap();
            peer.stopping = Some(Instant::now() + timeout);
            if peer
                .send(Command::CancelTask { task_id: id.into() }, timeout)
                .is_err()
            {
                self.finish_task(TaskState::NeedsAttention);
            }
        }
        Ok("task cancellation requested; voice unchanged; effects are never replayed".into())
    }

    fn start_voice(&mut self) -> io::Result<()> {
        if !self.settings.voice.enabled {
            return Err(io::Error::other(
                "voice disabled; no worker or models loaded",
            ));
        }
        if self.voice.is_some() || self.role_retiring(Role::Voice) || self.teardown_failed {
            return Err(io::Error::other(
                "voice already started or still being reaped",
            ));
        }
        self.voice = Some(Peer::start(
            &self.settings.voice_worker,
            Role::Voice,
            Duration::from_millis(self.settings.voice.startup_timeout_ms),
        )?);
        self.voice_state = "starting".into();
        Ok(())
    }

    fn ready_voice(&mut self) -> io::Result<&mut Peer> {
        if self.voice_state != "ready" {
            return Err(io::Error::other("voice is not ready"));
        }
        self.voice
            .as_mut()
            .ok_or_else(|| io::Error::other("voice unavailable"))
    }

    fn stop_voice(&mut self) -> io::Result<()> {
        self.activation = None;
        self.utterance = None;
        let timeout = self.shutdown_timeout();
        if let Some(peer) = self.voice.as_mut() {
            if peer.stopping.is_some() {
                return Ok(());
            }
            peer.stopping = Some(Instant::now() + timeout);
            self.voice_state = "stopping".into();
            if peer.send(Command::Shutdown, timeout).is_err() {
                self.fail_voice("stopped");
            }
        }
        Ok(())
    }

    fn retire(&mut self, mut peer: Peer) {
        if peer.channel.terminate().is_err() {
            self.teardown_failed = true;
        }
        self.retired.push(Retired {
            peer,
            until: Instant::now() + self.shutdown_timeout(),
        });
    }

    fn finish_task(&mut self, state: TaskState) {
        if let Some(task) = self.task_view.as_mut() {
            task.state = state;
        }
        self.goal = None;
        if let Some(peer) = self.task.take() {
            self.retire(peer);
        }
    }

    fn fail_voice(&mut self, reason: &str) {
        self.activation = None;
        self.utterance = None;
        self.announcements.clear();
        self.voice_state = reason.into();
        if let Some(peer) = self.voice.take() {
            self.retire(peer);
        }
    }

    pub fn tick(&mut self) {
        for role in [Role::Task, Role::Voice] {
            for _ in 0..wire::QUEUE_CAPACITY {
                let slot = if role == Role::Task {
                    &mut self.task
                } else {
                    &mut self.voice
                };
                let Some(peer) = slot.as_mut() else {
                    break;
                };
                let polled = peer.channel.poll();
                match polled {
                    Ok(Some(bytes)) => {
                        let message: Envelope<Event> = match serde_json::from_slice(&bytes) {
                            Ok(message) => message,
                            Err(_) => {
                                self.peer_failed(role, "malformed worker frame");
                                break;
                            }
                        };
                        if !peer.accept(&message) {
                            self.rejected_events += 1;
                            continue;
                        }
                        self.apply_event(role, message.body);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        self.peer_failed(role, "worker disconnected; not replayed");
                        break;
                    }
                }
            }
            let peer = if role == Role::Task {
                self.task.as_ref()
            } else {
                self.voice.as_ref()
            };
            if peer.is_some_and(|p| {
                p.stopping.is_some_and(|until| Instant::now() >= until)
                    || p.pending
                        .values()
                        .any(|request| Instant::now() >= request.until)
            }) {
                self.peer_failed(role, "worker deadline expired; tree terminated");
            }
        }
        if self
            .activation
            .as_ref()
            .is_some_and(|a| Instant::now() >= a.until)
        {
            self.fail_voice("push-to-talk deadline expired");
        }
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].peer.channel.reap() {
                let retired = self.retired.swap_remove(i);
                if retired.peer.role == Role::Task {
                    if let Some(task) = self.task_view.as_mut() {
                        task.pid = None;
                    }
                }
            } else {
                if Instant::now() >= self.retired[i].until {
                    self.teardown_failed = true;
                }
                i += 1;
            }
        }
        self.pump_announcements();
    }

    /// Speak queued task state only when the utterance slot is free and no
    /// push-to-talk activation is open, so an announcement can never talk over
    /// the operator or occupy the slot an explicit capture needs.
    fn pump_announcements(&mut self) {
        if self.shutting_down || self.utterance.is_some() || self.activation.is_some() {
            return;
        }
        let current = self.task_view.as_ref().map(|t| (t.id.clone(), t.state));
        if let Some((id, state)) = current.as_ref() {
            self.announcements.observe(id, *state);
        }
        let Some(text) = self
            .announcements
            .next(current.as_ref().map(|(id, _)| id.as_str()))
        else {
            return;
        };
        if self.voice_state != "ready" || self.role_retiring(Role::Voice) || self.teardown_failed {
            return;
        }
        let id = self.next_id();
        let timeout = Duration::from_millis(
            self.settings.voice.utterance_timeout_ms + self.settings.voice.request_timeout_ms,
        );
        if self
            .ready_voice()
            .and_then(|peer| {
                peer.send(
                    Command::Speak {
                        utterance: id,
                        text,
                    },
                    timeout,
                )
            })
            .is_ok()
        {
            self.utterance = Some(id);
        }
    }

    fn peer_failed(&mut self, role: Role, reason: &str) {
        if role == Role::Task {
            self.finish_task(TaskState::NeedsAttention);
        } else {
            self.fail_voice(reason);
        }
    }

    fn apply_event(&mut self, role: Role, event: Event) {
        match (role, event) {
            (Role::Task, Event::Ready { .. }) => {
                if let Some(goal) = self.goal.take() {
                    let id = self.task_view.as_ref().unwrap().id.clone();
                    if self
                        .task
                        .as_mut()
                        .unwrap()
                        .send(
                            Command::Execute { task_id: id, goal },
                            self.settings.task_timeout,
                        )
                        .is_err()
                    {
                        self.finish_task(TaskState::NeedsAttention);
                    }
                }
            }
            (Role::Task, Event::TaskStarted { .. }) => {
                self.task_view.as_mut().unwrap().state = TaskState::Running;
            }
            (Role::Task, Event::TaskFinished { state, .. })
                if state.is_terminal() || state == TaskState::NeedsAttention =>
            {
                self.finish_task(state)
            }
            (Role::Voice, Event::Ready { .. }) if self.voice_state == "starting" => {
                let timeout = Duration::from_millis(self.settings.voice.configure_timeout_ms);
                if self
                    .voice
                    .as_mut()
                    .unwrap()
                    .send(
                        Command::ConfigureVoice {
                            config: Box::new(self.settings.voice.clone()),
                        },
                        timeout,
                    )
                    .is_err()
                {
                    self.fail_voice("voice configuration failed");
                }
            }
            (Role::Voice, Event::Configured) if self.voice_state == "starting" => {
                self.voice_state = "ready".into()
            }
            (Role::Voice, Event::SpeechStarted { .. }) => {}
            (Role::Voice, Event::SpeechStopped { utterance })
                if self.utterance == Some(utterance) =>
            {
                self.utterance = None
            }
            (Role::Voice, Event::Capturing { activation }) => {
                // The device stream is open. This is the answer to "Starting microphone…",
                // and it is the only thing that turns that into "Listening".
                if let Some(a) = self.activation.as_mut().filter(|a| a.id == activation) {
                    a.opened = true;
                }
            }
            (Role::Voice, Event::Transcript { activation, text }) => {
                if let Some(a) = self
                    .activation
                    .as_mut()
                    .filter(|a| a.id == activation && a.released)
                {
                    if wire::text_valid(&text) && a.transcript.is_none() {
                        a.transcript = Some(text);
                    } else {
                        self.rejected_events += 1;
                    }
                } else {
                    self.rejected_events += 1;
                }
            }
            (Role::Voice, Event::Bye) => self.fail_voice("stopped"),
            (role, Event::Refused { reason }) => {
                self.peer_failed(role, &format!("worker refused: {reason}"))
            }
            _ => self.rejected_events += 1,
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.shutting_down && self.task.is_none() && self.voice.is_none() && self.retired.is_empty()
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        let _ = self.dispatch(OwnerCommand::Shutdown);
        let until = Instant::now() + self.shutdown_timeout() * 2 + Duration::from_millis(100);
        while !self.is_stopped() && Instant::now() < until {
            self.tick();
            std::thread::yield_now();
        }
        if self.is_stopped() && !self.teardown_failed {
            Ok(())
        } else {
            Err(io::Error::other(
                "owned worker teardown did not settle within its deadline",
            ))
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

pub fn cancel_io_thread<T>(thread: std::thread::JoinHandle<T>) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::CancelSynchronousIo;
    let until = Instant::now() + Duration::from_secs(2);
    while !thread.is_finished() && Instant::now() < until {
        let _ = unsafe { CancelSynchronousIo(HANDLE(thread.as_raw_handle())) };
        std::thread::yield_now();
    }
    // The CLI must not return with an uncancellable thread retaining its standard streams.
    if !thread.is_finished() {
        std::process::abort();
    }
    let _ = thread.join();
}

pub struct ClientOutput {
    writer: Option<std::thread::JoinHandle<()>>,
    sender: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    completed: std::sync::mpsc::Receiver<()>,
    pending: std::collections::VecDeque<Instant>,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ClientOutput {
    pub fn new() -> io::Result<Self> {
        use std::io::Write;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, Arc,
        };
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(wire::QUEUE_CAPACITY);
        let (done, completed) = mpsc::sync_channel(wire::QUEUE_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let writer = std::thread::Builder::new()
            .name("jean-owner-output".into())
            .spawn(move || {
                while let Ok(bytes) = receiver.recv() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let mut out = io::stdout().lock();
                    if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
                        break;
                    }
                    if done.try_send(()).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            writer: Some(writer),
            sender: Some(sender),
            completed,
            pending: std::collections::VecDeque::new(),
            stopping,
        })
    }

    pub fn send(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        self.poll()?;
        if bytes.len() > wire::MAX_FRAME || self.pending.len() >= wire::QUEUE_CAPACITY {
            return Err(io::Error::other("control client output limit reached"));
        }
        self.sender
            .as_ref()
            .ok_or_else(|| io::Error::other("control output closed"))?
            .try_send(bytes)
            .map_err(|_| io::Error::other("control client disconnected"))?;
        self.pending
            .push_back(Instant::now() + Duration::from_secs(2));
        Ok(())
    }

    pub fn poll(&mut self) -> io::Result<()> {
        while self.completed.try_recv().is_ok() {
            self.pending.pop_front();
        }
        if !self.pending.is_empty()
            && (self.writer.as_ref().is_some_and(|t| t.is_finished())
                || self
                    .pending
                    .front()
                    .is_some_and(|until| Instant::now() >= *until))
        {
            return Err(io::Error::other(
                "control client disconnected or stopped reading",
            ));
        }
        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        self.sender.take();
        let result = loop {
            if let Err(error) = self.poll() {
                break Err(error);
            }
            if self.pending.is_empty() {
                break Ok(());
            }
            std::thread::yield_now();
        };
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(writer) = self.writer.take() {
            cancel_io_thread(writer);
        }
        self.pending.clear();
        result
    }
}

impl Drop for ClientOutput {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
