use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::config::VoiceConfig;
use crate::TaskState;

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 16 * 1024;
pub const MAX_TEXT: usize = 4096;
pub const QUEUE_CAPACITY: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub version: u32,
    pub session: String,
    pub request: u64,
    pub deadline_ms: u64,
    pub body: T,
}

impl<T> Envelope<T> {
    pub fn new(session: String, request: u64, deadline_ms: u64, body: T) -> Self {
        Self {
            version: VERSION,
            session,
            request,
            deadline_ms,
            body,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Task,
    Voice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCommand {
    Hello { role: Role },
    ConfigureVoice { config: Box<VoiceConfig> },
    Execute { task_id: String, goal: String },
    CancelTask { task_id: String },
    Speak { utterance: u64, text: String },
    Interrupt { utterance: u64 },
    BeginCapture { activation: u64 },
    EndCapture { activation: u64 },
    Ping,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerEvent {
    Ready { role: Role },
    Configured,
    TaskStarted { task_id: String },
    TaskFinished { task_id: String, state: TaskState },
    SpeechStarted { utterance: u64 },
    SpeechStopped { utterance: u64 },
    Capturing { activation: u64 },
    Transcript { activation: u64, text: String },
    Pong,
    Bye,
    Refused { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerCommand {
    Submit {
        goal: String,
    },
    Status,
    CancelTask {
        task_id: String,
    },
    Speak {
        text: String,
    },
    Interrupt {
        utterance: u64,
    },
    StartVoice,
    StopVoice,
    BeginCapture,
    EndCapture {
        activation: u64,
    },
    /// Drop a capture without keeping or submitting its transcript. This is the only
    /// way an operator-facing tester can finish a press and start another: the words it
    /// heard are discarded here rather than turning into a task.
    ClearCapture {
        activation: u64,
    },
    SubmitTranscript {
        activation: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub state: TaskState,
    pub pid: Option<u32>,
    pub cancelling: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub session: String,
    pub task: Option<TaskView>,
    pub voice: String,
    pub voice_session: Option<String>,
    pub voice_pid: Option<u32>,
    pub utterance: Option<u64>,
    pub activation: Option<u64>,
    /// True only once the voice worker has confirmed the capture stream actually opened.
    /// `activation` is set the moment a press is requested, so without this a client
    /// cannot tell "starting the microphone" from "listening", and would show the second
    /// one first.
    pub capturing: bool,
    pub transcript_ready: bool,
    /// The finalized words for the active capture, if there are any. Reading them here is
    /// the transcript-only path: `transcript_ready` says a phrase exists, this says what it
    /// was, and neither has submitted anything. `SubmitTranscript` remains the only command
    /// that turns a spoken phrase into a task.
    pub transcript: Option<String>,
    pub shutting_down: bool,
    pub retiring: usize,
    pub rejected_events: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerReply {
    pub accepted: bool,
    pub detail: String,
    pub snapshot: Snapshot,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn text_valid(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_TEXT && !text.contains('\0')
}

pub fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() >= MAX_FRAME {
        return Err(io::Error::other("IPC frame exceeds 16 KiB"));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

pub const UTF8_PREAMBLE: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// One IPC frame, or `None` at a clean EOF.
///
/// A leading UTF-8 encoding preamble is dropped rather than rejected: a Windows client
/// that writes its stdin through a default UTF-8 text writer puts those three bytes in
/// front of the very first frame without being able to stop, and a frame that is otherwise
/// valid is not a reason to close the session.
pub fn read<T: DeserializeOwned>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::other("truncated IPC frame"))
            };
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if bytes.len() + count > MAX_FRAME {
            return Err(io::Error::other("IPC frame exceeds 16 KiB"));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if bytes.last() == Some(&b'\n') {
            if bytes.starts_with(&UTF8_PREAMBLE) {
                bytes.drain(..UTF8_PREAMBLE.len());
            }
            return serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(io::Error::other);
        }
    }
}

pub fn write<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    writer.write_all(&encode(value)?)?;
    writer.flush()
}

pub struct RequestGate {
    session: String,
    last: u64,
}

impl RequestGate {
    pub fn new(session: String) -> Self {
        Self { session, last: 0 }
    }
    pub fn accept<T>(&mut self, message: &Envelope<T>) -> Result<(), &'static str> {
        if message.version != VERSION || message.session != self.session {
            return Err("stale session or protocol version");
        }
        if message.request == 0 || message.request <= self.last {
            return Err("duplicate or out-of-order request");
        }
        self.last = message.request;
        if now_ms() > message.deadline_ms {
            return Err("request deadline expired");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_stale_duplicate_expired_and_versioned_requests() {
        let mut gate = RequestGate::new("current".into());
        let mut m = Envelope::new("old".into(), 1, now_ms() + 10000, OwnerCommand::Status);
        assert!(gate.accept(&m).is_err());
        m.session = "current".into();
        m.version += 1;
        assert!(gate.accept(&m).is_err());
        m.version = VERSION;
        assert!(gate.accept(&m).is_ok());
        assert!(gate.accept(&m).is_err());
        m.request = 2;
        m.deadline_ms = 0;
        assert!(gate.accept(&m).is_err());
        m.deadline_ms = now_ms() + 10000;
        assert!(gate.accept(&m).is_err());
    }
    #[test]
    fn bounded_frames_reject_oversize_partial_and_unknown_operations() {
        assert!(encode(&"a".repeat(MAX_FRAME)).is_err());
        assert!(read::<OwnerCommand>(&mut &b"{\"op\":\"submit\"}"[..]).is_err());
        assert!(read::<OwnerCommand>(&mut &b"{\"op\":\"ambient_submit\"}\n"[..]).is_err());
        assert!(read::<OwnerCommand>(&mut vec![b'a'; MAX_FRAME + 1].as_slice()).is_err());
    }
    #[test]
    fn a_leading_encoding_preamble_does_not_reject_the_frame() {
        let mut stream = UTF8_PREAMBLE.to_vec();
        stream.extend_from_slice(&encode(&OwnerCommand::BeginCapture).unwrap());
        stream.extend_from_slice(&encode(&OwnerCommand::Status).unwrap());
        // One reader across both frames: the preamble fronts the stream once, so the second
        // frame has to decode with nothing stripped a second time.
        let mut cursor = stream.as_slice();
        assert!(matches!(
            read::<OwnerCommand>(&mut cursor).unwrap(),
            Some(OwnerCommand::BeginCapture)
        ));
        assert!(matches!(
            read::<OwnerCommand>(&mut cursor).unwrap(),
            Some(OwnerCommand::Status)
        ));
        assert!(read::<OwnerCommand>(&mut &b"\xef\xbb\xbf{\"op\":\"nonsense\"}\n"[..]).is_err());
    }

    #[test]
    fn the_exact_frames_a_powershell_client_emits_decode() {
        // Byte for byte what the voice tester writes, captured from its own `Send-Owner` body by
        // `target/p5t-frames.ps1`, with the preamble that client's stream writer supplies in
        // front. `Envelope` denies unknown fields, so this also pins the key names and the
        // number-vs-string shapes a hand-built client has to get right. A deadline is not
        // consulted here: decoding is the claim, `RequestGate` runs after it.
        const FRAMES: &str = concat!(
            "{\"version\":1,\"session\":\"t-18e5d8df3ad16618-1304-0\",\"request\":2,",
            "\"deadline_ms\":1794080109122,\"body\":{\"op\":\"begin_capture\"}}\n",
            "{\"version\":1,\"session\":\"t-18e5d8df3ad16618-1304-0\",\"request\":3,",
            "\"deadline_ms\":1794080109133,\"body\":{\"op\":\"end_capture\",\"activation\":7}}\n",
            "{\"version\":1,\"session\":\"t-18e5d8df3ad16618-1304-0\",\"request\":4,",
            "\"deadline_ms\":1794080109134,\"body\":{\"op\":\"clear_capture\",\"activation\":7}}\n",
            "{\"version\":1,\"session\":\"t-18e5d8df3ad16618-1304-0\",\"request\":6,",
            "\"deadline_ms\":1794080109138,\"body\":{\"op\":\"interrupt\",\"utterance\":9}}\n",
        );
        let mut bytes = UTF8_PREAMBLE.to_vec();
        bytes.extend_from_slice(FRAMES.as_bytes());
        let mut cursor = bytes.as_slice();
        let bodies: Vec<OwnerCommand> = (0..FRAMES.matches('\n').count())
            .map(|_| {
                read::<Envelope<OwnerCommand>>(&mut cursor)
                    .expect("a client frame must decode, not close the session")
                    .expect("a frame is present")
                    .body
            })
            .collect();
        assert!(matches!(bodies[0], OwnerCommand::BeginCapture));
        assert!(matches!(
            bodies[1],
            OwnerCommand::EndCapture { activation: 7 }
        ));
        assert!(matches!(
            bodies[2],
            OwnerCommand::ClearCapture { activation: 7 }
        ));
        assert!(matches!(
            bodies[3],
            OwnerCommand::Interrupt { utterance: 9 }
        ));
    }
}
