//! Asking a person, and the two ways a question does not get answered.

use std::io::{IsTerminal, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::decision::DenyReason;
use jean_protocol::config::PolicyConfig;

/// The question, assembled by the engine and rendered by whoever asks it.
#[derive(Debug, Clone)]
pub struct Prompt<'a> {
    pub summary: &'a str,
    pub operation: &'a str,
    pub tool_id: Option<&'a str>,
    pub tier: &'a str,
    /// The capabilities that matched `require_approval`, and so are the reason this
    /// question exists.
    pub capabilities: &'a [String],
    pub allow_remember: bool,
}

impl Prompt<'_> {
    /// The text an operator reads.
    ///
    /// Deliberately long. A prompt that can be answered without reading is a prompt
    /// that trains the operator not to read it, which is how an approval engine
    /// becomes a rubber stamp. The capability names are printed because they are
    /// what the configuration is actually keyed on.
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut text = String::new();
        let _ = writeln!(text, "Approval needed: {}", self.summary);
        let _ = writeln!(
            text,
            "  operation: {}   tier: {}",
            self.operation, self.tier
        );
        if let Some(tool) = self.tool_id {
            let _ = writeln!(text, "  tool: {tool}");
        }
        let _ = writeln!(text, "  capability: {}", self.capabilities.join(", "));
        if self.allow_remember {
            text.push_str(
                "  [y] yes, this once   [a] yes, and remember exactly these arguments   [n] no\n",
            );
        } else {
            text.push_str("  [y] yes, this once   [n] no\n");
        }
        text.push_str("> ");
        text
    }
}

/// An answer to a prompt. Every path out of a prompt is here, including the two
/// that are not a person's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    ApproveOnce,
    ApproveAndRemember,
    Denied(DenyReason),
}

/// How the engine reaches a human.
///
/// A trait, so the CLI can use [`ConsolePrompt`] while a test uses
/// [`ScriptedPrompt`] and a later tray UI can answer the same question without the
/// engine changing.
pub trait Prompter {
    /// Ask, and return the answer. Implementations must never guess: an
    /// unanswered question is a [`Answer::Denied`].
    fn ask(&mut self, prompt: &Prompt<'_>) -> Result<Answer, PolicyError>;
}

/// What came back from waiting on the terminal.
enum Read {
    Answer(String),
    /// The stream closed before answering.
    Closed,
    /// The deadline passed with the question still on screen.
    TimedOut,
}

/// Reads the operator's answer from this process's standard input.
pub struct ConsolePrompt {
    timeout: Duration,
    out: std::io::Stdout,
}

impl ConsolePrompt {
    pub fn new(config: &PolicyConfig) -> Self {
        let secs = config.approval_timeout_secs.max(1);
        Self {
            timeout: Duration::from_secs(secs),
            out: std::io::stdout(),
        }
    }

    /// One line, with a deadline.
    ///
    /// The read happens on a worker thread because a blocking terminal read cannot
    /// be interrupted from this thread, and an approval that waits forever is an
    /// approval that never times out. The timeout therefore abandons the *wait*,
    /// not the read: the worker stays parked until the terminal produces a byte or
    /// the process exits. That parked thread holds one stack and no lock, which is
    /// the cheaper of the two available bugs — the alternative is a `ReadFile` with
    /// a timeout, which needs the pipe or console handle to be opened in
    /// non-blocking mode and would change how every other reader of this terminal
    /// behaves.
    fn read_line(&self) -> Read {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                // Zero bytes read is EOF: the stream was closed or redirected away
                // mid-question. That is not an answer.
                Ok(0) => {
                    let _ = sender.send(Read::Closed);
                }
                Ok(_) => {
                    let _ = sender.send(Read::Answer(line));
                }
                Err(_) => {
                    let _ = sender.send(Read::Closed);
                }
            }
        });
        match receiver.recv_timeout(self.timeout) {
            Ok(read) => read,
            // `Disconnected` means the worker died without answering and `Timeout`
            // means it is still parked. Neither is an answer, but they are
            // different explanations, and the operator deserves the right one.
            Err(mpsc::RecvTimeoutError::Timeout) => Read::TimedOut,
            Err(mpsc::RecvTimeoutError::Disconnected) => Read::Closed,
        }
    }
}

impl Prompter for ConsolePrompt {
    fn ask(&mut self, prompt: &Prompt<'_>) -> Result<Answer, PolicyError> {
        // A prompt nobody can see cannot be answered. This is checked before the
        // question is written, because writing it to a redirected stream and then
        // waiting out the timeout would turn `jean run … > log` into a two-minute
        // stall with no explanation.
        if !std::io::stdin().is_terminal() {
            return Ok(Answer::Denied(DenyReason::Unanswerable(
                "standard input is not a terminal, so nobody is being asked".to_owned(),
            )));
        }

        write!(self.out, "{}", prompt.render()).map_err(|e| PolicyError::Io(e.to_string()))?;
        self.out
            .flush()
            .map_err(|e| PolicyError::Io(e.to_string()))?;

        let line = match self.read_line() {
            Read::Answer(line) => line,
            // D-011: a timeout is not consent, and neither is a stream that closed
            // while the question was on screen.
            Read::TimedOut => {
                return Ok(Answer::Denied(DenyReason::Unanswerable(format!(
                    "no answer within {} s; a timeout is not consent",
                    self.timeout.as_secs()
                ))));
            }
            Read::Closed => {
                return Ok(Answer::Denied(DenyReason::Unanswerable(
                    "the input stream closed before an answer".to_owned(),
                )));
            }
        };

        let answer = line.trim().to_ascii_lowercase();
        match answer.as_str() {
            "y" | "yes" => Ok(Answer::ApproveOnce),
            // Refused rather than downgraded: an operator who meant `a` and gets a
            // one-time yes has been over-ruled by a keypress they did not make.
            "a" | "always" if !prompt.allow_remember => {
                Ok(Answer::Denied(DenyReason::Unanswerable(
                    "`allow_remember` is false, so remembering is refused".to_owned(),
                )))
            }
            "a" | "always" => Ok(Answer::ApproveAndRemember),
            "n" | "no" => Ok(Answer::Denied(DenyReason::Declined)),
            // An empty line is Enter on a blank prompt. The default answer to an
            // approval question is no.
            other => Ok(Answer::Denied(DenyReason::Unanswerable(format!(
                "{other:?} is not an answer; the default is no"
            )))),
        }
    }
}

/// A prompter that replays a scripted sequence, for tests and for `--yes`-style
/// non-interactive runs.
#[derive(Debug, Default)]
pub struct ScriptedPrompt {
    pub answers: Vec<Answer>,
    pub asked: Vec<String>,
}

impl ScriptedPrompt {
    pub fn approving() -> Self {
        Self {
            answers: vec![Answer::ApproveOnce],
            asked: Vec::new(),
        }
    }

    pub fn denying() -> Self {
        Self {
            answers: vec![Answer::Denied(DenyReason::Declined)],
            asked: Vec::new(),
        }
    }

    /// Approve and remember.
    pub fn remembering() -> Self {
        Self {
            answers: vec![Answer::ApproveAndRemember],
            asked: Vec::new(),
        }
    }
}

impl Prompter for ScriptedPrompt {
    /// Consumes one scripted answer per question.
    ///
    /// Running out of scripted answers is an error rather than a silent deny: a
    /// test that expected one prompt and got two has found something, and swallowing
    /// that as a denial is how a test goes green for the wrong reason.
    fn ask(&mut self, prompt: &Prompt<'_>) -> Result<Answer, PolicyError> {
        self.asked.push(prompt.summary.to_owned());
        if self.answers.is_empty() {
            return Err(PolicyError::ScriptExhausted(prompt.summary.to_owned()));
        }
        Ok(self.answers.remove(0))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("could not reach the approval ledger: {0}")]
    Store(#[from] jean_store::StoreError),
    #[error("could not write the prompt: {0}")]
    Io(String),
    #[error("the operator answered `[a]` but policy.allow_remember is false, so it was refused")]
    RememberNotAllowed,
    #[error("could not write the audit record: {0}")]
    Audit(String),
    #[error("a prompt was raised that no scripted answer covered: {0}")]
    ScriptExhausted(String),
}
