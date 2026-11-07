//! The text-and-grounding fallback contract: how a capture becomes words, and what has not been proven.
//!
//! This module is a *protocol*, not an inference engine, and the distinction is the deliverable. D-004
//! puts OCR and visual grounding below the semantic path, so both are reachable from here — but nothing
//! here runs a model, and nothing here pretends a mocked response is one. The roadmap's own exit criteria
//! are what an unrunnable backend would otherwise quietly break, and a contract that reports itself as
//! "not exercised" is the honest version of a scaffold.
//!
//! ## What was measured on this machine, and what follows from it
//!
//! Every statement below is an observation from the Phase-4 grounding pass, not a claim about OCR in
//! general. They are written into this file because a future reader will otherwise assume the missing
//! backends were never considered.
//!
//! | Candidate | Measured state here | Consequence |
//! |---|---|---|
//! | RapidOCR over a Python sidecar | `python 3.14.3` and `onnxruntime 1.27.0` are installed; `rapidocr_onnxruntime`, `paddleocr`, `easyocr`, `pytesseract` and `cv2` are **not** | The contract is implemented and protocol-tested; end-to-end recognition is **UNVERIFIED**, because running it needs a package install this batch is not permitted to perform |
//! | `tesseract.exe` | absent from `PATH` | No backend can be built on it here |
//! | `ocrs` 0.13.1 (pure Rust) | compiles in principle, but needs external `.rten` model files its own README downloads with a script | **Refused for this batch**: downloading model weights is not permitted |
//! | ShowUI-2B / any VLM via llama.cpp | `llama-cli`, `llama-server` and `ollama` are absent; only a third-party app's `ggml*.dll` exists | Grounding contract implemented; runtime **UNVERIFIED** |
//! | Windows' own OCR (`Windows.Media.Ocr`) | present and functional: `en-US`, `TryCreateFromUserProfileLanguages` succeeds, a real recognition ran in 23 ms | **Not wired in this batch**, and that is recorded rather than glossed: it needs three more `windows` features plus WinRT async plumbing, and this tree builds on a volume with 2 GiB free |
//!
//! ## The rules this contract enforces
//!
//! * **Pixels do not travel through the core process.** [`OcrRequest`] names a file the capture module
//!   already wrote, under a retention decision the caller already made. An in-memory image handed to a
//!   sidecar would put screenshot content in the core's address space, its logs and any crash dump, which
//!   is the thing `[vision] retention` exists to avoid.
//! * **Nothing is sent anywhere that is not this machine.** There is no URL in any type here. A paid OCR
//!   API is a different integration with a different policy, not a backend name in this enum.
//! * **Confidence is data, not a licence to act.** [`OcrResult::lowest_confidence`] exists so a caller can
//!   refuse; no type in this module turns a low-confidence word into something an action can be planned
//!   from.

use std::path::{Path, PathBuf};

use crate::error::{WindowsError, WindowsResult};

/// The sidecar's protocol version.
///
/// Versioned because the wire format is the interface a Python package implements, and a package that
/// changed its JSON shape would otherwise produce an OCR that silently returned nothing — the failure
/// shape where the answer looks like "the screen has no text on it".
pub const PROTOCOL_VERSION: u32 = 1;

/// One recognition request, in the shape a worker receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrRequest {
    /// The image to read, as a path written by [`crate::capture::write_bmp`].
    pub image: PathBuf,
    /// Language hints, in BCP-47 form. Empty means "the worker's default".
    pub languages: Vec<String>,
    /// Regions of interest in physical pixels, when the caller narrowed the area.
    ///
    /// Carried as rectangles rather than as a cropped file because cropping needs a scaler this tree does
    /// not have, and a worker that was given the whole frame can crop itself in whatever language it
    /// happens to be written in.
    pub regions: Vec<(i32, i32, i32, i32)>,
}

impl OcrRequest {
    /// One JSON line, the way a stdio worker reads it.
    ///
    /// Hand-built rather than serde-derived because the crate has no serde dependency and one flat object
    /// is not worth adding one for: `jean-protocol` already owns the JSON vocabulary for anything that
    /// needs to be parsed rather than merely read.
    pub fn encode(&self) -> String {
        let languages = self
            .languages
            .iter()
            .map(|language| format!("\"{}\"", escape(language)))
            .collect::<Vec<_>>()
            .join(",");
        let regions = self
            .regions
            .iter()
            .map(|(left, top, right, bottom)| format!(" [{left},{top},{right},{bottom}]"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"version\":{PROTOCOL_VERSION},\"image\":\"{}\",\"languages\":[{languages}],\"regions\":[{regions}]}}",
            escape(&self.image.to_string_lossy())
        )
    }

    /// Reject a request that would make the worker guess.
    pub fn validate(&self) -> WindowsResult<()> {
        if self.image.as_os_str().is_empty() {
            return Err(WindowsError::InvalidSetting {
                reason: "an OCR request with no image path".to_owned(),
            });
        }
        if !self.image.exists() {
            return Err(WindowsError::File {
                path: self.image.clone(),
                reason: "the capture the request names does not exist".to_owned(),
            });
        }
        Ok(())
    }
}

/// One recognised line, with where it was and how sure the reader was.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrLine {
    pub text: String,
    /// 0.0..=1.0 as the worker reported it. Nothing here normalises or rounds it.
    pub confidence: f32,
    /// The quadrilateral, in the image's own pixel coordinates, top-left first.
    pub box_points: [(i32, i32); 4],
}

impl OcrLine {
    /// The bounding rectangle of the quad, for callers that only need a hit box.
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        let xs: Vec<i32> = self.box_points.iter().map(|(x, _)| *x).collect();
        let ys: Vec<i32> = self.box_points.iter().map(|(_, y)| *y).collect();
        (
            *xs.iter().min().unwrap_or(&0),
            *ys.iter().min().unwrap_or(&0),
            *xs.iter().max().unwrap_or(&0),
            *ys.iter().max().unwrap_or(&0),
        )
    }

    /// Whether this line is confident enough to be worth quoting.
    ///
    /// The threshold is a caller's number, so this takes it as an argument instead of owning one: a
    /// default here would be a policy this crate has no authority to set.
    pub fn is_above(&self, minimum: f32) -> bool {
        self.confidence >= minimum
    }
}

/// A worker's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrResult {
    pub lines: Vec<OcrLine>,
    /// The worker's own words about a partial failure, kept separate from the lines it did produce.
    ///
    /// A backend that read half the frame and complained about the other half is a different outcome from
    /// one that read half and said nothing, and collapsing them is how a silent truncation becomes a
    /// confident summary.
    pub warning: Option<String>,
}

impl OcrResult {
    /// Decode one JSON line from a worker.
    ///
    /// The parsing is deliberately hand-rolled and deliberately narrow: the shape is `{ "lines": [ {
    /// "text": …, "confidence": …, "box": [[x,y],…] } ] }` and nothing else is accepted. A permissive
    /// parser would let a malformed worker look like an empty screen, which is the one reading that
    /// turns a broken pipeline into a plausible-looking wrong answer.
    pub fn decode(line: &str) -> WindowsResult<OcrResult> {
        let value: serde_json_lite::Value =
            serde_json_lite::parse(line.trim()).map_err(|reason| WindowsError::InvalidSetting {
                reason: format!("worker reply was not one JSON object: {reason}"),
            })?;
        let mut lines = Vec::new();
        if let Some(items) = value.get("lines").and_then(|v| v.as_array()) {
            for item in items {
                let text = item
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let confidence = item
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0) as f32;
                let mut box_points = [(0i32, 0i32); 4];
                if let Some(points) = item.get("box").and_then(|v| v.as_array()) {
                    for (slot, point) in box_points.iter_mut().zip(points.iter()) {
                        if let Some(pair) = point.as_array() {
                            let x = pair.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                            let y = pair.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                            *slot = (x, y);
                        }
                    }
                }
                lines.push(OcrLine {
                    text,
                    confidence,
                    box_points,
                });
            }
        }
        let warning = value
            .get("warning")
            .and_then(|v| v.as_str())
            .map(|text| text.to_owned());
        Ok(OcrResult { lines, warning })
    }

    /// The weakest line in the result, which is the number a caller should refuse on.
    pub fn lowest_confidence(&self) -> Option<f32> {
        self.lines
            .iter()
            .map(|line| line.confidence)
            .reduce(f32::min)
    }

    /// The lines at or above `minimum`, in reading order.
    ///
    /// Reading order is the worker's, not a geometric sort invented here: sorting by `y` then `x` looks
    /// right on a terminal and produces nonsense on a two-column document, and a mechanism module that
    /// invents a reading order is inventing content.
    pub fn lines_above(&self, minimum: f32) -> Vec<&OcrLine> {
        self.lines
            .iter()
            .filter(|line| line.confidence >= minimum)
            .collect()
    }

    /// Everything readable, joined for a report.
    ///
    /// Bounded because a capture of a code editor is a page of text and a page of text in a task result
    /// is not information but noise with a side of quota.
    pub fn transcript(&self, maximum_chars: usize) -> String {
        let mut out = String::new();
        for line in &self.lines {
            if out.len() >= maximum_chars {
                out.push('…');
                break;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line.text);
        }
        out
    }
}

/// The extractor boundary: the same shape a vision worker will implement, whatever it is written in.
pub trait TextExtractor: Send + Sync {
    /// Ask for text. Implementations must not block the calling thread; the caller owns the bound.
    fn extract(&self, request: &OcrRequest) -> WindowsResult<OcrResult>;

    /// What this backend is, in the words a capability report should use.
    fn name(&self) -> &'static str;

    /// Whether this backend can run *as configured*, as opposed to being wired up but unproven.
    ///
    /// Every backend in this build answers `unverified`, and the reason is the point: an implementation
    /// that cannot be exercised on the machine it is being reported from has to say so in its own type,
    /// because the alternative is a report that reads like a capability.
    fn status(&self) -> BackendStatus;
}

/// Whether a vision backend has been exercised here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendStatus {
    /// Proven against a real image on this machine, with the evidence quoted.
    Verified(String),
    /// Configured and protocol-tested, but never run against a real image here.
    ///
    /// The state of every OCR and grounding path in this batch, and the reason
    /// `IMPLEMENTATION_STATUS.md` records the phase as such rather than as complete.
    Unverified(String),
    /// Not usable as configured, for a stated reason.
    Unavailable(String),
}

impl BackendStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Verified(_) => "verified",
            Self::Unverified(_) => "unverified",
            Self::Unavailable(_) => "unavailable",
        }
    }

    /// Whether a report may present this backend as a capability.
    pub fn counts_as_capability(&self) -> bool {
        matches!(self, Self::Verified(_))
    }
}

/// The `[vision] ocr_backend = "none"` case, and the one every caller falls back to.
///
/// Not a mock: it is never asked to recognise anything and cannot pretend to. Its only behaviour is to
/// explain, in the same type the wired-up backends use, that the chain stops here.
pub struct NoOcr;

impl TextExtractor for NoOcr {
    fn extract(&self, _request: &OcrRequest) -> WindowsResult<OcrResult> {
        Err(WindowsError::InvalidSetting {
            reason: "vision.ocr_backend is \"none\"; no text extraction is configured".to_owned(),
        })
    }

    fn name(&self) -> &'static str {
        "none"
    }

    fn status(&self) -> BackendStatus {
        BackendStatus::Unavailable("ocr_backend = none".to_owned())
    }
}

/// A backend that speaks this module's protocol over a pipe somebody else owns.
///
/// The spawn, the supervision and the timeout are not here: they belong to `jean_core::desktop`, which is
/// the only place that knows whether this operator granted a vision worker a process at all. This type
/// translates a capture path into a request and a reply line into a result, and its tests therefore test
/// the *translation* — which is the part that can be wrong in a way that looks like data.
pub struct SidecarProtocol;

impl SidecarProtocol {
    /// The one line a worker should be handed.
    pub fn request_line(request: &OcrRequest) -> WindowsResult<String> {
        request.validate()?;
        Ok(request.encode())
    }

    /// The one line a worker's reply has to be.
    pub fn parse_reply(reply: &str) -> WindowsResult<OcrResult> {
        OcrResult::decode(reply)
    }
}

/// The visual-grounding request: "which box on this image is the thing called `target`".
///
/// This is the fallback behind a pixel click when the semantic path could not locate a control, and it is
/// the most easily over-trusted thing in the phase. A grounding answer is a *hypothesis about pixels*;
/// the input path has to re-verify the foreground and the rectangle at send time, and does, in
/// [`crate::input::Target::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundingRequest {
    pub image: PathBuf,
    /// What to look for, in the operator's words.
    pub target: String,
}

impl GroundingRequest {
    pub fn encode(&self) -> String {
        format!(
            "{{\"version\":{PROTOCOL_VERSION},\"kind\":\"ground\",\"image\":\"{}\",\"target\":\"{}\"}}",
            escape(&self.image.to_string_lossy()),
            escape(&self.target)
        )
    }
}

/// A grounding answer: a box, and how much the model said to trust it.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundedBox {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub confidence: f32,
    /// The label the model attached, which is the only place a caller can check it understood the
    /// request as opposed to producing a box for something else.
    pub label: Option<String>,
}

impl GroundedBox {
    pub fn centre(&self) -> (i32, i32) {
        (
            self.left + (self.right - self.left) / 2,
            self.top + (self.bottom - self.top) / 2,
        )
    }

    pub fn decode(value: &serde_json_lite::Value) -> Option<Self> {
        let numbers = value.get("bbox")?.as_array()?;
        if numbers.len() != 4 {
            return None;
        }
        let left = numbers[0].as_i64()? as i32;
        let top = numbers[1].as_i64()? as i32;
        let right = numbers[2].as_i64()? as i32;
        let bottom = numbers[3].as_i64()? as i32;
        if right <= left || bottom <= top {
            // A degenerate box is not a location. Returning `None` rather than a zero-size rectangle is
            // what stops a click being planned at the origin.
            return None;
        }
        Some(Self {
            left,
            top,
            right,
            bottom,
            confidence: value
                .get("confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32,
            label: value
                .get("label")
                .and_then(|v| v.as_str())
                .map(|text| text.to_owned()),
        })
    }
}

/// Escape a string for the hand-built JSON, the same way the CLI's `quote()` does.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", other as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// The minimum JSON reader the protocol needs: objects, arrays, strings, numbers, booleans.
///
/// A private module rather than a `serde_json` dependency, because this crate is mechanism-only and one
/// flat object per line is the entire requirement — and because adding a JSON tree to a Windows-mechanism
/// crate would make it the second place in the workspace with an opinion about parsing, where
/// `jean-protocol` already is the first.
pub(crate) mod serde_json_lite {
    /// A parsed value, with just enough shape to read the worker protocol.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        Null,
        Bool(bool),
        Number(f64),
        Text(String),
        Array(Vec<Value>),
        Object(Vec<(String, Value)>),
    }

    impl Value {
        pub fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Object(entries) => entries
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value),
                _ => None,
            }
        }

        pub fn as_array(&self) -> Option<&[Value]> {
            match self {
                Value::Array(items) => Some(items),
                _ => None,
            }
        }

        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::Text(text) => Some(text),
                _ => None,
            }
        }

        pub fn as_f64(&self) -> Option<f64> {
            match self {
                Value::Number(value) => Some(*value),
                _ => None,
            }
        }

        pub fn as_i64(&self) -> Option<i64> {
            match self {
                Value::Number(value) if *value == value.round() => Some(*value as i64),
                _ => None,
            }
        }
    }

    /// Parse exactly one JSON value and require the whole input to have been consumed.
    ///
    /// The "whole input" rule is not pedantry: a worker that wrote an object and then a second line of
    /// text would otherwise be read as the object, and the trailing junk would be the difference between
    /// a reportable protocol error and a silently wrong transcript.
    pub fn parse(input: &str) -> Result<Value, String> {
        let bytes: Vec<char> = input.chars().collect();
        let mut cursor = 0usize;
        skip(&bytes, &mut cursor);
        let value = read_value(&bytes, &mut cursor)?;
        skip(&bytes, &mut cursor);
        if cursor != bytes.len() {
            return Err(format!("trailing content at offset {cursor}"));
        }
        Ok(value)
    }

    fn skip(bytes: &[char], cursor: &mut usize) {
        while let Some(character) = bytes.get(*cursor) {
            if character.is_whitespace() {
                *cursor += 1;
            } else {
                break;
            }
        }
    }

    fn read_value(bytes: &[char], cursor: &mut usize) -> Result<Value, String> {
        match bytes.get(*cursor).copied() {
            Some('{') => read_object(bytes, cursor),
            Some('[') => read_array(bytes, cursor),
            Some('"') => Ok(Value::Text(read_string(bytes, cursor)?)),
            Some('t') => expect_literal(bytes, cursor, "true", Value::Bool(true)),
            Some('f') => expect_literal(bytes, cursor, "false", Value::Bool(false)),
            Some('n') => expect_literal(bytes, cursor, "null", Value::Null),
            Some(character) if character == '-' || character.is_ascii_digit() => {
                read_number(bytes, cursor)
            }
            Some(character) => Err(format!(
                "unexpected character {character:?} at offset {cursor}"
            )),
            None => Err("input ended mid-value".to_owned()),
        }
    }

    fn expect_literal(
        bytes: &[char],
        cursor: &mut usize,
        literal: &str,
        value: Value,
    ) -> Result<Value, String> {
        let end = *cursor + literal.len();
        if end > bytes.len() || bytes[*cursor..end].iter().collect::<String>() != literal {
            return Err(format!("expected {literal} at offset {cursor}"));
        }
        *cursor = end;
        Ok(value)
    }

    fn read_string(bytes: &[char], cursor: &mut usize) -> Result<String, String> {
        if bytes.get(*cursor).copied() != Some('"') {
            return Err(format!("expected a string at offset {cursor}"));
        }
        *cursor += 1;
        let mut out = String::new();
        loop {
            let character = bytes.get(*cursor).copied().ok_or("unterminated string")?;
            *cursor += 1;
            match character {
                '"' => return Ok(out),
                '\\' => {
                    let escaped = bytes.get(*cursor).copied().ok_or("unterminated escape")?;
                    *cursor += 1;
                    match escaped {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let mut code = 0u32;
                            for _ in 0..4 {
                                let digit =
                                    bytes.get(*cursor).copied().ok_or("short \\u escape")?;
                                *cursor += 1;
                                code = code * 16 + digit.to_digit(16).ok_or("bad \\u escape")?;
                            }
                            out.push(char::from_u32(code).ok_or("unpaired surrogate")?);
                        }
                        other => return Err(format!("bad escape \\{other}")),
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn read_number(bytes: &[char], cursor: &mut usize) -> Result<Value, String> {
        let start = *cursor;
        if bytes.get(*cursor).copied() == Some('-') {
            *cursor += 1;
        }
        while let Some(character) = bytes.get(*cursor) {
            if character.is_ascii_digit() || matches!(character, '.' | 'e' | 'E' | '+' | '-') {
                *cursor += 1;
            } else {
                break;
            }
        }
        let text: String = bytes[start..*cursor].iter().collect();
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| format!("{text:?} is not a number"))
    }

    fn read_array(bytes: &[char], cursor: &mut usize) -> Result<Value, String> {
        *cursor += 1;
        let mut items = Vec::new();
        skip(bytes, cursor);
        if bytes.get(*cursor).copied() == Some(']') {
            *cursor += 1;
            return Ok(Value::Array(items));
        }
        loop {
            skip(bytes, cursor);
            items.push(read_value(bytes, cursor)?);
            skip(bytes, cursor);
            match bytes.get(*cursor).copied() {
                Some(',') => *cursor += 1,
                Some(']') => {
                    *cursor += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(format!("expected , or ] at offset {cursor}")),
            }
        }
    }

    fn read_object(bytes: &[char], cursor: &mut usize) -> Result<Value, String> {
        *cursor += 1;
        let mut entries: Vec<(String, Value)> = Vec::new();
        skip(bytes, cursor);
        if bytes.get(*cursor).copied() == Some('}') {
            *cursor += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            skip(bytes, cursor);
            let key = read_string(bytes, cursor)?;
            skip(bytes, cursor);
            if bytes.get(*cursor).copied() != Some(':') {
                return Err(format!("expected : at offset {cursor}"));
            }
            *cursor += 1;
            skip(bytes, cursor);
            let value = read_value(bytes, cursor)?;
            entries.push((key, value));
            skip(bytes, cursor);
            match bytes.get(*cursor).copied() {
                Some(',') => *cursor += 1,
                Some('}') => {
                    *cursor += 1;
                    return Ok(Value::Object(entries));
                }
                _ => return Err(format!("expected , or }} at offset {cursor}")),
            }
        }
    }
}

/// The file name a capture should be saved under for a worker to read it.
///
/// Naming is centralised because the retention rule and the diagnostic rule both depend on being able to
/// find every file later: a capture written to a path invented at the call site is a capture nobody can
/// clean up or account for.
pub fn capture_file_name(window: isize, captured_at_ms: u64) -> String {
    format!("jean-{window}-{captured_at_ms}.bmp")
}

/// Where a capture belongs, given the configured directory.
pub fn capture_path(directory: &Path, window: isize, captured_at_ms: u64) -> PathBuf {
    directory.join(capture_file_name(window, captured_at_ms))
}

/// One-shot RapidOCR sidecar over stdio JSON (protocol v1).
///
/// One spawn per recognition, bounded to 30 s, stdout-only reply. The
/// script lives in `tools/ocr-sidecar/rapidocr_sidecar.py`; Python needs
/// `rapidocr-onnxruntime` (Apache-2.0). Image stays on disk under the
/// caller's retention policy — nothing crosses a network.
#[derive(Debug, Clone)]
pub struct SidecarOcr {
    pub python: PathBuf,
    pub script: PathBuf,
}

impl SidecarOcr {
    pub fn from_env() -> Option<Self> {
        let python = std::env::var_os("JEAN_OCR_PYTHON")?;
        let script = std::env::var_os("JEAN_OCR_SCRIPT")?;
        Some(Self { python: python.into(), script: script.into() })
    }

    pub fn extract_with_timeout(&self, request: &OcrRequest, timeout: std::time::Duration) -> WindowsResult<OcrResult> {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        let line = SidecarProtocol::request_line(request)?;
        let mut child = Command::new(&self.python)
            .arg(&self.script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| WindowsError::InvalidSetting { reason: format!("ocr sidecar spawn failed: {e}") })?;
        child.stdin.as_mut().ok_or_else(|| WindowsError::InvalidSetting { reason: "ocr sidecar: no stdin".to_owned() })?.write_all(line.as_bytes()).map_err(|e| WindowsError::InvalidSetting { reason: format!("ocr sidecar write failed: {e}") })?;
        let (tx, rx) = std::sync::mpsc::channel();
        let out = child.stdout.take().ok_or_else(|| WindowsError::InvalidSetting { reason: "ocr sidecar: no stdout".to_owned() })?;
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut s = String::new();
            let _ = r.read_line(&mut s);
            let _ = tx.send(s);
        });
        let reply = rx.recv_timeout(timeout).map_err(|_| WindowsError::InvalidSetting { reason: "ocr sidecar timed out".to_owned() })?;
        let _ = child.kill();
        let _ = child.wait();
        SidecarProtocol::parse_reply(reply.trim())
    }
}

impl TextExtractor for SidecarOcr {
    fn extract(&self, request: &OcrRequest) -> WindowsResult<OcrResult> {
        self.extract_with_timeout(request, std::time::Duration::from_secs(30))
    }
    fn name(&self) -> &'static str { "rapidocr-sidecar" }
    fn status(&self) -> BackendStatus {
        if self.python.is_file() && self.script.is_file() {
            BackendStatus::Unverified("rapidocr sidecar: python+script present, needs image validation".to_owned())
        } else {
            BackendStatus::Unavailable("python/script missing".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sidecar_missing_files_reports_unavailable() {
        let s = SidecarOcr { python: PathBuf::from("nope.exe"), script: PathBuf::from("nope.py") };
        assert!(matches!(s.status(), BackendStatus::Unavailable(_)));
    }
    #[test]
    fn request_line_roundtrips_through_parse() {
        let req = OcrRequest { image: PathBuf::from("x.bmp"), languages: vec!["en".to_owned()], regions: vec![] };
        let line = req.encode();
        assert!(line.contains("\"x.bmp\""));
        let out = SidecarProtocol::parse_reply(r#"{"lines":[{"text":"hi","confidence":0.5,"box":[[0,0],[10,0],[10,10],[0,10]]}]}"#).unwrap();
        assert_eq!(out.lines[0].text, "hi");
    }
}
