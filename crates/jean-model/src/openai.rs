//! The OpenAI-compatible adapter: `POST {base_path}/chat/completions`, the subset llama.cpp,
//! LM Studio, vLLM and OpenAI itself agree on.
//!
//! ## What is sent, exactly
//!
//! ```json
//! {
//!   "model": "…",
//!   "messages": [{"role": "system", "content": "…"}, {"role": "user", "content": "…"}],
//!   "stream": false,
//!   "temperature": 0.0,
//!   "max_tokens": 2048,
//!   "response_format": {"type": "json_object"}
//! }
//! ```
//!
//! No `tools`, `functions`, `tool_choice`, `stop`, `seed`, `n`, `presence_penalty`, `logprobs`,
//! `background` or `reasoning_effort`. "OpenAI-compatible" describes a wire shape, not a guaranteed
//! parameter set: a field one server ignores and another rejects is a field that makes the
//! configuration silently provider-specific. `response_format` is the one exception, because a JSON
//! mode is what turns a plan request into something a strict parser can hold to a contract. A server
//! that rejects it answers `400`, which this adapter reports verbatim and does not retry — the
//! operator then either uses a server that honours it or plans deterministically.
//!
//! ## What is read
//!
//! `{"model":"…","choices":[{"message":{"content":"…"},"finish_reason":"stop"}],
//! "usage":{"total_tokens":n}}`. Only `choices[0]` is used; asking for `n > 1` is not supported.
//! `content` may legitimately be an empty string when a server puts the answer in `reasoning_content`
//! or refuses on policy, and an empty answer is reported as a shape failure rather than parsed as an
//! empty plan — a plan with no steps would otherwise look like a model that simply had nothing to do.
//!
//! ## The credential
//!
//! `model.api_key_env` names the environment variable to read, at request time, through
//! [`crate::Secret`]. An empty name means the endpoint needs no credential, which is the normal case
//! for a loopback server. When a name is configured and the variable is unset, the request fails
//! before connecting, and the message names the variable and never its value. Everything the server
//! sends back — including an error body, which providers routinely echo request headers into — is
//! scrubbed before it can reach a log, a record, or this error type.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use jean_protocol::config::ModelConfig;
use serde_json::json;

use crate::error::{ModelError, Result};
use crate::http::{OutgoingRequest, Target};
use crate::secret::Secret;
use crate::{
    assemble_stream_text, cancelled, check_prompt_size, retry_loop, ChatReply, ChatRequest,
    ModelClient, ProviderInfo,
};

pub struct OpenAiCompatible {
    target: Target,
    model: String,
    /// The *name* of the variable holding the key. `None` when configuration asks for no credential.
    credential_env: Option<String>,
    timeout: Duration,
    max_response_bytes: usize,
    max_prompt_bytes: usize,
    temperature: f32,
    max_output_tokens: u32,
    stream: bool,
    retry_backoff_ms: Vec<u64>,
}

impl OpenAiCompatible {
    pub fn new(config: &ModelConfig) -> Result<Self> {
        Ok(Self {
            target: Target::parse(&config.base_url)?,
            model: config.model.clone(),
            credential_env: (!config.api_key_env.trim().is_empty())
                .then(|| config.api_key_env.trim().to_owned()),
            timeout: Duration::from_secs(config.request_timeout_secs),
            max_response_bytes: config.max_response_bytes,
            max_prompt_bytes: config.max_prompt_bytes,
            temperature: config.temperature,
            max_output_tokens: config.max_output_tokens,
            stream: config.stream,
            retry_backoff_ms: config.retry_backoff_ms.clone(),
        })
    }

    /// The endpoint path, with whatever prefix the base URL carried. `/v1` for llama.cpp and
    /// OpenAI itself, empty for a server that mounts completions at the root.
    fn path(&self) -> String {
        self.target.join("/chat/completions")
    }

    fn body(&self, request: &ChatRequest) -> Vec<u8> {
        let mut payload = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "stream": self.stream,
            "temperature": self.temperature,
            "max_tokens": self.max_output_tokens,
        });
        if request.prefer_json {
            payload["response_format"] = json!({ "type": "json_object" });
        }
        serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec())
    }
}

impl ModelClient for OpenAiCompatible {
    fn generate(&self, request: &ChatRequest, cancel: &AtomicBool) -> Result<ChatReply> {
        check_prompt_size(request, self.max_prompt_bytes)?;
        if cancelled(cancel) {
            return Err(ModelError::Cancelled);
        }

        // Read the credential now, and only for as long as this call lives.
        let secret = match &self.credential_env {
            Some(name) => {
                let secret = Secret::from_env(name);
                if !secret.is_present() {
                    return Err(ModelError::MissingCredential {
                        name: name.to_owned(),
                    });
                }
                secret
            }
            None => Secret::absent(""),
        };

        let started = Instant::now();
        let body = self.body(request);
        let endpoint = format!("{}{}", self.target.display(), self.path());
        let target = self.target.clone();
        let path = self.path();
        let bearer = secret.reveal();

        let attempted = retry_loop(&self.retry_backoff_ms, cancel, |_attempt| {
            let accept = if self.stream {
                "text/event-stream"
            } else {
                "application/json"
            };
            let mut headers = vec![
                ("Accept".to_owned(), accept.to_owned()),
                ("Content-Type".to_owned(), "application/json".to_owned()),
            ];
            if let Some(token) = &bearer {
                headers.push(("Authorization".to_owned(), format!("Bearer {token}")));
            }
            let outgoing = OutgoingRequest {
                method: "POST",
                path: path.clone(),
                headers,
                body: body.clone(),
                timeout: self.timeout,
                max_response_bytes: self.max_response_bytes,
            };
            let response = crate::http::send(&target, &outgoing, cancel)
                .map_err(|error| scrub_error(&secret, error))?;
            if response.status != 200 {
                return Err(scrub_error(
                    &secret,
                    ModelError::HttpStatus {
                        status: response.status,
                        endpoint: endpoint.clone(),
                        detail: shorten(&response.body_text()),
                    },
                ));
            }
            parse_answer(&response.body_text(), self.stream)
                .map_err(|error| scrub_error(&secret, error))
        })?;

        let mut reply = attempted.value;
        // A provider that echoes its own prompt back (some do, in an error or a debug field) would
        // put the credential in the record. Scrubbing the answer is cheap insurance.
        reply.content = secret.scrub(&reply.content);
        reply.elapsed_ms = started.elapsed().as_millis() as u64;
        reply.attempts = attempted.attempts;
        Ok(reply)
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "openai_compatible",
            base_url: self.target.display(),
            model: self.model.clone(),
            // Presence is checked at request time; `jean model status` says "a variable is named"
            // and whether it currently resolves, never what it holds.
            credential_present: self
                .credential_env
                .as_deref()
                .map(|name| Secret::from_env(name).is_present())
                .unwrap_or(false),
            timeout_secs: self.timeout.as_secs(),
            max_response_bytes: self.max_response_bytes,
            max_prompt_bytes: self.max_prompt_bytes,
            stream: self.stream,
            retries: self.retry_backoff_ms.len(),
        }
    }

    fn provider_name(&self) -> &'static str {
        "openai_compatible"
    }
}

/// Turn one response body into an answer, streamed or whole.
fn parse_answer(text: &str, stream: bool) -> Result<ChatReply> {
    if stream {
        let assembled =
            assemble_stream_text(text, true).ok_or_else(|| ModelError::ResponseShape {
                provider: "openai_compatible",
                detail: "no SSE `data:` line carried choices[0].delta.content".to_owned(),
            })?;
        if assembled.is_empty() {
            return Err(empty_answer("openai_compatible", text));
        }
        return Ok(ChatReply {
            content: assembled,
            model: None,
            // A stream that carried a terminal `finish_reason` was completed by the server; one
            // that did not was cut short, which is exactly what a truncated plan looks like.
            finish_reason: if text.contains("\"finish_reason\"") {
                Some("stop".to_owned())
            } else {
                Some("length".to_owned())
            },
            tokens_used: None,
            elapsed_ms: 0,
            attempts: 0,
        });
    }

    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ModelError::ResponseShape {
            provider: "openai_compatible",
            detail: format!("the body is not the JSON an OpenAI-compatible server returns: {e}"),
        })?;

    if let Some(message) = error_message(&value) {
        return Err(ModelError::ResponseShape {
            provider: "openai_compatible",
            detail: format!("the server reported an error instead of an answer: {message}"),
        });
    }

    let choice =
        value
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| ModelError::ResponseShape {
                provider: "openai_compatible",
                detail: format!(
                    "no choices[0] in the answer; the body was {}",
                    shorten(text)
                ),
            })?;

    let content = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_owned)
        // A refusal or a tool-call-only answer leaves `content` null. That is an answer with
        // nothing in it, and parsing it as a plan would produce a confusing schema error.
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| empty_answer("openai_compatible", text))?;

    Ok(ChatReply {
        content,
        model: value
            .get("model")
            .and_then(|m| m.as_str())
            .map(str::to_owned),
        finish_reason: choice
            .get("finish_reason")
            .and_then(|r| r.as_str())
            .map(str::to_owned),
        tokens_used: value
            .get("usage")
            .and_then(|u| u.get("total_tokens"))
            .and_then(|t| t.as_u64()),
        elapsed_ms: 0,
        attempts: 0,
    })
}

/// `{"error":{"message":"…"}}`, which a compatible server returns for an unsupported parameter.
fn error_message(value: &serde_json::Value) -> Option<String> {
    value
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("error")
                .and_then(|e| e.as_str())
                .map(str::to_owned)
        })
}

fn empty_answer(provider: &'static str, text: &str) -> ModelError {
    ModelError::ResponseShape {
        provider,
        detail: format!(
            "the answer carries no text to plan from; the server said {}",
            shorten(text)
        ),
    }
}

/// Replace any echo of the credential in an error before it can be shown or recorded.
fn scrub_error(secret: &Secret, error: ModelError) -> ModelError {
    match error {
        ModelError::HttpStatus {
            status,
            endpoint,
            detail,
        } => ModelError::HttpStatus {
            status,
            endpoint,
            detail: secret.scrub(&detail),
        },
        ModelError::ResponseShape { provider, detail } => ModelError::ResponseShape {
            provider,
            detail: secret.scrub(&detail),
        },
        ModelError::Protocol(detail) => ModelError::Protocol(secret.scrub(&detail)),
        other => other,
    }
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
    use crate::stub::{chunked_body, ok_body, status_only, Stub};
    use std::sync::atomic::AtomicBool;

    /// One variable per test: `set_var` is process-wide and tests run on several threads, so a
    /// name shared between two of them would let one read the other's value.
    const KEY_SENT: &str = "JEAN_ADAPTER_TEST_KEY_SENT";
    const KEY_LEAKED: &str = "JEAN_ADAPTER_TEST_KEY_LEAKED";
    const KEY_UNSET: &str = "JEAN_ADAPTER_TEST_KEY_UNSET";
    const KEY_REPORTED: &str = "JEAN_ADAPTER_TEST_KEY_REPORTED";

    fn config(url: &str) -> ModelConfig {
        ModelConfig {
            provider: "openai_compatible".to_owned(),
            model: "test-model".to_owned(),
            base_url: url.to_owned(),
            request_timeout_secs: 5,
            max_response_bytes: 64 * 1024,
            api_key_env: String::new(),
            ..Default::default()
        }
    }

    fn answer(content: &str) -> String {
        format!(
            r#"{{"model":"srv-model","choices":[{{"message":{{"role":"assistant","content":{content:?}}},"finish_reason":"stop"}}],"usage":{{"total_tokens":41}}}}"#
        )
    }

    #[test]
    fn a_normal_completion_becomes_a_reply_with_provenance() {
        let stub = Stub::spawn(ok_body(&answer(r#"{"schema_version":1}"#)));
        let client = OpenAiCompatible::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("system", "user"), &cancel)
            .expect("generate");
        assert_eq!(reply.content, r#"{"schema_version":1}"#);
        assert_eq!(reply.model.as_deref(), Some("srv-model"));
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
        assert_eq!(reply.tokens_used, Some(41));
        assert_eq!(reply.attempts, 1);
        assert!(!reply.truncated());
    }

    #[test]
    fn the_request_is_the_subset_every_compatible_server_agrees_on() {
        let stub = Stub::spawn(ok_body(&answer("{}")));
        let mut config = config(&stub.url());
        config.base_url = format!("{}/v1", stub.url());
        config.max_output_tokens = 777;
        config.temperature = 0.1;
        let client = OpenAiCompatible::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        client
            .generate(&ChatRequest::new("rules", "open notepad"), &cancel)
            .expect("generate");
        let payload = stub.first_json();
        assert_eq!(payload["model"], "test-model");
        assert_eq!(payload["stream"], false);
        assert_eq!(payload["max_tokens"], 777);
        // `temperature` is an `f32` in configuration, so the JSON round trip is approximate.
        let temperature = payload["temperature"].as_f64().expect("a number");
        assert!((temperature - 0.1).abs() < 1e-6, "{temperature}");
        assert_eq!(payload["response_format"]["type"], "json_object");
        assert_eq!(payload["messages"][0]["content"], "rules");
        assert_eq!(payload["messages"][1]["content"], "open notepad");
        // The path keeps the configured prefix: llama.cpp mounts completions under /v1.
        let raw = stub.bodies()[0].clone();
        assert!(
            raw.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{raw}"
        );
        // No tool-calling or sampling fields this build does not send.
        for absent in ["tools", "functions", "tool_choice", "seed", "n", "stop"] {
            assert!(payload.get(absent).is_none(), "{absent} must not be sent");
        }
    }

    #[test]
    fn a_credential_comes_from_the_named_variable_and_is_a_bearer_header() {
        std::env::set_var(KEY_SENT, "sk-test-abcdefghij");
        let stub = Stub::spawn(ok_body(&answer("{}")));
        let mut config = config(&stub.url());
        config.api_key_env = KEY_SENT.to_owned();
        let client = OpenAiCompatible::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("generate");
        let raw = stub.bodies()[0].clone();
        // Matched case-insensitively because HTTP/1.1 field names are case-insensitive
        // (RFC 9110 §3.2); the capitalisation a particular client happens to write is not a
        // property of the request. That the header is present, and is a bearer of the key, is.
        let head = raw.to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer sk-test-abcdefghij"),
            "the header must actually be sent: {raw}"
        );
        assert!(client.info().credential_present);
        std::env::remove_var(KEY_SENT);
    }

    #[test]
    fn an_unset_credential_fails_before_the_socket_is_opened() {
        std::env::remove_var(KEY_UNSET);
        let stub = Stub::spawn(ok_body(&answer("{}")));
        let mut config = config(&stub.url());
        config.api_key_env = KEY_UNSET.to_owned();
        let client = OpenAiCompatible::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(
            matches!(&error, ModelError::MissingCredential { name } if name == KEY_UNSET),
            "{error:?}"
        );
        assert!(
            !error.is_retryable(),
            "a missing key is not a transport fault"
        );
        assert!(stub.bodies().is_empty(), "nothing must reach the provider");
        // The diagnostic names the variable, which is the operator's to fix.
        assert!(error.to_string().contains(KEY_UNSET), "{error}");
    }

    #[test]
    fn a_provider_that_echoes_the_key_back_in_an_error_does_not_put_it_in_our_error() {
        std::env::set_var(KEY_LEAKED, "sk-leak-0123456789");
        let stub = Stub::spawn_dyn(|_| {
            status_only(
                401,
                "Unauthorized",
                r#"{"error":{"message":"invalid key sk-leak-0123456789"}}"#,
            )
        });
        let mut config = config(&stub.url());
        config.api_key_env = KEY_LEAKED.to_owned();
        let client = OpenAiCompatible::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect_err("401");
        let shown = error.to_string();
        assert!(!shown.contains("sk-leak-0123456789"), "leaked: {shown}");
        assert!(shown.contains("[REDACTED]"), "{shown}");
        assert!(!error.is_retryable(), "401 is not a transport error");
        std::env::remove_var(KEY_LEAKED);
    }

    #[test]
    fn an_answer_with_no_text_is_a_shape_failure_not_an_empty_plan() {
        let stub = Stub::spawn(ok_body(
            r#"{"choices":[{"message":{"role":"assistant","content":""},"finish_reason":"stop"}]}"#,
        ));
        let client = OpenAiCompatible::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(
            matches!(
                error,
                ModelError::ResponseShape {
                    provider: "openai_compatible",
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!error.is_retryable());
    }

    #[test]
    fn a_server_error_inside_a_200_body_is_reported_rather_than_parsed() {
        let stub = Stub::spawn(ok_body(
            r#"{"error":{"message":"response_format is not supported by this server"}}"#,
        ));
        let client = OpenAiCompatible::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect_err("shape");
        let shown = error.to_string();
        assert!(shown.contains("response_format"), "{shown}");
    }

    #[test]
    fn a_truncated_generation_is_flagged_before_the_parser_blames_the_schema() {
        let stub = Stub::spawn(ok_body(
            r#"{"choices":[{"message":{"content":"{\"schema\":1,"},"finish_reason":"length"}]}"#,
        ));
        let client = OpenAiCompatible::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("reply");
        assert!(reply.truncated(), "{reply:?}");
    }

    #[test]
    fn a_streamed_answer_is_reassembled_and_checked_by_the_same_rules() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"{\\\"a\\\"\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\":1}\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = Stub::spawn_dyn(move |request| {
            assert!(request.contains("\"stream\":true"), "{request}");
            let head = request.to_ascii_lowercase();
            assert!(
                head.contains("accept: text/event-stream"),
                "a streaming request says so: {request}"
            );
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
        });
        let mut config = config(&stub.url());
        config.stream = true;
        let client = OpenAiCompatible::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("reply");
        assert_eq!(reply.content, r#"{"a":1}"#);
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn a_5xx_is_retried_and_the_attempts_reach_the_record() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&hits);
        let stub = Stub::spawn_dyn(move |_| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                status_only(502, "Bad Gateway", "upstream dead")
            } else {
                ok_body(&answer("{}"))
            }
        });
        let mut config = config(&stub.url());
        config.retry_backoff_ms = vec![10];
        let client = OpenAiCompatible::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("second attempt");
        assert_eq!(reply.attempts, 2);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_redirect_is_refused_because_the_destination_was_never_configured() {
        let stub = Stub::spawn(status_only(
            302,
            "Found",
            r#"{"location":"https://elsewhere.example/v1/chat/completions"}"#,
        ));
        let client = OpenAiCompatible::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(matches!(error, ModelError::Redirect(_)), "{error:?}");
        assert_eq!(error.code(), "redirect_refused");
        assert!(!error.is_retryable());
    }

    #[test]
    fn an_oversized_prompt_never_reaches_the_endpoint() {
        let stub = Stub::spawn(ok_body(&answer("{}")));
        let mut config = config(&stub.url());
        config.max_prompt_bytes = 32;
        let client = OpenAiCompatible::new(&config).expect("build");
        let cancel = AtomicBool::new(false);
        let error = client
            .generate(&ChatRequest::new("s", "y".repeat(400)), &cancel)
            .unwrap_err();
        assert!(
            matches!(error, ModelError::PromptTooLarge { limit: 32, .. }),
            "{error:?}"
        );
        assert!(stub.bodies().is_empty());
    }

    #[test]
    fn cancellation_is_respected_while_talking_to_the_provider() {
        let stub = Stub::spawn_dyn(|_| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            ok_body(&answer("{}"))
        });
        let client = OpenAiCompatible::new(&config(&stub.url())).expect("build");
        let cancel = AtomicBool::new(true);
        let error = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        assert!(matches!(error, ModelError::Cancelled), "{error:?}");
    }

    #[test]
    fn an_https_endpoint_is_built_and_dialed_rather_than_refused_at_load() {
        // The transport verifies certificates now, so an https base_url has to survive both the
        // parse and the dial attempt with a message about TLS rather than about a missing feature.
        let client = OpenAiCompatible::new(&config("https://api.example.com/v1")).expect("build");
        let info = client.info();
        assert_eq!(info.base_url, "https://api.example.com:443");

        // The stub below speaks plain HTTP, so this is a real handshake against a non-TLS port.
        // The deadline is generous so the TLS verdict, not the clock, is what the test sees.
        let stub = Stub::spawn(ok_body("{}"));
        let mut remote = config(&format!("https://{}", stub.authority()));
        remote.request_timeout_secs = 20;
        let remote = OpenAiCompatible::new(&remote).expect("build https client");
        let cancel = AtomicBool::new(false);
        let error = remote
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .unwrap_err();
        let text = error.to_string().to_ascii_lowercase();
        assert!(
            !text.contains("does not ship") && !text.contains("no tls client"),
            "the client still refuses HTTPS by policy: {text}"
        );
        assert!(
            text.contains("tls"),
            "expected the TLS layer to be named, got: {text}"
        );
    }

    #[test]
    fn a_chunked_sse_answer_is_reassembled_by_the_client_that_receives_it() {
        // `stream = true` was always unattainable over the previous transport, because an SSE
        // response is chunked. This is the end-to-end proof that it now is.
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"schema\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"_version\\\":1}\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = Stub::spawn(chunked_body(sse, 7));
        let mut config = config(&stub.url());
        config.stream = true;
        let client = OpenAiCompatible::new(&config).expect("build streaming client");
        let cancel = AtomicBool::new(false);
        let reply = client
            .generate(&ChatRequest::new("s", "u"), &cancel)
            .expect("reassembled answer");
        assert_eq!(reply.content, r#"{"schema_version":1}"#);
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
        assert!(!reply.truncated());
    }

    #[test]
    fn the_factory_builds_each_configured_provider_and_refuses_an_unknown_one() {
        let mut config = config("http://127.0.0.1:1/v1");
        let client = crate::build(&config).expect("build openai_compatible");
        assert_eq!(client.provider_name(), "openai_compatible");

        config.provider = "ollama".to_owned();
        let ollama = crate::build(&config).expect("build ollama");
        assert_eq!(ollama.provider_name(), "ollama");

        // `ModelConfig::check` rejects this at load, so this branch is defensive.
        config.provider = "claude".to_owned();
        let Err(error) = crate::build(&config) else {
            panic!("an unimplemented provider must not build a client");
        };
        assert!(
            matches!(error, ModelError::UnsupportedProvider(_)),
            "{error:?}"
        );
    }

    #[test]
    fn info_reports_the_limits_that_shape_a_plan_request() {
        std::env::set_var(KEY_REPORTED, "sk-quiet-0123456789");
        let mut config = config("http://127.0.0.1:9/v1");
        config.api_key_env = KEY_REPORTED.to_owned();
        config.max_response_bytes = 4096;
        config.max_prompt_bytes = 8192;
        config.request_timeout_secs = 11;
        config.retry_backoff_ms = vec![1, 2, 3];
        let client = OpenAiCompatible::new(&config).expect("build");
        let info = client.info();
        assert_eq!(info.name, "openai_compatible");
        assert_eq!(info.base_url, "http://127.0.0.1:9");
        assert_eq!(info.model, "test-model");
        assert!(info.credential_present);
        assert_eq!(
            (info.max_response_bytes, info.max_prompt_bytes),
            (4096, 8192)
        );
        assert_eq!(info.timeout_secs, 11);
        assert_eq!(info.retries, 3);
        // Nothing in the reporting path carries the value.
        assert!(!format!("{info:?}").contains("sk-quiet"));
        std::env::remove_var(KEY_REPORTED);
    }
}
