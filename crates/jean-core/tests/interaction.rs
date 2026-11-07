use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use jean_core::interaction::{Controller, Settings};
use jean_process::{LaunchSpec, TerminalMode};
use jean_protocol::config::{SpeechBackend, VoiceConfig};
use jean_protocol::interaction::{
    self as wire, Envelope, OwnerCommand as Command, OwnerReply, Snapshot,
};
use jean_protocol::TaskState;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessHandleCount, OpenEventW, OpenProcess, SetEvent, TerminateProcess,
    WaitForSingleObject, EVENT_MODIFY_STATE, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

static SERIAL: Mutex<()> = Mutex::new(());
const TEST_BOUND: Duration = Duration::from_secs(8);

struct Harness {
    owner: Controller,
    next: u64,
    dir: PathBuf,
}
impl Harness {
    fn new(enabled: bool, task_args: &[&str], stale_voice: bool) -> Self {
        Self::with_voice(task_args, stale_voice, |voice, dir| {
            voice.enabled = enabled;
            voice.backend = SpeechBackend::Fixture;
            voice.fixture_transcript = dir.to_string_lossy().into_owned();
        })
    }

    fn with_voice(
        task_args: &[&str],
        stale_voice: bool,
        configure: impl FnOnce(&mut VoiceConfig, &PathBuf),
    ) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("p5-fixture-{}", jean_protocol::ipc::new_task_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut voice = VoiceConfig::default();
        configure(&mut voice, &dir);
        let fixture = PathBuf::from(env!("CARGO_BIN_EXE_jean-interaction-fixture"));
        let voice_executable = fixture.parent().unwrap().join("jean-voice.exe");
        let env: BTreeMap<String, String> = [
            "SystemRoot",
            "PATH",
            "TEMP",
            "TMP",
            "LOCALAPPDATA",
            "APPDATA",
            "USERPROFILE",
        ]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.into(), v)))
        .collect();
        let spec = |executable, args: Vec<String>| LaunchSpec {
            executable,
            args,
            env: env.clone(),
            working_dir: None,
            timeout: None,
            output_cap_bytes: 16384,
            stdout_path: None,
            stderr_path: None,
            terminal: TerminalMode::Pipes,
        };
        let owner = Controller::new(Settings {
            voice,
            task_worker: spec(
                fixture.clone(),
                task_args.iter().map(|s| (*s).into()).collect(),
            ),
            voice_worker: if stale_voice {
                spec(fixture, vec!["--stale-voice".into()])
            } else {
                spec(voice_executable, vec![])
            },
            task_timeout: Duration::from_secs(60),
        })
        .unwrap();
        Self {
            owner,
            next: 0,
            dir,
        }
    }
    fn request(&mut self, command: Command) -> OwnerReply {
        self.next += 1;
        self.owner.handle(Envelope::new(
            self.owner.snapshot().session,
            self.next,
            wire::now_ms() + 5000,
            command,
        ))
    }
    fn send(&mut self, command: Command) -> OwnerReply {
        let reply = self.request(command);
        assert!(reply.accepted, "{}", reply.detail);
        reply
    }
    fn until(&mut self, ready: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let until = Instant::now() + TEST_BOUND;
        loop {
            self.owner.tick();
            let state = self.owner.snapshot();
            if ready(&state) {
                return state;
            }
            assert!(Instant::now() < until, "barrier did not arrive: {state:?}");
            std::thread::yield_now();
        }
    }
    fn submit(&mut self) -> String {
        let goal = self.dir.to_string_lossy().into_owned();
        let id = self
            .send(Command::Submit { goal })
            .snapshot
            .task
            .unwrap()
            .id;
        self.until(|s| {
            s.task
                .as_ref()
                .is_some_and(|t| t.state == TaskState::Running)
        });
        id
    }
    fn descendant(&self, id: &str) -> u32 {
        std::fs::read_to_string(self.dir.join(format!("{id}.started")))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }
}

fn release(id: &str) {
    let name: Vec<u16> = format!("Local\\jean-fixture-{id}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())).unwrap();
        SetEvent(event).unwrap();
        CloseHandle(event).unwrap();
    }
}
fn handle_count() -> u32 {
    let mut count = 0;
    unsafe {
        GetProcessHandleCount(GetCurrentProcess(), &mut count).unwrap();
    }
    count
}
fn dead(pid: u32) -> bool {
    unsafe {
        match OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) {
            Ok(handle) => {
                let done = WaitForSingleObject(handle, 0) == WAIT_OBJECT_0;
                CloseHandle(handle).unwrap();
                done
            }
            Err(_) => true,
        }
    }
}
fn await_dead(pid: u32) {
    unsafe {
        if let Ok(handle) = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) {
            let status = WaitForSingleObject(handle, 2000);
            CloseHandle(handle).unwrap();
            assert_eq!(
                status, WAIT_OBJECT_0,
                "owned fixture {pid} did not finish termination"
            );
        }
    }
}
fn crash_owned_fixture(pid: u32) {
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid).unwrap();
        TerminateProcess(handle, 73).unwrap();
        assert_eq!(WaitForSingleObject(handle, 2000), WAIT_OBJECT_0);
        CloseHandle(handle).unwrap();
    }
}

#[test]
fn supervised_task_voice_status_cancel_restart_and_resource_bounds() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    // One worker lifecycle before measuring. The *first* Controller in a process
    // pays a one-time six-handle warm-up that no later cycle repeats, so a count
    // taken across it reports start-up as a leak — and the assertion only looked
    // clean when another test had already paid it. What has to hold is that
    // repeated utterances, crashes and restarts do not accumulate.
    {
        let mut warm = Harness::new(true, &[], false);
        warm.until(|s| s.voice == "ready");
        warm.owner.shutdown().unwrap();
    }
    let before = handle_count();
    let mut h = Harness::new(true, &[], false);
    h.until(|s| s.voice == "ready");
    let task = h.submit();
    let child = h.descendant(&task);
    let task_pid = h.owner.snapshot().task.unwrap().pid.unwrap();
    assert!(!dead(child));
    assert!(
        !h.request(Command::Submit {
            goal: "must not queue another task".into()
        })
        .accepted
    );
    let baseline = handle_count();
    let mut max_interrupt = Duration::ZERO;
    for _ in 0..24 {
        let utterance = h
            .send(Command::Speak {
                text: "task accepted".into(),
            })
            .snapshot
            .utterance
            .unwrap();
        let start = Instant::now();
        h.send(Command::Interrupt { utterance });
        h.until(|s| s.utterance.is_none());
        max_interrupt = max_interrupt.max(start.elapsed());
        assert!(!dead(child));
        assert_eq!(h.owner.snapshot().task.unwrap().id, task);
    }
    let started = Instant::now();
    let status = h.send(Command::Status);
    let status_time = started.elapsed();
    assert_eq!(status.snapshot.task.unwrap().state, TaskState::Running);
    assert!(
        handle_count() <= baseline,
        "interruptions must not accumulate OS handles"
    );
    let old_voice = h.owner.snapshot().voice_pid.unwrap();
    let old_session = h.owner.snapshot().voice_session.unwrap();
    crash_owned_fixture(old_voice);
    h.until(|s| s.voice_pid.is_none() && s.retiring == 0);
    assert!(!dead(child));
    h.send(Command::StartVoice);
    let restarted = h.until(|s| s.voice == "ready");
    assert_ne!(restarted.voice_session.unwrap(), old_session);
    assert_eq!(restarted.task.unwrap().id, task);
    assert_eq!(
        std::fs::read_dir(&h.dir).unwrap().count(),
        1,
        "voice restart cannot replay task submission"
    );
    let start = Instant::now();
    h.send(Command::CancelTask {
        task_id: task.clone(),
    });
    let final_state =
        h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()) && s.retiring == 0);
    assert_eq!(final_state.task.unwrap().state, TaskState::Cancelled);
    await_dead(child);
    await_dead(task_pid);
    let cancellation_time = start.elapsed();
    assert_eq!(h.send(Command::Status).snapshot.voice, "ready");
    assert!(!h.request(Command::CancelTask { task_id: task }).accepted);
    h.owner.shutdown().unwrap();
    drop(h);
    let after = handle_count();
    assert!(
        after <= before,
        "worker shutdown must not leak handles: before={before}, after={after}, delta={}",
        after as i64 - before as i64
    );
    eprintln!("MEASURED: status={status_time:?}; maximum of 24 interrupts={max_interrupt:?}; cooperative task-tree cancellation={cancellation_time:?}; handles before={before}, after={after}");
}

#[test]
fn explicit_push_to_talk_consumes_one_transcript_and_acknowledgement_cannot_replay_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::new(true, &[], false);
    h.until(|s| s.voice == "ready");
    let activation = h.send(Command::BeginCapture).snapshot.activation.unwrap();
    assert!(!h.request(Command::SubmitTranscript { activation }).accepted);
    h.send(Command::EndCapture { activation });
    h.until(|s| s.transcript_ready);
    assert!(
        h.owner.snapshot().task.is_none(),
        "transcription alone carries no task authority"
    );
    let result = h.send(Command::SubmitTranscript { activation });
    let task = result.snapshot.task.unwrap().id;
    assert!(!h.request(Command::SubmitTranscript { activation }).accepted);
    let utterance = h
        .send(Command::Speak {
            text: "acknowledged".into(),
        })
        .snapshot
        .utterance
        .unwrap();
    h.send(Command::Interrupt { utterance });
    h.until(|s| {
        s.utterance.is_none()
            && s.task
                .as_ref()
                .is_some_and(|t| t.state == TaskState::Running)
    });
    assert_eq!(std::fs::read_dir(&h.dir).unwrap().count(), 1);
    release(&task);
    h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
    assert_eq!(h.owner.snapshot().task.unwrap().state, TaskState::Succeeded);
    let newer = h.submit();
    assert_ne!(newer, task);
    assert!(!h.request(Command::CancelTask { task_id: task }).accepted);
    h.send(Command::CancelTask { task_id: newer });
    h.owner.shutdown().unwrap();
}

#[test]
fn forced_cancellation_reaps_descendants_and_task_failure_does_not_freeze_voice() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::new(true, &["--ignore-cancel"], false);
    h.until(|s| s.voice == "ready");
    let task = h.submit();
    let descendant = h.descendant(&task);
    h.send(Command::CancelTask { task_id: task });
    assert_eq!(h.send(Command::Status).snapshot.voice, "ready");
    let result = h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
    assert_eq!(result.task.unwrap().state, TaskState::NeedsAttention);
    await_dead(descendant);
    h.owner.shutdown().unwrap();
    let mut failed = Harness::new(true, &["--fail"], false);
    failed.until(|s| s.voice == "ready");
    let goal = failed.dir.to_string_lossy().into_owned();
    failed.send(Command::Submit { goal });
    failed.until(|s| {
        s.task
            .as_ref()
            .is_some_and(|t| t.state == TaskState::Failed && t.pid.is_none())
    });
    let utterance = failed
        .send(Command::Speak {
            text: "task failed, voice is available".into(),
        })
        .snapshot
        .utterance
        .unwrap();
    failed.send(Command::Interrupt { utterance });
    failed.until(|s| s.utterance.is_none());
    failed.owner.shutdown().unwrap();
}

#[test]
fn an_actual_old_session_frame_is_rejected_without_stopping_current_speech() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::new(true, &[], true);
    h.until(|s| s.voice == "ready");
    let utterance = h
        .send(Command::Speak {
            text: "held".into(),
        })
        .snapshot
        .utterance
        .unwrap();
    let state = h.until(|s| s.rejected_events == 3);
    assert_eq!(state.utterance, Some(utterance));
    assert!(
        !h.request(Command::Interrupt {
            utterance: utterance + 1
        })
        .accepted
    );
    h.send(Command::Interrupt { utterance });
    h.until(|s| s.utterance.is_none());
    let next = h
        .send(Command::Speak {
            text: "newer".into(),
        })
        .snapshot
        .utterance
        .unwrap();
    assert!(!h.request(Command::Interrupt { utterance }).accepted);
    assert_eq!(h.owner.snapshot().utterance, Some(next));
    h.owner.shutdown().unwrap();
}

#[test]
fn typed_and_ptt_goals_use_the_production_worker_and_run_scoped_policy() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::new(false, &[], false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let model = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(30);
        let mut requests = 0;
        while !stopping.load(Ordering::Acquire) && Instant::now() < until {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                    continue;
                }
                Err(error) => panic!("local model fixture: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut length = 0;
            let mut header_bytes = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                header_bytes += line.len();
                assert!(header_bytes < 16384);
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
            }
            assert!(length < 256 * 1024);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(request["messages"].is_array());
            let step = |id: &str, dependencies: Vec<&str>| serde_json::json!({"id":id,"title":"run registered fixture","verify":{"method":"exit_code","expected":0},"operation":{"kind":"run_command","tool":"fixture.echo"},"depends_on":dependencies});
            let plan = serde_json::json!({"schema_version":1,"goal":"run the registered fixture twice","steps":[step("s1",vec![]),step("s2",vec!["s1"])]}).to_string();
            let response = serde_json::json!({"model":"fixture","message":{"content":plan},"done":true,"done_reason":"stop"}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            requests += 1;
        }
        requests
    });
    let config = format!("[model]\nprovider = 'ollama'\nbase_url = 'http://{address}'\nmodel = 'fixture'\nstream = false\n[policy]\nrequire_approval = ['network_egress_new_domain']\n");
    std::fs::write(h.dir.join("jean.toml"), &config).unwrap();
    std::fs::write(h.dir.join("tools.toml"), "config_version = 1\n[[tool]]\nid = 'fixture.echo'\nkind = 'shell'\ncommand = 'cmd.exe'\nargs = ['/c', 'echo', 'synthetic-output']\ncapabilities = ['network_egress_new_domain']\nverify = { method = 'exit_code', expected = 0 }\n").unwrap();
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_jean-task-worker"));
    let env: BTreeMap<String, String> = [
        "SystemRoot",
        "PATH",
        "TEMP",
        "TMP",
        "LOCALAPPDATA",
        "APPDATA",
        "USERPROFILE",
    ]
    .into_iter()
    .filter_map(|k| std::env::var(k).ok().map(|v| (k.into(), v)))
    .collect();
    let mut task_env = env.clone();
    task_env.insert("JEAN_DATA_DIR".into(), h.dir.to_string_lossy().into_owned());
    let spec = |executable, env| LaunchSpec {
        executable,
        args: vec![],
        env,
        working_dir: None,
        timeout: None,
        output_cap_bytes: 16384,
        stdout_path: None,
        stderr_path: None,
        terminal: TerminalMode::Pipes,
    };
    h.owner = Controller::new(Settings {
        voice: VoiceConfig {
            enabled: true,
            backend: SpeechBackend::Fixture,
            fixture_transcript: "run the registered fixture twice".into(),
            ..VoiceConfig::default()
        },
        voice_worker: spec(bin.parent().unwrap().join("jean-voice.exe"), env),
        task_worker: spec(bin, task_env),
        task_timeout: Duration::from_secs(20),
    })
    .unwrap();
    h.until(|s| s.voice == "ready");
    for spoken in [false, true] {
        let task = if spoken {
            let activation = h.send(Command::BeginCapture).snapshot.activation.unwrap();
            h.send(Command::EndCapture { activation });
            h.until(|s| s.transcript_ready);
            h.send(Command::SubmitTranscript { activation })
                .snapshot
                .task
                .unwrap()
                .id
        } else {
            h.send(Command::Submit {
                goal: "run the registered fixture twice".into(),
            })
            .snapshot
            .task
            .unwrap()
            .id
        };
        let final_state = h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
        let store =
            jean_store::Store::open(&h.dir.join("jean.db"), "WAL", Duration::from_secs(2)).unwrap();
        let record = store.require_task(&task).unwrap();
        assert_eq!(
            final_state.task.unwrap().state,
            TaskState::Succeeded,
            "{record:?}"
        );
        assert_eq!(record.state, TaskState::Succeeded);
        let plan = store.current_plan_for_task(&task).unwrap().unwrap();
        assert_eq!(store.plan_steps(&plan.id).unwrap().len(), 2);
        assert!(store.list_approvals().unwrap().is_empty());
    }
    let audit = std::fs::read_to_string(h.dir.join("audit.jsonl")).unwrap();
    assert!(
        audit.matches("\"authority\":\"run_scope\"").count() >= 4,
        "{audit}"
    );
    let store =
        jean_store::Store::open(&h.dir.join("jean.db"), "WAL", Duration::from_secs(2)).unwrap();
    std::fs::write(h.dir.join("tools.toml"), format!("config_version = 1\n[[tool]]\nid = 'fixture.echo'\nkind = 'cli'\ncommand = '{}'\nargs = ['--held-tool', '{}']\ncapabilities = ['network_egress_new_domain']\nverify = {{ method = 'exit_code', expected = 0 }}\n", env!("CARGO_BIN_EXE_jean-interaction-fixture"), h.dir.display())).unwrap();
    let active = h
        .send(Command::Submit {
            goal: "run the registered fixture twice".into(),
        })
        .snapshot
        .task
        .unwrap()
        .id;
    let marker = h.dir.join("held.started");
    h.until(|_| std::fs::read_to_string(&marker).is_ok_and(|s| s.split_whitespace().count() == 2));
    let pids: Vec<u32> = std::fs::read_to_string(&marker)
        .unwrap()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2);
    h.send(Command::CancelTask {
        task_id: active.clone(),
    });
    h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
    assert_eq!(
        store.require_task(&active).unwrap().state,
        TaskState::Cancelled
    );
    for pid in pids {
        await_dead(pid);
    }
    let utterance = h
        .send(Command::Speak {
            text: "cancelled task, live voice".into(),
        })
        .snapshot
        .utterance
        .unwrap();
    h.send(Command::Interrupt { utterance });
    h.until(|s| s.utterance.is_none());
    let tasks_before = store
        .list_tasks(jean_store::TaskFilter::All, 100)
        .unwrap()
        .len();
    std::fs::write(
        h.dir.join("jean.toml"),
        format!("{config}auto_deny = ['network_egress_new_domain']\n"),
    )
    .unwrap();
    let task = h
        .send(Command::Submit {
            goal: "run the registered fixture twice".into(),
        })
        .snapshot
        .task
        .unwrap()
        .id;
    h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
    assert_eq!(store.require_task(&task).unwrap().state, TaskState::Failed);
    assert_eq!(
        store
            .list_tasks(jean_store::TaskFilter::All, 100)
            .unwrap()
            .len(),
        tasks_before + 1,
        "auto-denied proposal cannot create child tasks"
    );
    std::fs::write(h.dir.join("jean.toml"), "[malformed").unwrap();
    h.send(Command::Submit {
        goal: "must not execute with an unreadable configuration".into(),
    });
    let uncertain = h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
    assert_eq!(uncertain.task.unwrap().state, TaskState::NeedsAttention);
    assert_eq!(h.send(Command::Status).snapshot.voice, "ready");
    h.owner.shutdown().unwrap();
    stop.store(true, Ordering::Release);
    assert!(model.join().unwrap() >= 3);
    eprintln!("VERIFIED: production task worker, typed + PTT goals, four gated commands, run_scope audit, zero remembered approvals, auto-deny preserved");
}

#[test]
fn startup_timeout_and_backpressure_are_bounded() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::new(true, &["--hang-startup"], false);
    h.until(|s| s.voice == "ready");
    h.send(Command::Submit {
        goal: h.dir.to_string_lossy().into_owned(),
    });
    let state = h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
    assert_eq!(state.task.unwrap().state, TaskState::NeedsAttention);
    assert_eq!(h.send(Command::Status).snapshot.voice, "ready");
    h.owner.shutdown().unwrap();
    let spec = LaunchSpec {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_jean-interaction-fixture")),
        args: vec![
            "--hang-startup".into(),
            "--session".into(),
            "blocked".into(),
        ],
        env: ["SystemRoot", "PATH", "TEMP", "TMP"]
            .into_iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.into(), v)))
            .collect(),
        working_dir: None,
        timeout: None,
        output_cap_bytes: 16384,
        stdout_path: None,
        stderr_path: None,
        terminal: TerminalMode::Pipes,
    };
    let mut channel = jean_process::channel::Channel::spawn(&spec).unwrap();
    let pid = channel.control().pid();
    channel
        .send(
            wire::encode(&Envelope::new(
                "blocked".into(),
                1,
                wire::now_ms() + 5000,
                wire::WorkerCommand::Hello {
                    role: wire::Role::Task,
                },
            ))
            .unwrap(),
        )
        .unwrap();
    let refused = (0..32).any(|_| channel.send(vec![b'x'; wire::MAX_FRAME]).is_err());
    assert!(
        refused,
        "a non-reading worker must hit the bounded outgoing queue"
    );
    channel.terminate().unwrap();
    drop(channel);
    await_dead(pid);
}

#[test]
fn stopping_voice_leaves_task_alive_and_completion_wins_a_late_cancel() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::new(true, &[], false);
    h.until(|s| s.voice == "ready");
    let task = h.submit();
    let child = h.descendant(&task);
    let activation = h.send(Command::BeginCapture).snapshot.activation.unwrap();
    h.send(Command::StopVoice);
    h.until(|s| s.voice_pid.is_none() && s.retiring == 0);
    assert!(!dead(child));
    assert!(!h.request(Command::SubmitTranscript { activation }).accepted);
    h.send(Command::StartVoice);
    h.until(|s| s.voice == "ready");
    release(&task);
    h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
    assert!(!h.request(Command::CancelTask { task_id: task }).accepted);
    assert_eq!(h.owner.snapshot().task.unwrap().state, TaskState::Succeeded);
    h.owner.shutdown().unwrap();
}

#[test]
fn voice_disabled_creates_no_worker_and_rejects_activation_and_duplicate_submission() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let before = handle_count();
    let mut h = Harness::new(false, &[], false);
    assert!(handle_count() <= before);
    assert!(h.owner.snapshot().voice_pid.is_none());
    assert!(!h.request(Command::StartVoice).accepted);
    assert!(!h.request(Command::BeginCapture).accepted);
    let request = Envelope::new(
        h.owner.snapshot().session,
        100,
        wire::now_ms() + 5000,
        Command::Submit {
            goal: h.dir.to_string_lossy().into_owned(),
        },
    );
    let first = h.owner.handle(request.clone());
    assert!(first.accepted);
    assert!(!h.owner.handle(request).accepted);
    let stale = Envelope::new(
        "previous-owner".into(),
        101,
        wire::now_ms() + 5000,
        Command::Shutdown,
    );
    assert!(!h.owner.handle(stale).accepted);
    assert!(!h.owner.snapshot().shutting_down);
    h.owner.shutdown().unwrap();
}

#[test]
fn a_spoken_status_control_answers_the_active_task_and_submits_nothing_else() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::with_voice(&[], false, |voice, _| {
        voice.enabled = true;
        voice.backend = SpeechBackend::Fixture;
        voice.fixture_transcript = "status".into();
        voice.spoken_controls = true;
    });
    h.until(|s| s.voice == "ready");
    let task = h.submit();
    h.send(Command::BeginCapture);
    let activation = h.until(|s| s.activation.is_some()).activation.unwrap();
    h.send(Command::EndCapture { activation });
    h.until(|s| s.transcript_ready);
    let answer = h.send(Command::SubmitTranscript { activation });
    assert!(answer.detail.contains("spoken status"), "{}", answer.detail);
    assert_eq!(
        answer.snapshot.task.as_ref().unwrap().id,
        task,
        "a spoken control answers the current task; it never replaces it"
    );
    assert_eq!(
        answer.snapshot.activation, None,
        "the activation is consumed by the control it triggered"
    );
    assert_eq!(
        std::fs::read_dir(&h.dir).unwrap().count(),
        1,
        "no second task submission reached the task worker"
    );
    h.send(Command::CancelTask {
        task_id: task.clone(),
    });
    h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()));
    assert_eq!(h.send(Command::Status).snapshot.voice, "ready");
    h.owner.shutdown().unwrap();
}

#[test]
fn a_spoken_cancel_control_cancels_the_task_and_leaves_voice_running() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::with_voice(&[], false, |voice, _| {
        voice.enabled = true;
        voice.backend = SpeechBackend::Fixture;
        voice.fixture_transcript = "  Cancel   THE TASK! ".into();
        voice.spoken_controls = true;
    });
    h.until(|s| s.voice == "ready");
    let task = h.submit();
    let activation = h.send(Command::BeginCapture).snapshot.activation.unwrap();
    h.send(Command::EndCapture { activation });
    h.until(|s| s.transcript_ready);
    let answer = h.send(Command::SubmitTranscript { activation });
    assert!(
        answer.detail.contains("task cancellation requested"),
        "{}",
        answer.detail
    );
    let child = h.descendant(&task);
    let settled = h.until(|s| s.task.as_ref().is_some_and(|t| t.pid.is_none()) && s.retiring == 0);
    assert_eq!(settled.task.unwrap().state, TaskState::Cancelled);
    assert_eq!(
        settled.voice, "ready",
        "cancelling a task is not a voice event"
    );
    await_dead(child);
    let utterance = h
        .send(Command::Speak {
            text: "task cancelled".into(),
        })
        .snapshot
        .utterance
        .unwrap();
    h.send(Command::Interrupt { utterance });
    h.until(|s| s.utterance.is_none());
    h.owner.shutdown().unwrap();
}

#[test]
fn a_spoken_control_without_an_active_task_is_submitted_as_the_goal_it_says() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::with_voice(&[], false, |voice, _| {
        voice.enabled = true;
        voice.backend = SpeechBackend::Fixture;
        voice.fixture_transcript = "status".into();
        voice.spoken_controls = true;
    });
    h.until(|s| s.voice == "ready");
    let activation = h.send(Command::BeginCapture).snapshot.activation.unwrap();
    h.send(Command::EndCapture { activation });
    h.until(|s| s.transcript_ready);
    let submitted = h.send(Command::SubmitTranscript { activation });
    assert!(
        !submitted.detail.contains("spoken status"),
        "with no task to report, `status` is only a word: {}",
        submitted.detail
    );
    assert!(
        submitted.snapshot.task.is_some(),
        "the phrase became an ordinary goal"
    );
    h.owner.shutdown().unwrap();
    let mut silent = Harness::with_voice(&[], false, |voice, dir| {
        voice.enabled = true;
        voice.backend = SpeechBackend::Fixture;
        voice.fixture_transcript = "cancel the task".into();
        voice.spoken_controls = false;
        let _ = dir;
    });
    silent.until(|s| s.voice == "ready");
    let activation = silent
        .send(Command::BeginCapture)
        .snapshot
        .activation
        .unwrap();
    silent.send(Command::EndCapture { activation });
    silent.until(|s| s.transcript_ready);
    let answer = silent.send(Command::SubmitTranscript { activation });
    assert!(
        answer.detail.contains("accepted exactly once"),
        "with spoken controls off the transcript is a goal and nothing else: {}",
        answer.detail
    );
    silent.owner.shutdown().unwrap();
}

#[test]
fn task_state_announcements_speak_when_the_utterance_slot_is_free() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::with_voice(&[], false, |voice, dir| {
        voice.enabled = true;
        voice.backend = SpeechBackend::Fixture;
        voice.fixture_transcript = dir.to_string_lossy().into_owned();
        voice.announce_task_state = true;
    });
    h.until(|s| s.voice == "ready");
    let goal = h.dir.to_string_lossy().into_owned();
    h.send(Command::Submit { goal });
    let running = h.until(|s| s.utterance.is_some());
    assert_eq!(running.task.as_ref().unwrap().state, TaskState::Running);
    let utterance = running.utterance.unwrap();
    assert!(
        !h.request(Command::Speak {
            text: "later".into()
        })
        .accepted
    );
    h.send(Command::Interrupt { utterance });
    h.until(|s| s.utterance.is_none());
    let task = h.owner.snapshot().task.as_ref().unwrap().id.clone();
    let child = h.descendant(&task);
    release(&task);
    let settled = h.until(|s| {
        s.task.as_ref().is_some_and(|t| t.pid.is_none()) && s.utterance.is_some() && s.retiring == 0
    });
    assert_eq!(settled.task.unwrap().state, TaskState::Succeeded);
    assert_eq!(settled.voice, "ready");
    await_dead(child);
    h.owner.shutdown().unwrap();
}

#[test]
fn a_worker_stays_answerable_while_a_release_is_still_being_finalized() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::with_voice(&[], false, |voice, dir| {
        voice.enabled = true;
        voice.backend = SpeechBackend::Fixture;
        voice.fixture_transcript = dir.to_string_lossy().into_owned();
    });
    h.until(|s| s.voice == "ready");
    let activation = h.send(Command::BeginCapture).snapshot.activation.unwrap();
    h.send(Command::EndCapture { activation });
    // The release is still being answered, so the control loop is not waiting on
    // inference: it settles the capture and then still answers a ping.
    let transcript = h.until(|s| s.transcript_ready);
    assert_eq!(transcript.activation, Some(activation));
    let reply = h.send(Command::Status);
    assert_eq!(reply.snapshot.voice, "ready");
    h.send(Command::SubmitTranscript { activation });
    h.owner.shutdown().unwrap();
}

/// The transcript-only path the voice tester window drives, and the reason it exists:
/// today a spoken phrase can only become a task, so a client that wants the words has no
/// way to finish a press. Here the worker's own capture confirmation is visible, the
/// finalized words are readable straight off the snapshot, and `ClearCapture` ends the
/// press with nothing submitted -- `SubmitTranscript` is never sent.
#[test]
fn a_transcript_can_be_read_and_discarded_without_creating_a_task() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut h = Harness::with_voice(&[], false, |voice, _| {
        voice.enabled = true;
        voice.backend = SpeechBackend::Fixture;
        voice.fixture_transcript = "the quick brown fox".into();
    });
    h.until(|s| s.voice == "ready");

    // "Starting microphone..." is not a guess: the snapshot flips only on the worker's
    // own Capturing answer, which is what separates it from a press that was requested.
    h.send(Command::BeginCapture);
    let listening = h.until(|s| s.capturing);
    let activation = listening.activation.unwrap();
    assert!(
        !listening.transcript_ready,
        "nothing has been finalized while the press is still open"
    );

    // A second press while one is open is refused, not queued behind it.
    assert!(!h.request(Command::BeginCapture).accepted);

    h.send(Command::EndCapture { activation });
    let heard = h.until(|s| s.transcript.is_some());
    assert_eq!(heard.transcript.as_deref(), Some("the quick brown fox"));
    assert!(heard.transcript_ready);
    assert!(heard.task.is_none(), "a transcript is not a task");
    assert!(!heard.capturing, "the press was released");

    // Discarding keeps the words for whoever read them and frees the slot for the next
    // press without any task worker ever being started.
    let discarded = h.send(Command::ClearCapture { activation });
    assert!(
        discarded.detail.contains("nothing submitted"),
        "{}",
        discarded.detail
    );
    assert_eq!(discarded.snapshot.activation, None);
    assert_eq!(discarded.snapshot.transcript, None);
    assert!(discarded.snapshot.task.is_none());
    assert_eq!(
        std::fs::read_dir(&h.dir).unwrap().count(),
        0,
        "no task submission wrote a start file"
    );

    // The id it consumed is now stale, and a fresh press is allowed again.
    assert!(!h.request(Command::ClearCapture { activation }).accepted);
    let next = h.send(Command::BeginCapture).snapshot.activation.unwrap();
    assert_ne!(next, activation);
    h.until(|s| s.capturing);
    h.send(Command::ClearCapture { activation: next });
    h.owner.shutdown().unwrap();
}
