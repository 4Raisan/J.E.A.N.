use std::io::{self, BufReader};
use std::sync::mpsc;

use jean_policy::prompt::Prompt;
use jean_policy::{Answer, DenyReason, Prompter, RunAuthority};
use jean_protocol::interaction::{
    self as wire, Envelope, RequestGate, Role, WorkerCommand as Command, WorkerEvent as Event,
};
use jean_protocol::TaskState;

use crate::{plan_runner, CancelFlag, Context};

struct NoInteractiveApproval;
impl Prompter for NoInteractiveApproval {
    fn ask(&mut self, _prompt: &Prompt<'_>) -> Result<Answer, jean_policy::PolicyError> {
        Ok(Answer::Denied(DenyReason::Unanswerable("this control session cannot answer an uncovered approval; use the existing CLI approval surface".into())))
    }
}

pub fn serve(session: String) -> io::Result<()> {
    let mut input = BufReader::new(io::stdin());
    let mut gate = RequestGate::new(session.clone());
    let hello: Envelope<Command> =
        wire::read(&mut input)?.ok_or_else(|| io::Error::other("missing handshake"))?;
    gate.accept(&hello).map_err(io::Error::other)?;
    if !matches!(hello.body, Command::Hello { role: Role::Task }) {
        return Err(io::Error::other("task handshake required"));
    }
    let reply = |request, body| {
        wire::write(
            &mut io::stdout().lock(),
            &Envelope::new(session.clone(), request, wire::now_ms() + 2000, body),
        )
    };
    reply(hello.request, Event::Ready { role: Role::Task })?;
    let execute: Envelope<Command> =
        wire::read(&mut input)?.ok_or_else(|| io::Error::other("missing task"))?;
    gate.accept(&execute).map_err(io::Error::other)?;
    let Command::Execute { task_id, goal } = execute.body else {
        return Err(io::Error::other("expected task execution"));
    };
    jean_protocol::ipc::validate_task_id(&task_id).map_err(io::Error::other)?;
    if !wire::text_valid(&goal) {
        return Err(io::Error::other("invalid goal"));
    }
    let cancel = CancelFlag::new();
    let listener_cancel = cancel.clone();
    let owned_id = task_id.clone();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("task-owner-control".into())
        .spawn(move || {
            loop {
                match wire::read::<Envelope<Command>>(&mut input) {
                    Ok(Some(message)) => {
                        if gate.accept(&message).is_err() {
                            continue;
                        }
                        match message.body {
                            Command::CancelTask { task_id } if task_id == owned_id => {
                                listener_cancel.raise();
                                crate::signals::cancel_owned_task();
                            }
                            Command::Shutdown => {
                                listener_cancel.raise();
                                crate::signals::cancel_owned_task();
                                break;
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        listener_cancel.raise();
                        crate::signals::cancel_owned_task();
                        break;
                    }
                }
            }
            let _ = done_tx.send(());
        })?;
    reply(
        execute.request,
        Event::TaskStarted {
            task_id: task_id.clone(),
        },
    )?;
    let result = (|| -> crate::Result<_> {
        let exe = std::env::current_exe().ok();
        let mut ctx = Context::load(exe.as_deref().and_then(|p| p.parent()))?;
        crate::reconcile(&ctx.store)?;
        let outcome = (|| -> crate::Result<_> {
            let model = crate::build_model(&ctx.config.model)?;
            plan_runner::run_owned(
                &mut ctx,
                model.as_ref(),
                (&task_id, &goal),
                &mut NoInteractiveApproval,
                &cancel,
                &mut |_| {},
                RunAuthority::OperatorRun,
            )
        })();
        match outcome {
            Ok(report) => Ok(report.state),
            Err(error) => {
                let state = if cancel.raised() {
                    TaskState::Cancelled
                } else {
                    TaskState::Failed
                };
                let detail = ctx.redactor.redact(&error.to_string());
                if ctx.store.task(&task_id)?.is_none() {
                    ctx.store.insert_task(&jean_store::TaskRecord::new(
                        task_id.clone(),
                        ctx.redactor.redact(&goal),
                        Some("plan".into()),
                        "{\"intent\":\"plan\"}".into(),
                    ))?;
                }
                if !ctx.store.require_task(&task_id)?.state.is_terminal() {
                    ctx.store.transition(&task_id, state, Some(&detail))?;
                    ctx.store.clear_runner(&task_id)?;
                }
                Ok(ctx.store.require_task(&task_id)?.state)
            }
        }
    })();
    let state = match result {
        Ok(state) => state,
        Err(_) => TaskState::NeedsAttention,
    };
    reply(execute.request, Event::TaskFinished { task_id, state })?;
    let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
    Ok(())
}
