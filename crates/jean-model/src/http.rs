//! The HTTP client: [`ureq`] with JEAN's own limits applied on top of it.
//!
//! ## Why a crate rather than a hand-rolled client
//!
//! A transport is a parser for a protocol an adversary writes, and the interesting part of that
//! job is the half you did not think of. `ureq` is a maintained blocking client, so no async
//! runtime enters the tree (DV-02 keeps the supervisor synchronous), and it owns the parts that
//! were previously ours to get wrong: HTTP/1.1 framing, **chunked transfer decoding**, header
//! bounds, TLS with certificate-chain and hostname verification through the Windows system trust
//! store, and a connection pool that can be shut down from another thread.
//!
//! What stays ours, and is the reason this file exists at all: the ceiling on how many bytes of
//! body we will hold, the deadline the operator configured, the cancellation flag Ctrl-C sets, the
//! refusal to follow a redirect to a host nobody configured, and the scrubbing of any credential
//! that comes back inside an error body. Those are policy, and policy does not belong to a
//! dependency.
//!
//! ## The limits, and where each is enforced
//!
//! * **Verified HTTPS.** `https` is accepted, and a non-loopback endpoint is only configurable over
//!   it. Verification is never weakened: nothing here sets an accept-any verifier or a hostname
//!   override, and `native-tls` on Windows means Schannel validates the chain *and* the name. The
//!   one substitution available is a test-only anchor set ("send_test_anchored", test builds
//!   only), which replaces
//!   the roots with a narrower list while leaving both checks running; it is compiled out of a
//!   shipped build and cannot disable verification. `http` stays allowed because the documented
//!   local case is a loopback model server reached without TLS.
//! * **Bounded streaming.** The body is read through `Read::take(max + 1)`, so an answer larger
//!   than `model.max_response_bytes` fails as `ResponseTooLarge` rather than arriving truncated —
//!   and that bound is what holds a *chunked* answer down, which declares no length in advance.
//! * **Deadline.** `timeout_global` bounds the whole exchange including the body;
//!   `timeout_connect` and `timeout_resolve` bound the two phases a shutdown cannot interrupt.
//! * **Cancellation.** The exchange runs on a worker thread; this thread polls the flag and calls
//!   `Agent::shutdown()`, which closes the pooled sockets so a parked read returns immediately
//!   instead of waiting out the deadline.
//! * **Redirects.** `max_redirects(0)`: a 3xx comes back to us as a response and is refused below.
//!   `ureq` separately never re-attaches an `Authorization` header across a redirect.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ureq::Agent;

use crate::error::{ModelError, Result};

/// The largest response head this build will accept, in bytes.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// How often the watching thread looks at the cancellation flag and the clock.
///
/// The exchange itself is interrupted by a socket close, not by this poll; the slice only decides
/// how quickly a Ctrl-C is noticed while the worker is between reads.
const POLL_SLICE: Duration = Duration::from_millis(25);

const PORT_HTTP: u16 = 80;
const PORT_HTTPS: u16 = 443;

/// Attached to a failure that happened before any HTTP was spoken on an `https` endpoint.
///
/// At the socket layer a refused certificate and a refused TCP connection look the same, and the
/// operator's next question is always "did this fail because verification is switched off
/// somewhere?", so the answer is in the message. It is worded for whichever roots the client is
/// using, because the only substitution that exists (`agent_config`'s anchor) narrows the roots
/// instead of disabling the check.
const TLS_EXPLAINED: &str = " — this is a verified TLS connection: the certificate chain and the \
                              host name are checked against the trust roots this build is \
                              configured with, and it cannot be configured to skip that check";

/// The pieces of `model.base_url` needed to dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Path prefix, e.g. `/v1` for llama.cpp. Always begins with `/`, never ends with one.
    pub prefix: String,
    pub scheme: String,
}

impl Target {
    /// Parse the URL. Kept strict: a userinfo, query or fragment is refused, matching the check
    /// in `jean_protocol::config`, so a key can never ride in on a URL.
    pub fn parse(raw: &str) -> Result<Self> {
        let text = raw.trim();
        let Some((scheme, rest)) = text.split_once("://") else {
            return Err(ModelError::Protocol(format!(
                "endpoint {text:?} has no scheme"
            )));
        };
        let default_port = match scheme {
            "http" => PORT_HTTP,
            "https" => PORT_HTTPS,
            other => {
                return Err(ModelError::Protocol(format!(
                    "endpoint scheme {other:?} is not http or https"
                )))
            }
        };
        if rest.contains('@') || rest.contains('?') || rest.contains('#') {
            return Err(ModelError::Protocol(
                "the endpoint must not carry credentials, a query or a fragment".to_owned(),
            ));
        }
        let (authority, prefix) = match rest.find('/') {
            Some(at) => (&rest[..at], rest[at..].to_owned()),
            None => (rest, String::new()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.parse::<u16>().is_ok() => {
                (h.to_owned(), p.parse::<u16>().unwrap_or(default_port))
            }
            _ => (authority.to_owned(), default_port),
        };
        if host.is_empty() || host.contains(':') || host.contains(' ') {
            // A bracketed IPv6 literal is the one ordinary form refused here; loopback by name or
            // address covers every endpoint this build is configured to talk to.
            return Err(ModelError::Protocol(format!(
                "endpoint {text:?} has no usable host"
            )));
        }
        let prefix = prefix.trim_end_matches('/').to_owned();
        Ok(Self {
            host,
            port,
            prefix,
            scheme: scheme.to_owned(),
        })
    }

    pub fn join(&self, path: &str) -> String {
        if self.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}{}", self.prefix, path)
        }
    }

    pub fn display(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }

    /// The absolute URL a request path resolves to, with the port always explicit.
    ///
    /// `path` is the already-prefixed request path the adapter built with [`Target::join`]; joining
    /// it again here would produce `/v1/v1/chat/completions` against a llama.cpp endpoint.
    pub fn dial(&self, path: &str) -> String {
        format!("{}{}", self.display(), path)
    }

    /// Whether this is the local-model-server case the documentation describes.
    ///
    /// Used only to word an error: a TLS failure against a loopback endpoint that answered 404 on
    /// `http` is a different sentence from a TLS failure against a public API's certificate.
    pub fn is_loopback(&self) -> bool {
        self.host == "localhost" || self.host == "::1" || self.host.starts_with("127.")
    }
}

/// One request, ready to send.
#[derive(Clone)]
pub struct OutgoingRequest {
    pub method: &'static str,
    pub path: String,
    /// Header name and value pairs. Values here may contain a credential — `send` never logs them
    /// and the caller scrubs any echoed text.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
    pub max_response_bytes: usize,
}

/// A response we agreed to accept: a status and a body that stayed inside the ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The client configuration for one request, in one place.
///
/// Extracted because the security-relevant lines in it are worth asserting directly: a build that
/// quietly lost certificate verification would still pass every other test in this file.
///
/// `anchor` is `None` on every path a shipped build takes. When it is `Some`, the roots become that
/// one DER certificate and nothing else — `ureq`'s native-tls layer then sets
/// `disable_built_in_roots(true)` and adds only that cert. That is a *narrower* trust set, not a
/// weaker check: `disable_verification` stays `false`, so the chain and the hostname are still
/// validated. It exists so a test can prove certificate validation happens without touching the
/// Windows trust store.
fn agent_config(request: &OutgoingRequest, anchor: Option<&[u8]>) -> ureq::config::Config {
    let root_certs = match anchor {
        // The platform's roots, so a certificate the machine already trusts works and one it does
        // not, fails. This is the setting that makes "verified against the system store" literally
        // true.
        None => ureq::tls::RootCerts::PlatformVerifier,
        Some(der) => ureq::tls::RootCerts::Specific(std::sync::Arc::new(vec![
            ureq::tls::Certificate::from_der(der).to_owned(),
        ])),
    };
    let tls = ureq::tls::TlsConfig::builder()
        // Schannel through `native-tls`, i.e. the Windows trust store, and no C-built crypto crate
        // in the tree. ureq's own default provider is rustls, which this build does not compile.
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(root_certs)
        // `disable_verification` is left at its default `false`, and nothing in this workspace
        // ever calls it. That is what makes an https endpoint mean something.
        .build();

    Agent::config_builder()
        .tls_config(tls)
        .user_agent(concat!("jean-model/", env!("CARGO_PKG_VERSION")))
        // The adapters decide what a non-2xx means; do not let the client raise on it first.
        .http_status_as_error(false)
        // Redirects are off by default in this build, not merely bounded.
        .max_redirects(0)
        .max_response_header_size(MAX_HEADER_BYTES)
        .timeout_global(Some(request.timeout))
        // The two phases a shutdown cannot interrupt get their own bound.
        .timeout_connect(Some(request.timeout))
        .timeout_resolve(Some(request.timeout))
        .build()
}

/// Send the request, honouring the configured deadline, the size ceiling and the cancel flag.
///
/// Statuses come back as a [`RawResponse`] rather than as errors, so the adapters can read a
/// provider's own error body and report it. A 3xx is the one status refused here, because its
/// destination was never configured.
pub fn send(
    target: &Target,
    request: &OutgoingRequest,
    cancel: &AtomicBool,
) -> Result<RawResponse> {
    send_inner(target, request, cancel, None)
}

/// [`send_inner`] with a pinned test root, for the TLS tests only.
#[cfg(test)]
pub(crate) fn send_test_anchored(
    target: &Target,
    request: &OutgoingRequest,
    cancel: &AtomicBool,
    anchor_der: &[u8],
) -> Result<RawResponse> {
    send_inner(target, request, cancel, Some(anchor_der))
}

fn send_inner(
    target: &Target,
    request: &OutgoingRequest,
    cancel: &AtomicBool,
    anchor: Option<&[u8]>,
) -> Result<RawResponse> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ModelError::Cancelled);
    }
    let deadline = Instant::now() + request.timeout;

    let agent = agent_config(request, anchor).new_agent();

    // The worker owns everything it touches: a thread cannot borrow the caller's request, and the
    // cancellation flag is deliberately left with this thread so only one place reads it.
    let worker_agent = agent.clone();
    let worker_target = target.clone();
    let worker_request = request.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = exchange(&worker_agent, &worker_target, &worker_request);
        // A receiver that went away means the operator already has an answer; nothing to report.
        let _ = tx.send(outcome);
    });

    loop {
        if cancel.load(Ordering::Relaxed) {
            // Closing the pool is what interrupts a read parked on a slow answer.
            agent.shutdown();
            return Err(ModelError::Cancelled);
        }
        if Instant::now() >= deadline {
            agent.shutdown();
            return Err(ModelError::TimedOut {
                timeout_secs: request.timeout.as_secs(),
            });
        }
        match rx.recv_timeout(POLL_SLICE) {
            Ok(outcome) => return outcome,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                agent.shutdown();
                return Err(ModelError::Protocol(
                    "the transport thread ended without producing an answer".to_owned(),
                ));
            }
        }
    }
}

/// One exchange, on the worker thread, with the body read to the ceiling.
///
/// A `Content-Length` above the ceiling is refused before anything is read; a chunked answer
/// declares no length, so the `take()` below is the bound that actually holds.
fn exchange(agent: &Agent, target: &Target, request: &OutgoingRequest) -> Result<RawResponse> {
    let endpoint = target.dial(&request.path);
    if request.method != "POST" {
        return Err(ModelError::Protocol(format!(
            "the model client only sends POST; {method} was asked for",
            method = request.method
        )));
    }
    let mut builder = agent.post(endpoint.as_str());
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }

    let response = builder
        .send(request.body.as_slice())
        .map_err(|error| describe(error, target, request))?;

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or("<binary>").to_owned(),
            )
        })
        .collect();

    if (300..400).contains(&status) {
        // Refused before the body is read: the point is that we do not go there, not what is at it.
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("<unset>")
            .to_owned();
        return Err(ModelError::Redirect(format!(
            "{endpoint} answered {status} with Location {location}. This build does not follow \
             redirects, because one could carry the prompt and the credential to a host the \
             operator never configured; put the resolved address in model.base_url instead."
        )));
    }

    if let Some(declared) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
    {
        if declared > request.max_response_bytes {
            return Err(ModelError::ResponseTooLarge {
                read: declared,
                limit: request.max_response_bytes,
            });
        }
    }

    let mut body = Vec::new();
    response
        .into_body()
        .as_reader()
        .take(request.max_response_bytes as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| {
            ModelError::Protocol(format!(
                "reading the response body from {endpoint}: {error}"
            ))
        })?;

    if body.len() > request.max_response_bytes {
        return Err(ModelError::ResponseTooLarge {
            read: body.len(),
            limit: request.max_response_bytes,
        });
    }

    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

/// Turn a transport error into the one this build reports.
///
/// A header value never appears in the output of this function: the message reaches the task
/// record and the terminal, and the request headers are where the credential lives.
fn describe(error: ureq::Error, target: &Target, request: &OutgoingRequest) -> ModelError {
    use ureq::Error as Wire;
    let endpoint = target.dial(&request.path);
    match error {
        Wire::Timeout(_) => ModelError::TimedOut {
            timeout_secs: request.timeout.as_secs(),
        },
        Wire::HostNotFound => ModelError::Connect {
            endpoint,
            reason: "the endpoint host did not resolve".to_owned(),
        },
        Wire::RedirectFailed => ModelError::Redirect(format!(
            "{endpoint} could only be reached by a redirect, and redirects are not followed"
        )),
        Wire::StatusCode(code) => {
            ModelError::Protocol(format!("{endpoint} returned status {code}"))
        }
        Wire::Io(error) if target.scheme == "https" => ModelError::Connect {
            endpoint,
            reason: format!("{error}{}", TLS_EXPLAINED),
        },
        Wire::ConnectionFailed if target.scheme == "https" => ModelError::Connect {
            endpoint,
            reason: format!("the connection was not established{}", TLS_EXPLAINED),
        },
        Wire::Io(error) => ModelError::Connect {
            endpoint,
            reason: error.to_string(),
        },
        other => ModelError::Protocol(format!("{endpoint}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::{self, Stub};
    use std::sync::Arc;

    fn post(path: &str, timeout_secs: u64, max_bytes: usize) -> OutgoingRequest {
        OutgoingRequest {
            method: "POST",
            path: path.to_owned(),
            headers: vec![("authorization".to_owned(), "Bearer scratch".to_owned())],
            body: br#"{"hello":"world"}"#.to_vec(),
            timeout: Duration::from_secs(timeout_secs),
            max_response_bytes: max_bytes,
        }
    }

    #[test]
    fn endpoints_are_parsed_strictly() {
        let http = Target::parse("http://127.0.0.1:11434").expect("parse loopback");
        assert_eq!(http.scheme, "http");
        assert_eq!(http.host, "127.0.0.1");
        assert_eq!(http.port, 11434);
        assert!(http.is_loopback());
        assert_eq!(http.join("/api/chat"), "/api/chat");

        let prefixed = Target::parse("http://localhost:8080/v1/").expect("parse prefix");
        assert_eq!(prefixed.prefix, "/v1");
        assert_eq!(prefixed.join("/chat/completions"), "/v1/chat/completions");

        // HTTPS is a supported scheme now, with the port that implies.
        let secure = Target::parse("https://api.example.com").expect("parse https");
        assert_eq!(secure.port, PORT_HTTPS);
        assert!(!secure.is_loopback());
        assert_eq!(
            secure.dial("/chat/completions"),
            "https://api.example.com:443/chat/completions"
        );
        assert_eq!(
            Target::parse("http://example.com").expect("http").port,
            PORT_HTTP
        );

        // The dialled URL carries the prefix exactly once: adapters pass `join()`ed paths, and
        // joining inside `dial()` as well is what produced `/v1/v1/chat/completions` for llama.cpp.
        let with_prefix = Target::parse("http://127.0.0.1:9/v1").expect("parse prefixed");
        let path = with_prefix.join("/chat/completions");
        assert_eq!(
            with_prefix.dial(&path),
            "http://127.0.0.1:9/v1/chat/completions"
        );
    }

    #[test]
    fn a_url_that_could_carry_a_secret_is_refused() {
        for raw in [
            "http://user:pass@127.0.0.1:11434",
            "https://key:abc@@api.example.com",
            "http://127.0.0.1:11434?key=abc",
            "http://127.0.0.1:11434#frag",
            "ftp://127.0.0.1",
            "127.0.0.1:11434",
            "https://[::1]:11434",
        ] {
            assert!(
                Target::parse(raw).is_err(),
                "{raw:?} should be refused by the endpoint policy"
            );
        }
    }

    #[test]
    fn certificate_verification_is_never_switched_off_and_redirects_stay_unfollowed() {
        // The two settings that make "verified HTTPS, redirects off" more than a sentence in a doc
        // comment. If a future edit reaches for `disable_verification(true)` to make a test pass,
        // this fails first.
        let config = agent_config(&post("/api/chat", 5, 1024), None);
        let tls = config.tls_config();
        assert!(
            !tls.disable_verification(),
            "this build must not be able to accept a certificate the system rejects"
        );
        assert_eq!(
            tls.provider(),
            ureq::tls::TlsProvider::NativeTls,
            "the TLS provider must be the one that is compiled in"
        );
        assert!(
            matches!(tls.root_certs(), ureq::tls::RootCerts::PlatformVerifier),
            "roots must come from the platform store, not from a pinned set"
        );
        // The one substitution that exists in this file is the test-only anchor set, and what it
        // changes is *which* roots are trusted — never whether they are checked. `tls.rs` depends
        // on this staying true; if it ever gained the power to disable verification, its three
        // "rejected" results would prove nothing.
        let anchored = agent_config(&post("/api/chat", 5, 1024), Some(b"placeholder"));
        let pinned = anchored.tls_config();
        assert!(
            matches!(pinned.root_certs(), ureq::tls::RootCerts::Specific(_)),
            "an anchor must replace the roots, not add to them"
        );
        assert!(
            !pinned.disable_verification(),
            "an anchored test client must still verify"
        );
        assert_eq!(
            config.max_redirects(),
            0,
            "no redirect is followed, whatever a provider sends"
        );
        let timeouts = config.timeouts();
        let wanted = Some(Duration::from_secs(5));
        assert_eq!(
            timeouts.global, wanted,
            "the configured deadline is the budget"
        );
        assert_eq!(
            timeouts.connect, wanted,
            "connect is the phase a shutdown cannot interrupt, so it needs its own bound"
        );
        assert_eq!(timeouts.resolve, wanted, "name resolution likewise");
    }

    #[test]
    fn an_unreachable_endpoint_is_a_retryable_connect_failure() {
        // Nothing listens here; port 1 is reserved and never answers. The deadline is deliberately
        // far longer than the refusal: on this stack a loopback refusal takes about two seconds to
        // surface, and the point of the test is the error it produces, not the clock.
        let target = Target::parse("http://127.0.0.1:1").expect("parse");
        let cancel = AtomicBool::new(false);
        let error = send(&target, &post("/api/chat", 10, 1024), &cancel).expect_err("no listener");
        assert!(matches!(error, ModelError::Connect { .. }), "{error}");
        assert!(error.is_retryable(), "a connect failure may be tried again");
    }

    #[test]
    fn a_cancelled_flag_is_honoured_before_any_response() {
        let stub = Stub::spawn_dyn(|_| String::new());
        let target = Target::parse(&stub.url()).expect("parse stub");
        let cancel = AtomicBool::new(true);
        let error = send(&target, &post("/api/chat", 5, 1024), &cancel).expect_err("cancelled");
        assert_eq!(error.code(), "cancelled");
    }

    #[test]
    fn a_response_larger_than_the_ceiling_is_rejected_not_truncated() {
        let payload = "x".repeat(4096);
        let stub = Stub::spawn(stub::ok_body(&payload));
        let target = Target::parse(&stub.url()).expect("parse stub");
        let cancel = AtomicBool::new(false);
        let error = send(&target, &post("/api/chat", 5, 1024), &cancel).expect_err("oversized");
        match error {
            ModelError::ResponseTooLarge { read, limit } => {
                assert_eq!(limit, 1024);
                assert!(read > limit, "{read} should exceed the ceiling");
            }
            other => panic!("expected a size refusal, got {other}"),
        }
    }

    #[test]
    fn a_chunked_answer_is_decoded_and_the_ceiling_still_binds_it() {
        let cancel = AtomicBool::new(false);

        // No Content-Length anywhere: the read-side bound is the only thing that can hold here.
        let big = "y".repeat(4096);
        let stub = Stub::spawn(stub::chunked_body(&big, 1024));
        let target = Target::parse(&stub.url()).expect("parse stub");
        let error = send(&target, &post("/api/chat", 5, 1024), &cancel).expect_err("oversized");
        assert!(
            matches!(error, ModelError::ResponseTooLarge { .. }),
            "a chunked body must hit the ceiling, got {error}"
        );

        // And a chunked answer that fits arrives whole, which the previous client refused outright.
        let small = "z".repeat(500);
        let ok = Stub::spawn(stub::chunked_body(&small, 64));
        let target = Target::parse(&ok.url()).expect("parse stub");
        let response =
            send(&target, &post("/api/chat", 5, 1024), &cancel).expect("chunked decodes");
        assert_eq!(response.status, 200);
        assert_eq!(response.body_text(), small);
    }

    #[test]
    fn a_redirect_is_refused_because_its_destination_was_never_configured() {
        let redirect = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned();
        let stub = Stub::spawn(redirect);
        let target = Target::parse(&stub.url()).expect("parse stub");
        let cancel = AtomicBool::new(false);
        let error = send(&target, &post("/api/chat", 5, 1024), &cancel).expect_err("3xx refused");
        assert!(
            matches!(error, ModelError::Redirect(_)),
            "expected a redirect refusal, got {error}"
        );
        assert!(
            error.to_string().contains("127.0.0.1:9/elsewhere"),
            "the refusal should name the address it declined to visit"
        );
        assert!(!error.is_retryable(), "a redirect is not a transport fault");
        assert_eq!(
            stub.bodies().len(),
            1,
            "the client followed the Location it was handed"
        );
    }

    #[test]
    fn a_stalled_answer_is_cancelled_mid_flight() {
        // The stub accepts the request and never answers: the read parks until the pool is shut.
        let stub = Stub::spawn_stalling();
        let target = Target::parse(&stub.url()).expect("parse stub");
        let cancel = Arc::new(AtomicBool::new(false));
        let watching = {
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                cancel.store(true, Ordering::Relaxed);
            })
        };
        let started = Instant::now();
        let error = send(&target, &post("/api/chat", 30, 1024), &cancel).expect_err("cancelled");
        let elapsed = started.elapsed();
        watching.join().expect("watcher");
        assert_eq!(error.code(), "cancelled");
        assert!(
            elapsed < Duration::from_secs(5),
            "cancellation waited out the 30s deadline ({elapsed:?}); shutting the pool should \
             interrupt the read instead"
        );
    }

    #[test]
    fn a_deadline_reports_the_timeout_that_configuration_names() {
        let stub = Stub::spawn_dyn(|_| {
            std::thread::sleep(Duration::from_millis(1_500));
            stub::ok_body("{}")
        });
        let target = Target::parse(&stub.url()).expect("parse stub");
        let cancel = AtomicBool::new(false);
        let error = send(&target, &post("/api/chat", 1, 1024), &cancel).expect_err("deadline");
        assert!(
            matches!(error, ModelError::TimedOut { timeout_secs: 1 }),
            "expected the configured timeout to be named, got {error}"
        );
        assert!(error.is_retryable());
    }

    #[test]
    fn an_https_endpoint_reaches_tls_instead_of_being_refused_at_load() {
        // The stub speaks plain HTTP. Asking for https must fail inside the handshake, and the
        // sentence must no longer be "this build ships no TLS client".
        let stub = Stub::spawn(stub::ok_body("{}"));
        let secure = format!("https://{}", stub.authority());
        let target = Target::parse(&secure).expect("parse https url");
        let cancel = AtomicBool::new(false);
        let error = send(&target, &post("/api/chat", 10, 1024), &cancel)
            .expect_err("a plaintext server is not a TLS server");
        let text = error.to_string().to_ascii_lowercase();
        assert!(
            !text.contains("does not ship") && !text.contains("no tls client"),
            "HTTPS is still refused by policy rather than attempted: {text}"
        );
        assert!(
            matches!(error, ModelError::Connect { .. } | ModelError::Protocol(_)),
            "expected a handshake failure, got {error}"
        );
        assert!(
            text.contains("tls"),
            "the failure must name the TLS layer, which is the proof the handshake was attempted: \
             {text}"
        );
    }

    #[test]
    fn the_request_headers_reach_the_server_unchanged() {
        let stub = Stub::spawn(stub::ok_body("{}"));
        let target = Target::parse(&stub.url()).expect("parse stub");
        let cancel = AtomicBool::new(false);
        let response = send(&target, &post("/api/chat", 5, 1024), &cancel).expect("answered");
        assert_eq!(response.status, 200);
        let sent = stub.bodies().into_iter().next().expect("recorded request");
        let head = sent.to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer scratch"),
            "the header was not sent verbatim: {sent}"
        );
        assert!(head.contains("user-agent: jean-model/"), "{sent}");
        assert!(sent.contains("{\"hello\":\"world\"}"), "{sent}");
    }

    #[test]
    fn a_shared_cancellation_flag_is_an_atomic_bool() {
        // The planner hands the same flag to the transport and to the supervisor; this pins that
        // the type the CLI passes is the type this file polls.
        let flag = Arc::new(AtomicBool::new(false));
        let clone = Arc::clone(&flag);
        std::thread::spawn(move || clone.store(true, Ordering::Relaxed))
            .join()
            .expect("set");
        assert!(flag.load(Ordering::Relaxed));
    }
}
