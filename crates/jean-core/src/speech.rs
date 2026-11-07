//! Spoken interaction policy: what a released transcript may control, and what
//! the task plane is allowed to say back.
//!
//! Both halves are deliberately narrow. A transcript only reaches here after an
//! explicit push-to-talk release *and* an explicit submission, so ambient speech
//! and TTS playback can never arrive as a command; and an announcement is output
//! only — it never authorises anything.

use std::collections::VecDeque;

use jean_protocol::TaskState;

/// Ceiling on queued announcements. A long task that flaps between states must
/// not build up a backlog of speech that outlives the task it describes.
pub const MAX_QUEUED_ANNOUNCEMENTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpokenControl {
    Status,
    CancelTask,
}

/// Exact-match spoken controls, honoured only while a task is active.
///
/// Matching is exact after normalisation, never fuzzy: a recogniser that hears
/// "cancel" inside a longer goal must not cancel anything, and a goal that merely
/// mentions status must still be submitted as a goal. Cancellation and reporting
/// are the only two effects available here — a spoken phrase can never start work.
pub fn spoken_control(text: &str, task_active: bool) -> Option<SpokenControl> {
    if !task_active {
        return None;
    }
    let normalized = normalize(text);
    match normalized.as_str() {
        "status" | "task status" | "report status" | "what is the status" => {
            Some(SpokenControl::Status)
        }
        "cancel" | "cancel task" | "cancel the task" | "stop task" | "stop the task" => {
            Some(SpokenControl::CancelTask)
        }
        _ => None,
    }
}

fn normalize(text: &str) -> String {
    let trimmed = text.trim().trim_end_matches(['.', '!', '?', ',']).trim();
    let mut out = String::with_capacity(trimmed.len());
    for word in trimmed.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().map(|c| c.to_ascii_lowercase()));
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Announcement {
    task_id: String,
    state: TaskState,
    text: String,
}

/// Bounded, de-duplicated progress speech for the task plane.
///
/// Stale entries are dropped at drain time rather than at enqueue time, because
/// "stale" means "no longer the current task" and only the controller knows which
/// task that is when the utterance slot frees up.
pub struct Announcements {
    enabled: bool,
    queue: VecDeque<Announcement>,
    last_spoken: Option<(String, TaskState)>,
    suppressed: usize,
}

impl Announcements {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            queue: VecDeque::new(),
            last_spoken: None,
            suppressed: 0,
        }
    }

    pub fn observe(&mut self, task_id: &str, state: TaskState) {
        if !self.enabled {
            return;
        }
        let Some(text) = phrase(state) else {
            return;
        };
        let key = (task_id.to_owned(), state);
        if self.last_spoken.as_ref() == Some(&key)
            || self
                .queue
                .iter()
                .any(|a| (a.task_id.as_str(), a.state) == (task_id, state))
        {
            self.suppressed += 1;
            return;
        }
        if self.queue.len() >= MAX_QUEUED_ANNOUNCEMENTS {
            self.queue.pop_front();
            self.suppressed += 1;
        }
        self.queue.push_back(Announcement {
            task_id: task_id.to_owned(),
            state,
            text: text.to_owned(),
        });
    }

    /// The next announcement worth speaking, given the task that is current now.
    pub fn next(&mut self, current_task: Option<&str>) -> Option<String> {
        if !self.enabled {
            return None;
        }
        while let Some(entry) = self.queue.pop_front() {
            if current_task != Some(entry.task_id.as_str()) {
                self.suppressed += 1;
                continue;
            }
            self.last_spoken = Some((entry.task_id, entry.state));
            return Some(entry.text);
        }
        None
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.last_spoken = None;
    }

    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    pub fn suppressed(&self) -> usize {
        self.suppressed
    }
}

fn phrase(state: TaskState) -> Option<&'static str> {
    match state {
        TaskState::Running => Some("Task running."),
        TaskState::Succeeded => Some("Task succeeded."),
        TaskState::Failed => Some("Task failed."),
        TaskState::Cancelled => Some("Task cancelled."),
        TaskState::NeedsAttention => Some("Task needs attention."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spoken_control_needs_an_active_task_and_an_exact_phrase() {
        assert_eq!(spoken_control("status", false), None);
        assert_eq!(spoken_control("cancel task", false), None);
        assert_eq!(
            spoken_control("  STATUS.  ", true),
            Some(SpokenControl::Status)
        );
        assert_eq!(
            spoken_control("Cancel   the task!", true),
            Some(SpokenControl::CancelTask)
        );
        assert_eq!(
            spoken_control("cancel the task and delete everything", true),
            None
        );
        assert_eq!(
            spoken_control("what is the status of the backup", true),
            None
        );
        assert_eq!(spoken_control("status report", true), None);
    }

    #[test]
    fn announcements_deduplicate_bound_and_drop_stale_entries() {
        let mut announcements = Announcements::new(true);
        for _ in 0..10 {
            announcements.observe("t1", TaskState::Running);
        }
        assert_eq!(announcements.pending(), 1);
        assert_eq!(announcements.suppressed(), 9);
        for state in [
            TaskState::Succeeded,
            TaskState::Failed,
            TaskState::Cancelled,
            TaskState::NeedsAttention,
            TaskState::Running,
        ] {
            announcements.observe("t2", state);
        }
        assert!(announcements.pending() <= MAX_QUEUED_ANNOUNCEMENTS);
        assert_eq!(
            announcements.next(Some("t1")),
            None,
            "t2 speech is stale for t1"
        );
        announcements.observe("t2", TaskState::Running);
        assert_eq!(
            announcements.next(Some("t2")).as_deref(),
            Some("Task running.")
        );
        assert_eq!(
            announcements.next(Some("t2")),
            None,
            "the same state is not repeated"
        );
    }

    #[test]
    fn disabled_announcements_queue_nothing() {
        let mut announcements = Announcements::new(false);
        announcements.observe("t1", TaskState::Running);
        assert_eq!(announcements.pending(), 0);
        assert_eq!(announcements.next(Some("t1")), None);
    }
}
