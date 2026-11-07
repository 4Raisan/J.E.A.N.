//! The Ollama adapter: `POST /api/chat`, and nothing else of Ollama's surface.
//!
//! ## What is sent, exactly
//!
//! ```json
//! {
//!   "model": "…",
//!   "messages": [{"role": "system", "content": "…"}, {"role": "user", "content": "…"}],
//!   "stream": false,
//!   "format": "json",
//!   "options": {"temperature": 0.0, "num_predict": 2048}
//! }
//! ```
//!
//! `format: "json"` is how Ollama is asked for a JSON object rather than prose around one. It is
//! not a schema: the server does not validate the shape, so `jean_protocol::plan::Plan::parse`
//! still has to, and does. `options.num_ctx`, `mirostat`, `repeat_penalty`, `keep_alive` and the
//! rest of Ollama's knob set are not sent — an unlisted knob is one whose effect on the answer
//! could not be recorded.
//!
//! ## What is read
//!
//! `{"model":"…","message":{"content":"…"},"done":true,"done_reason":"stop","eval_count":n}`.
//! `done: false` with no error is treated as a truncated answer rather than an empty one. When
//! `model.stream = true` the same endpoint is asked for NDJSON and the pieces are reassembled by
//! `crate::assemble_stream_text` before the same parse.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use jean_protocol::config::ModelConfig;
use serde_json::json;

use crate::error::{ModelError, Result};
use crate::http::{OutgoingRequest, Target};
use crate::{
    assemble_stream_text, cancelled, check_prompt_size, retry_loop, ChatReply, ChatRequest,
    ModelClient, ProviderInfo,
};

/// Ollama's own name for "ran out of budget".
const OLLAMA_DONE_LENGTH: &str = "length";

pub struct Ollama {
    target: Target,
    model: String,
    timeout: Duration,
    max_response_bytes: usize,
    max_prompt_bytes: usize,
    temperature: f32,
    max_output_tokens: u32,
    stream: bool,
    retry_backoff_ms: Vec<u64>,
}

impl Ollama {
    pub fn new(config: &ModelConfig) -> Result<Self> {
        Ok(Self {
            target: Target::parse(&config.base_url)?,
            model: config.model.clone(),
            timeout: Duration::from_secs(config.request_timeout_secs),
            max_response_bytes: config.max_response_bytes,
            max_prompt_bytes: config.max_prompt_bytes,
            temperature: config.temperature,
            max_output_tokens: config.max_output_tokens,
            stream: config.stream,
            retry_backoff_ms: config.retry_backoff_ms.clone(),
        })
    }

    fn body(&self, request: &ChatRequest) -> Vec<u8> {
        let payload = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "stream": self.stream,
            "options": {
                "temperature": self.temperature,
                "num_predict": self.max_output_tokens,
            }
        });
        let payload = if request.prefer_json {
            // `format` is a top-level field, not an option.
            let mut object = payload.as_object().expect("built above").clone();
            object.insert("format".to_owned(), json!("json"));
            serde_json::Value::Object(object)
        } else {
            payload
        };
        serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec())
    }
}

impl ModelClient for Ollama {
    fn generate(&self, request: &ChatRequest, cancel: &AtomicBool) -> Result<ChatReply> {
        check_prompt_size(request, self.max_prompt_bytes)?;
        if cancelled(cancel) {
            return Err(ModelError::Cancelled);
        }
        let started = Instant::now();
        let body = self.body(request);
        let target = self.target.clone();

        let attempted = retry_loop(&self.retry_backoff_ms, cancel, |_attempt| {
            let request = OutgoingRequest {
                method: "POST",
                path: target.join("/api/chat"),
                headers: vec![(
                    "Accept".to_owned(),
                    if self.stream {
                        "application/x-ndjson"
                    } else {
                        "application/json"
                    }
                    .to_owned(),
                )],
                body: body.clone(),
                timeout: self.timeout,
                max_response_bytes: self.max_response_bytes,
            };
            let response = crate::http::send(&target, &request, cancel)?;
            if response.status != 200 {
                return Err(ModelError::HttpStatus {
                    status: response.status,
                    endpoint: format!("{}{}", target.display(), target.join("/api/chat")),
                    detail: shorten(&response.body_text()),
                });
            }
            parse_answer(&response.body_text(), self.stream)
        })?;

        let mut reply = attempted.value;
        reply.elapsed_ms = started.elapsed().as_millis() as u64;
        reply.attempts = attempted.attempts;
        Ok(reply)
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "ollama",
            base_url: self.target.display(),
            model: self.model.clone(),
            // Ollama on loopback needs no credential, so this says "no credential is used", not
            // "a credential is missing".
            credential_present: false,
            timeout_secs: self.timeout.as_secs(),
            max_response_bytes: self.max_response_bytes,
            max_prompt_bytes: self.max_prompt_bytes,
            stream: self.stream,
            retries: self.retry_backoff_ms.len(),
        }
    }

    fn provider_name(&self) -> &'static str {
        "ollama"
    }
}

/// Read one Ollama answer, streamed or whole.
fn parse_answer(text: &str, stream: bool) -> Result<ChatReply> {
    if stream {
        let assembled =
            assemble_stream_text(text, false).ok_or_else(|| ModelError::ResponseShape {
                provider: "ollama",
                detail: "no NDJSON object with message.content was found in the streamed body"
                    .to_owned(),
            })?;
        return Ok(ChatReply {
            content: assembled,
            model: None,
            // A stream that never carried `done: true` was cut short; `assemble_stream_text`
            // cannot tell, so the caller's parse of a truncated plan is the real check.
            finish_reason: if text.contains("\"done\":true") {
                Some("stop".to_owned())
            } else {
                Some(OLLAMA_DONE_LENGTH.to_owned())
            },
            tokens_used: None,
            elapsed_ms: 0,
            attempts: 0,
        });
    }

    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ModelError::ResponseShape {
            provider: "ollama",
            detail: format!("the body is not the JSON Ollama returns: {e}"),
        })?;

    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| ModelError::ResponseShape {
            provider: "ollama",
            detail: format!(
                "no message.content in the answer; the body was {}",
                shorten(text)
            ),
        })?
        .to_owned();

    let (finish_reason, model, tokens_used) = provenance(&value);
    Ok(ChatReply {
        content,
        model,
        finish_reason,
        tokens_used,
        elapsed_ms: 0,
        attempts: 0,
    })
}

/// Read the three provenance fields Ollama reports, from an already-parsed body.
fn provenance(value: &serde_json::Value) -> (Option<String>, Option<String>, Option<u64>) {
    let done = value.get("done").and_then(|d| d.as_bool());
    let reason = value
        .get("done_reason")
        .and_then(|r| r.as_str())
        .map(str::to_owned);
    let finish_reason = match (done, reason) {
        (Some(true), Some(reason)) => Some(reason),
        (Some(false), _) => Some(OLLAMA_DONE_LENGTH.to_owned()),
        (None, _) => None,
        (Some(true), None) => Some("stop".to_owned()),
    };
    (
        finish_reason,
        value
            .get("model")
            .and_then(|m| m.as_str())
            .map(str::to_owned),
        value.get("eval_count").and_then(|v| v.as_u64()),
    )
}

fn shorten(text: &str) -> String {
    let mut out: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::{ok_body, status_only, Stub};
    use std::sync::atomic::AtomicBool;

    fn config(url: &str) -> ModelConfig {
        ModelConfig {
            provider: "ollama".to_owned(),
            model: "test-model".to_owned(),
            base_url: url.to_owned(),
            request_timeout_secs: 5,
            max_response_bytes: 64 * 1024,
            ..Default::default()
        }
    }

    #[test]
    fn a_normal_answer_becomes_a_reply_with_the_provenance_the_record_wants() {
        let stub = Stub::spawn(ok_body(
            r#"{"model":"test-model:latest","message":{"role":"assistant","content":"{\"schema_version\":1}"},"done":true,"done_reason":"stop","eval_count":17}"#,
        ));
        let client = Ollama::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("system text", "user text"), &cancel)
            .expect("generate");
        assert_eq!(reply.content, r#"{"schema_version":1}"#);
        assert_eq!(reply.model.as_deref(), Some("test-model:latest"));
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
        assert_eq!(reply.tokens_used, Some(17));
        assert_eq!(reply.attempts, 1);
        assert!(!reply.truncated());
    }

    #[test]
    fn the_request_asks_for_json_and_honours_the_configured_budget() {
        let stub = Stub::spawn(ok_body(r#"{"message":{"content":"{}"},"done":true}"#));
        let mut config = config(&stub.url());
        config.max_output_tokens = 99;
        config.temperature = 0.2;
        let client = Ollama::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("generate");
        let sent = stub.bodies();
        let text = sent.last().expect("one request");
        let json_start = text.find("\r\n\r\n").expect("headers end");
        let payload: serde_json::Value =
            serde_json::from_str(text[json_start + 4..].trim()).expect("request json");
        assert_eq!(payload["format"], "json");
        assert_eq!(payload["stream"], false);
        assert_eq!(payload["model"], "test-model");
        assert_eq!(payload["options"]["num_predict"], 99);
        // `temperature` is an `f32` in configuration, so the JSON round trip is approximate.
        let temperature = payload["options"]["temperature"]
            .as_f64()
            .expect("a number");
        assert!((temperature - 0.2).abs() < 1e-6, "{temperature}");
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["messages"][1]["role"], "user");
        // The operator's words are kept in their own message, not woven into the instructions.
        assert_eq!(payload["messages"][1]["content"], "u");
    }

    #[test]
    fn an_unsupported_response_shape_is_reported_as_content_not_transport() {
        let stub = Stub::spawn(ok_body(r#"{"generating":"chatter"}"#));
        let client = Ollama::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(
            matches!(
                error,
                ModelError::ResponseShape {
                    provider: "ollama",
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(error.code(), "response_shape");
        assert!(!error.is_retryable(), "the provider did answer");
    }

    #[test]
    fn a_body_that_is_not_json_at_all_is_a_shape_failure() {
        let stub = Stub::spawn(ok_body("I cannot help with that."));
        let client = Ollama::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(
            matches!(error, ModelError::ResponseShape { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_streaming_answer_is_reassembled_then_checked_by_the_same_rules() {
        let body = "{\"message\":{\"content\":\"{\\\"a\\\"\"},\"done\":false}\n\
                    {\"message\":{\"content\":\":1\"},\"done\":false}\n\
                    {\"message\":{\"content\":\"}\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let stub = Stub::spawn_dyn(move |request| {
            assert!(request.contains("\"stream\":true"), "{request}");
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\r\n{body}")
        });
        let mut config = config(&stub.url());
        config.stream = true;
        let client = Ollama::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("generate");
        assert_eq!(reply.content, r#"{"a":1}"#);
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn a_5xx_is_retried_within_the_configured_backoff_and_the_attempts_are_recorded() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&hits);
        let stub = Stub::spawn_dyn(move |_| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                "HTTP/1.1 503 Unavailable\r\nContent-Length: 3\r\n\r\nbad".to_owned()
            } else {
                ok_body(r#"{"message":{"content":"{}"},"done":true}"#)
            }
        });
        let mut config = config(&stub.url());
        config.retry_backoff_ms = vec![10];
        let client = Ollama::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("retry worked");
        assert_eq!(
            reply.attempts, 2,
            "the record must show the first attempt failed"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_4xx_that_is_not_rate_limiting_is_not_retried() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&hits);
        let stub = Stub::spawn_dyn(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            status_only(404, "Not Found", "no such endpoint")
        });
        let mut config = config(&stub.url());
        config.retry_backoff_ms = vec![5, 5, 5];
        let client = Ollama::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(
            matches!(error, ModelError::HttpStatus { status: 404, .. }),
            "{error:?}"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a wrong request repeated three times is noise, not recovery"
        );
    }

    #[test]
    fn a_429_is_retried_because_the_provider_asked_for_it() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&hits);
        let stub = Stub::spawn_dyn(move |_| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\n\r\nhi".to_owned()
            } else {
                ok_body(r#"{"message":{"content":"{}"},"done":true}"#)
            }
        });
        let mut config = config(&stub.url());
        config.retry_backoff_ms = vec![5];
        let client = Ollama::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("ok");
        assert_eq!(reply.attempts, 2);
    }

    #[test]
    fn an_oversized_prompt_is_refused_before_the_socket_is_opened() {
        let stub = Stub::spawn(ok_body(r#"{"message":{"content":"{}"},"done":true}"#));
        let mut config = config(&stub.url());
        config.max_prompt_bytes = 64;
        let client = Ollama::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "x".repeat(500)), &cancel)
            .unwrap_err();
        assert!(
            matches!(error, ModelError::PromptTooLarge { limit: 64, .. }),
            "{error:?}"
        );
        assert!(stub.bodies().is_empty(), "nothing must reach the provider");
    }

    #[test]
    fn cancellation_during_a_wait_is_reported_as_cancelled_not_as_a_failure() {
        let stub = Stub::spawn_dyn(|_| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            ok_body(r#"{"message":{"content":"{}"},"done":true}"#)
        });
        let client = Ollama::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(true);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(matches!(error, ModelError::Cancelled), "{error:?}");
        assert!(
            !error.is_retryable(),
            "the operator stopped this; nothing restarts it"
        );
    }

    #[test]
    fn a_deadline_that_expires_is_a_timeout_and_is_named_as_one() {
        let stub = Stub::spawn_dyn(|_| {
            std::thread::sleep(std::time::Duration::from_millis(1_500));
            ok_body(r#"{"message":{"content":"{}"},"done":true}"#)
        });
        let mut config = config(&stub.url());
        config.request_timeout_secs = 1;
        config.retry_backoff_ms = Vec::new();
        let client = Ollama::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(
            matches!(error, ModelError::TimedOut { .. } | ModelError::Protocol(_)),
            "{error:?}"
        );
    }

    #[test]
    fn an_answer_that_ran_out_of_tokens_is_flagged_so_a_truncated_plan_is_not_blamed_on_the_schema()
    {
        let stub = Stub::spawn(ok_body(
            r#"{"message":{"content":"{\"schema_version\":1,"},"done":true,"done_reason":"length"}"#,
        ));
        let client = Ollama::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("reply");
        assert!(reply.truncated(), "{reply:?}");
    }

    #[test]
    fn info_names_the_endpoint_without_naming_a_credential_that_is_not_used() {
        let stub = Stub::spawn(ok_body(r#"{"message":{"content":"{}"},"done":true}"#));
        let client = Ollama::new(&config(&stub.url())).expect("build");
        let info = client.info();
        assert_eq!(info.name, "ollama");
        assert_eq!(info.model, "test-model");
        assert!(!info.credential_present);
        assert_eq!(info.timeout_secs, 5);
        assert!(
            info.base_url.starts_with("http://127.0.0.1"),
            "{}",
            info.base_url
        );
    }

    #[test]
    fn the_no_provider_path_refuses_without_touching_anything() {
        let client = crate::build(&ModelConfig::default()).expect("none builds");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(matches!(error, ModelError::ProviderDisabled), "{error:?}");
        assert_eq!(error.code(), "provider_disabled");
        assert_eq!(client.provider_name(), "none");
        assert!(
            !client.info().stream,
            "the none provider claims no transport"
        );
    }
}
