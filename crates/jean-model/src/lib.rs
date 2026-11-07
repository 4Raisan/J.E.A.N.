//! Provider-neutral model access, and nothing else.
//!
//! This crate asks a model for a plan. It cannot start a process, read policy, write a
//! record, or grant anything: those crates are not in its dependency list, so "model output
//! cannot grant authority" (D-011) is a property of the graph rather than a promise in a
//! comment.
//!
//! ## The supported subset, stated exactly
//!
//! "OpenAI-compatible" is a family name, not a schema. What this crate sends and accepts is
//! deliberately narrow, and every choice below is a limitation someone hit and recorded:
//!
//! | Aspect | Supported | Not supported |
//! |---|---|---|
//! | Transports | HTTP/1.1 over TCP, plain to a loopback endpoint and **verified TLS** to any other host and port | unix sockets, proxies, HTTP/2, plain http to a non-loopback host |
//! | Providers | `ollama` (`POST /api/chat`), `openai_compatible` (`POST {base_path}/chat/completions`) | provider names that need a cloud API the operator has not authorized |
//! | Response shapes | one JSON body, chunked or with `Content-Length`, or the same body streamed as NDJSON (Ollama) / SSE `data:` lines (OpenAI-compatible) | any provider whose JSON differs |
//! | Request control | `temperature`, a token budget when the provider names it, `format`/`response_format` for JSON | tool-calling APIs, function schemas, sampling parameters the provider ignores |
//! | Credentials | a bearer token read at request time from the environment variable *named* by `model.api_key_env` | a key in a config file, a URL, a log, or a record |
//!
//! A redirect is refused rather than followed: a 3xx from an endpoint JEAN was told to talk
//! to would send the prompt, and possibly the bearer token, somewhere the operator never
//! configured — and `max_redirects` is set to `0` in the transport, so the client never asks
//! for one. TLS is verified through the Windows system trust store; nothing here can be
//! configured to skip a certificate or a host name, which is also why `model.base_url` with a
//! non-loopback `http` scheme is a load error rather than an insecure request. Those three
//! claims — a trusted chain answers, an untrusted chain does not, a wrong name does not — are
//! what the crate-internal `tls` tests (test builds only) settle against a throwaway authority
//! on loopback, with no trust store on the machine touched.
//!
//! ## Streaming, since the roadmap asked for it
//!
//! Ollama streams NDJSON and OpenAI-compatible servers stream SSE. `model.stream = true`
//! reassembles the token pieces for both, but the reassembled text is then checked by the
//! same parse and validation as a single body — a stream is a delivery detail, not a looser
//! contract. With `stream = false` (the shipped default) one bounded body is read, which is
//! the easier shape to prove. Both shapes arrive over chunked transfer encoding, which the
//! transport now decodes; the ceiling in `model.max_response_bytes` is applied while reading,
//! not from a header, so a streamed answer is bounded the same way a whole one is.
//!
//! ## Cancellation and deadlines
//!
//! Every request carries both. The deadline is `model.request_timeout_secs`, applied as a
//! global transport timeout. Cancellation is an [`std::sync::atomic::AtomicBool`] the caller
//! owns: the exchange runs on its own thread and the watching thread shuts the connection pool
//! down when the flag is set, which makes a read parked on a slow provider return at once
//! rather than at the deadline. Name resolution and the TCP connect are the two phases a
//! shutdown cannot interrupt; they are bounded by their own timeouts. A cancelled request is
//! [`ModelError::Cancelled`], never an empty plan.

pub mod error;
pub mod http;
pub mod ollama;
pub mod openai;
pub mod secret;

/// The offline HTTP stub the adapter tests talk to.
#[cfg(test)]
pub(crate) mod stub;

/// Does the transport reject a certificate it should reject? Test builds only.
#[cfg(test)]
mod tls;

pub use error::{ModelError, Result, Retryable};
pub use ollama::Ollama;
pub use openai::OpenAiCompatible;
pub use secret::Secret;

use std::sync::atomic::{AtomicBool, Ordering};

use jean_protocol::config::ModelConfig;

/// What one model call is asked to produce.
///
/// Two text fields and no structured parts: the *contract* of the answer is the plan schema in
/// `jean_protocol::plan`, and the prompt that states it is built by `jean_core::planner`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    /// Instructions that are not the operator's words: the schema, the operation catalog, and
    /// the rules the model cannot negotiate.
    pub system: String,
    /// The operator's goal, verbatim, in its own field. Kept separate from `system` so a log or
    /// a record can say which words came from whom.
    pub user: String,
    /// Ask the provider for JSON specifically, when it supports the request.
    pub prefer_json: bool,
}

impl ChatRequest {
    pub fn new(system: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            user: user.into(),
            prefer_json: true,
        }
    }

    /// Bytes that would go on the wire, used for the `model.max_prompt_bytes` check before any
    /// connection is opened.
    pub fn byte_len(&self) -> usize {
        self.system.len() + self.user.len()
    }
}

/// What one model call returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    /// The assistant's text. For a plan this is the JSON to parse; nothing here interprets it.
    pub content: String,
    /// The model name the provider answered with, when it said one.
    pub model: Option<String>,
    /// Why the provider stopped generating. A truncated answer is a failure to plan, not a
    /// shorter plan.
    pub finish_reason: Option<String>,
    /// Token accounting for the record, when the provider reports it.
    pub tokens_used: Option<u64>,
    /// Wall-clock milliseconds this call took, including retries.
    pub elapsed_ms: u64,
    /// How many transport attempts were made, so the record can say "the first two failed".
    pub attempts: u32,
}

impl ChatReply {
    /// True when the provider said it ran out of budget rather than finishing.
    pub fn truncated(&self) -> bool {
        matches!(
            self.finish_reason.as_deref(),
            Some("length") | Some("max_tokens") | Some("content_filter")
        )
    }
}

/// How a provider describes itself, for `jean model status` and for the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub name: &'static str,
    pub base_url: String,
    pub model: String,
    /// Whether a credential is available *right now*, without saying what it is or where its
    /// value came from.
    pub credential_present: bool,
    pub timeout_secs: u64,
    pub max_response_bytes: usize,
    pub max_prompt_bytes: usize,
    pub stream: bool,
    pub retries: usize,
}

/// One round trip to a model provider.
///
/// Object-safe and `Send + Sync` so `jean_core` can hold one behind an `Arc` and hand it to a
/// planner; no async, because the whole supervisor is synchronous.
pub trait ModelClient: Send + Sync {
    /// Ask for text. The caller supplies the cancellation flag, which is polled between reads.
    fn generate(&self, request: &ChatRequest, cancel: &AtomicBool) -> Result<ChatReply>;

    /// What this client is configured as, for reporting. Never includes a credential value.
    fn info(&self) -> ProviderInfo;

    /// A short name for logs and the audit trail.
    fn provider_name(&self) -> &'static str;
}

/// The `none` provider: present so `jean_core` can hold a client unconditionally and so the
/// refusal is one message with one shape.
pub struct NoProvider {
    config: ModelConfig,
}

impl NoProvider {
    pub fn new(config: &ModelConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

impl ModelClient for NoProvider {
    fn generate(&self, _request: &ChatRequest, _cancel: &AtomicBool) -> Result<ChatReply> {
        Err(ModelError::ProviderDisabled)
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "none",
            base_url: String::new(),
            model: self.config.model.clone(),
            credential_present: false,
            timeout_secs: self.config.request_timeout_secs,
            max_response_bytes: self.config.max_response_bytes,
            max_prompt_bytes: self.config.max_prompt_bytes,
            stream: false,
            retries: 0,
        }
    }

    fn provider_name(&self) -> &'static str {
        "none"
    }
}

/// Build the client configuration names. A configuration that names an unimplemented provider
/// has already been refused at load by `ModelConfig::check`, so the fall-through here is a
/// defensive branch rather than a user-facing one.
pub fn build(config: &ModelConfig) -> Result<Box<dyn ModelClient>> {
    match config.provider.as_str() {
        "none" => Ok(Box::new(NoProvider::new(config))),
        "ollama" => Ollama::new(config).map(|c| Box::new(c) as Box<dyn ModelClient>),
        "openai_compatible" => {
            OpenAiCompatible::new(config).map(|c| Box::new(c) as Box<dyn ModelClient>)
        }
        other => Err(ModelError::UnsupportedProvider(other.to_owned())),
    }
}

/// Whether the flag says to stop. Polled between socket reads.
fn cancelled(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Relaxed)
}

/// Run one model call, retrying only failures the error type itself calls retryable.
///
/// The sleeps are interruptible: a cancelled flag between attempts stops the loop instead of
/// waiting out a backoff nobody wants any more. Nothing in this function knows about tools, so it
/// cannot retry a side effect — that rule lives in `jean_core::planner`, and the split is the
/// point: a lost *answer* is safe to ask again, a lost *response to an action* is not.
pub(crate) fn retry_loop<T>(
    backoff_ms: &[u64],
    cancel: &AtomicBool,
    mut attempt: impl FnMut(u32) -> Result<T>,
) -> Result<Attempted<T>> {
    let mut tries: u32 = 0;
    loop {
        tries += 1;
        let outcome = attempt(tries);
        let failures_allowed = backoff_ms.len() as u32;
        match outcome {
            Ok(value) => {
                return Ok(Attempted {
                    value,
                    attempts: tries,
                })
            }
            Err(error) if error.is_retryable() && tries - 1 < failures_allowed => {
                let wait = backoff_ms[(tries - 1) as usize].min(60_000);
                tracing::warn!(
                    attempt = tries,
                    code = error.code(),
                    wait_ms = wait,
                    "the model request failed before producing an answer, so asking again is safe"
                );
                if cancelled(cancel) {
                    return Err(ModelError::Cancelled);
                }
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait);
                while std::time::Instant::now() < deadline {
                    if cancelled(cancel) {
                        return Err(ModelError::Cancelled);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// A successful call, plus how many attempts it took.
pub(crate) struct Attempted<T> {
    pub value: T,
    pub attempts: u32,
}

/// Refuse a prompt that is larger than configuration allows, before opening a connection.
pub(crate) fn check_prompt_size(request: &ChatRequest, limit: usize) -> Result<()> {
    let bytes = request.byte_len();
    if bytes > limit {
        return Err(ModelError::PromptTooLarge { bytes, limit });
    }
    Ok(())
}

/// Pull the assistant text out of a streamed body.
///
/// Both supported streaming shapes arrive as one body over a connection the server closes: NDJSON
/// objects for Ollama, `data:` lines for OpenAI-compatible servers. The pieces are concatenated and
/// the result goes through the same parse and the same size ceiling as a single body, because a
/// stream is a delivery detail rather than a looser contract.
pub(crate) fn assemble_stream_text(body: &str, sse: bool) -> Option<String> {
    let mut out = String::new();
    let mut found_any = false;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let payload = if sse {
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.trim();
                if rest == "[DONE]" {
                    break;
                }
                rest
            } else {
                continue;
            }
        } else {
            line
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        let piece = if sse {
            value
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
        } else {
            value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
        };
        if let Some(piece) = piece {
            out.push_str(piece);
            found_any = true;
        }
    }
    found_any.then_some(out)
}
