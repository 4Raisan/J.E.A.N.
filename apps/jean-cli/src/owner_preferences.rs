use clap::Subcommand;
use std::path::Path;

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "owner-preferences.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    entries: Vec<Entry>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Record explicit operator feedback about animation on a completed task.
    Feedback {
        workspace: String,
        task_id: String,
        value: Value,
    },
    /// Inspect evidence and any pending candidate for a workspace.
    Status { workspace: String },
    /// Accept a repeated-feedback candidate.
    Accept { workspace: String },
    /// Correct and directly set the workspace preference.
    Correct { workspace: String, value: Value },
    /// Forget evidence, candidate and accepted preference.
    Forget { workspace: String },
    /// Enable or disable learning for one workspace.
    Learning {
        workspace: String,
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: bool,
    },
}

#[derive(clap::ValueEnum, Clone)]
pub enum Value {
    NoAnimation,
    Animation,
}

pub fn command(data_dir: &Path, store: &jean_store::Store, command: Command) -> anyhow::Result<()> {
    let (workspace, action) = match command {
        Command::Feedback {
            workspace,
            task_id,
            value,
        } => {
            let workspace = canonical_workspace(&workspace)?;
            let task = store.task(&task_id)?.ok_or_else(|| {
                anyhow::anyhow!("feedback task id does not identify a recorded JEAN task")
            })?;
            if task.finished_at_ms.is_none()
                || !store
                    .delegations_for_task(&task_id)?
                    .iter()
                    .any(|delegation| {
                        canonical_workspace(&delegation.workspace)
                            .is_ok_and(|scope| scope == workspace)
                    })
            {
                anyhow::bail!("feedback requires a completed delegation in this workspace");
            }
            let mut state = State::load(data_dir)?;
            state.feedback(&workspace, &task_id, matches!(value, Value::NoAnimation))?;
            state.save(data_dir)?;
            println!("{}", state.status(&workspace));
            return Ok(());
        }
        Command::Status { workspace } => (canonical_workspace(&workspace)?, Action::Status),
        Command::Accept { workspace } => (canonical_workspace(&workspace)?, Action::Accept),
        Command::Correct { workspace, value } => (
            canonical_workspace(&workspace)?,
            Action::Correct(matches!(value, Value::NoAnimation)),
        ),
        Command::Forget { workspace } => (canonical_workspace(&workspace)?, Action::Forget),
        Command::Learning { workspace, enabled } => {
            (canonical_workspace(&workspace)?, Action::Learning(enabled))
        }
    };
    let mut state = State::load(data_dir)?;
    match action {
        Action::Status => {}
        Action::Accept => state.accept(&workspace)?,
        Action::Correct(value) => state.correct(&workspace, value),
        Action::Forget => state.forget(&workspace),
        Action::Learning(enabled) => state.learning(&workspace, enabled),
    }
    if !matches!(action, Action::Status) {
        state.save(data_dir)?;
    }
    println!("{}", state.status(&workspace));
    Ok(())
}

#[derive(Clone, Copy)]
enum Action {
    Status,
    Accept,
    Correct(bool),
    Forget,
    Learning(bool),
}

fn canonical_workspace(workspace: &str) -> anyhow::Result<String> {
    Ok(std::path::Path::new(workspace)
        .canonicalize()?
        .display()
        .to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    workspace: String,
    learning: bool,
    accepted_no_animation: bool,
    feedback: Vec<Feedback>,
    candidate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Feedback {
    task_id: String,
    value: bool,
}

impl State {
    fn entry(&mut self, workspace: &str) -> &mut Entry {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.workspace == workspace)
        {
            return &mut self.entries[index];
        }
        self.entries.push(Entry {
            workspace: workspace.to_owned(),
            learning: true,
            accepted_no_animation: false,
            feedback: Vec::new(),
            candidate: false,
        });
        self.entries.last_mut().expect("entry was inserted")
    }

    pub fn load(data_dir: &Path) -> anyhow::Result<Self> {
        let path = data_dir.join(FILE_NAME);
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)?;
        if bytes.len() > 256 * 1024 {
            anyhow::bail!("owner preference state exceeds 256 KiB");
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        let path = data_dir.join(FILE_NAME);
        let temp = data_dir.join(format!(".{FILE_NAME}.tmp"));
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() > 256 * 1024 {
            anyhow::bail!("owner preference state exceeds 256 KiB");
        }
        std::fs::write(&temp, bytes)?;
        std::fs::rename(temp, path)?;
        Ok(())
    }

    pub fn feedback(
        &mut self,
        workspace: &str,
        task_id: &str,
        no_animation: bool,
    ) -> anyhow::Result<()> {
        jean_protocol::ipc::validate_task_id(task_id)?;
        let entry = self.entry(workspace);
        if !entry.learning {
            anyhow::bail!("learning is disabled for this workspace");
        }
        entry
            .feedback
            .retain(|feedback| feedback.task_id != task_id);
        entry.feedback.push(Feedback {
            task_id: task_id.to_owned(),
            value: no_animation,
        });
        if entry.feedback.len() > 32 {
            entry.feedback.remove(0);
        }
        entry.candidate = entry
            .feedback
            .iter()
            .filter(|feedback| feedback.value)
            .map(|feedback| &feedback.task_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            >= 2;
        Ok(())
    }

    pub fn accept(&mut self, workspace: &str) -> anyhow::Result<()> {
        let entry = self.entry(workspace);
        if !entry.candidate {
            anyhow::bail!("no repeated-feedback candidate exists for this workspace");
        }
        entry.accepted_no_animation = entry.candidate;
        entry.candidate = false;
        Ok(())
    }
    pub fn correct(&mut self, workspace: &str, no_animation: bool) {
        let entry = self.entry(workspace);
        entry.accepted_no_animation = no_animation;
        entry.candidate = false;
        entry.feedback.clear();
    }
    pub fn forget(&mut self, workspace: &str) {
        let entry = self.entry(workspace);
        entry.accepted_no_animation = false;
        entry.candidate = false;
        entry.feedback.clear();
    }
    pub fn learning(&mut self, workspace: &str, enabled: bool) {
        self.entry(workspace).learning = enabled;
    }
    pub fn applies(&self, workspace: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.workspace == workspace && e.accepted_no_animation)
    }
    pub fn status(&self, workspace: &str) -> String {
        match self.entries.iter().find(|e| e.workspace == workspace) {
            Some(e) => format!(
                "workspace: {}\nlearning: {}\naccepted no-animation: {}\ncandidate: {}\nevidence: {}",
                e.workspace,
                e.learning,
                e.accepted_no_animation,
                e.candidate,
                e.feedback
                    .iter()
                    .map(|f| format!(
                        "{}={}",
                        f.task_id,
                        if f.value { "no-animation" } else { "animation" }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => format!(
                "workspace: {workspace}\nlearning: true\naccepted no-animation: false\ncandidate: false\nevidence: 0 distinct task(s)"
            ),
        }
    }
}
