use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use jean_protocol::interaction::{self as wire, Envelope, Role, WorkerCommand, WorkerEvent};
use jean_protocol::TaskState;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|s| s == "--leaf") {
        println!("ready {}", std::process::id());
        let _ = io::stdin().read(&mut [0u8; 1]);
        return Ok(());
    }
    if let Some(pair) = args.windows(2).find(|a| a[0] == "--held-tool") {
        let mut leaf = Command::new(std::env::current_exe()?)
            .arg("--leaf")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut ready = String::new();
        BufReader::new(leaf.stdout.take().unwrap()).read_line(&mut ready)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(std::path::Path::new(&pair[1]).join("held.started"))?;
        writeln!(file, "{} {}", std::process::id(), leaf.id())?;
        drop(file);
        let _ = leaf.wait();
        return Ok(());
    }
    let session = args
        .windows(2)
        .find(|a| a[0] == "--session")
        .map(|a| a[1].clone())
        .ok_or_else(|| io::Error::other("session required"))?;
    let mut input = BufReader::new(io::stdin());
    let hello: Envelope<WorkerCommand> = wire::read(&mut input)?.unwrap();
    let voice = args.iter().any(|s| s == "--stale-voice");
    let role = if voice { Role::Voice } else { Role::Task };
    let send = |request, body| {
        wire::write(
            &mut io::stdout().lock(),
            &Envelope::new(session.clone(), request, wire::now_ms() + 5000, body),
        )
    };
    if args.iter().any(|s| s == "--hang-startup") {
        let _ = input.read(&mut [0u8; 1]);
        return Ok(());
    }
    send(hello.request, WorkerEvent::Ready { role })?;
    let execute: Envelope<WorkerCommand> = wire::read(&mut input)?.unwrap();
    if voice {
        send(execute.request, WorkerEvent::Configured)?;
        while let Some(request) = wire::read::<Envelope<WorkerCommand>>(&mut input)? {
            if let WorkerCommand::Speak { utterance, .. } = request.body {
                wire::write(
                    &mut io::stdout().lock(),
                    &Envelope::new(
                        "old-worker-session".into(),
                        request.request,
                        wire::now_ms() + 5000,
                        WorkerEvent::SpeechStopped { utterance },
                    ),
                )?;
                wire::write(
                    &mut io::stdout().lock(),
                    &Envelope::new(
                        session.clone(),
                        request.request,
                        0,
                        WorkerEvent::SpeechStopped { utterance },
                    ),
                )?;
                send(request.request, WorkerEvent::SpeechStarted { utterance })?;
                send(request.request, WorkerEvent::SpeechStarted { utterance })?;
            } else if let WorkerCommand::Interrupt { utterance } = request.body {
                send(request.request, WorkerEvent::SpeechStopped { utterance })?;
            } else if matches!(request.body, WorkerCommand::Shutdown) {
                send(request.request, WorkerEvent::Bye)?;
                break;
            }
        }
        return Ok(());
    }
    let WorkerCommand::Execute { task_id, goal } = execute.body else {
        return Err(io::Error::other("execute required"));
    };
    let mut leaf = Command::new(std::env::current_exe()?)
        .arg("--leaf")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut ready = String::new();
    BufReader::new(leaf.stdout.take().unwrap()).read_line(&mut ready)?;
    if !ready.starts_with("ready ") {
        return Err(io::Error::other("descendant failed its barrier"));
    }
    let event_name: Vec<_> = format!("Local\\jean-fixture-{task_id}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let release = unsafe { CreateEventW(None, true, false, PCWSTR(event_name.as_ptr())) }
        .map_err(io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(std::path::Path::new(&goal).join(format!("{task_id}.started")))?;
    writeln!(file, "{} {}", std::process::id(), leaf.id())?;
    drop(file);
    send(
        execute.request,
        WorkerEvent::TaskStarted {
            task_id: task_id.clone(),
        },
    )?;
    let (tx, rx) = mpsc::sync_channel(8);
    std::thread::spawn(move || {
        while let Ok(Some(request)) = wire::read::<Envelope<WorkerCommand>>(&mut input) {
            if tx.try_send(request).is_err() {
                break;
            }
        }
    });
    let mut state = TaskState::Succeeded;
    loop {
        if unsafe { WaitForSingleObject(release, 0) } == WAIT_OBJECT_0 {
            break;
        }
        if args.iter().any(|s| s == "--fail") {
            state = TaskState::Failed;
            break;
        }
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(message) if matches!(message.body, WorkerCommand::CancelTask { .. }) => {
                if !args.iter().any(|s| s == "--ignore-cancel") {
                    state = TaskState::Cancelled;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            _ => {}
        }
    }
    unsafe {
        let _ = CloseHandle(release);
    }
    send(
        execute.request,
        WorkerEvent::TaskFinished { task_id, state },
    )?;
    // The parent must kill descendants even when a cooperative task has already completed.
    let _ = leaf.wait();
    Ok(())
}
