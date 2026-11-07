//! Wire protocol for the local cancel/status channel.
//!
//! ## The coordination mechanism, and why this one
//!
//! Cancellation has to reach a *different process* — the `jean run` invocation
//! that owns the child — from a second, short-lived `jean cancel` invocation.
//! The options are:
//!
//! | Mechanism | Rejected/adopted | Why |
//! |---|---|---|
//! | TCP listener on loopback | **rejected** | The task specification forbids a network listener, and loopback is still reachable by other local users and, with a firewall misconfiguration, from the network. |
//! | Signal file polled by the runner | rejected | Works, but adds up to one poll interval of latency and leaves a stale-flag cleanup problem. Kept as the documented fallback if named pipes prove unusable. |
//! | **Named pipe per task** | **adopted** | Instant, no polling, no network surface, and — decisively — a named pipe takes a `SECURITY_ATTRIBUTES` descriptor, so the DACL can be restricted to the owning user. A signal file inherits its ACL from the parent directory, which is weaker and less explicit. |
//!
//! One pipe **per task** (`<prefix>.<task-id>`) rather than one global pipe, so
//! that a `jean cancel` cannot reach a task it does not name, and so that a
//! crashed runner leaves at most one stale pipe rather than taking down a shared
//! endpoint.
//!
//! ## Framing
//!
//! One UTF-8 JSON object per line, request then response, then the server
//! disconnects. There is no session, no sequence number and no streaming: the
//! channel exists to deliver exactly one verb.
//!
//! ## Threat model
//!
//! The pipe name is guessable (it contains the task id), so the DACL is the
//! access control, not the name. `FILE_FLAG_FIRST_PIPE_INSTANCE` is required at
//! creation time: without it another local process could pre-create the name and
//! impersonate the runner, and a `jean cancel` would hand its request to the
//! attacker. A creation failure caused by the name already existing is therefore
//! treated as a hard error, never as "someone else is already serving this,
//! that's fine".

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// Default prefix. The full name is `\\.\pipe\jean.task.<task-id>`.
pub const DEFAULT_PIPE_PREFIX: &str = r"\\.\pipe\jean.task";

/// Upper bound on one request line. A cancel request is a few dozen bytes;
/// anything larger is a client bug or an attack, and must not be buffered.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientRequest {
    /// Terminate the task's whole process tree.
    Cancel { task_id: String },
    /// Ask the runner what it is doing, without changing anything.
    Status { task_id: String },
    /// Tell the runner to stop accepting work and exit once idle.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ServerResponse {
    Ok {
        detail: String,
    },
    /// The named task is not owned by this runner.
    NotFound {
        task_id: String,
    },
    /// The task exists but has already reached a terminal state.
    AlreadyFinished {
        task_id: String,
        state: String,
    },
    /// The request was understood but could not be carried out.
    Failed {
        detail: String,
    },
    /// The request could not be parsed. The connection is closed afterwards.
    Malformed {
        detail: String,
    },
}

impl ServerResponse {
    pub fn ok(detail: impl Into<String>) -> Self {
        ServerResponse::Ok {
            detail: detail.into(),
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        ServerResponse::Failed {
            detail: detail.into(),
        }
    }

    /// True when the CLI should exit non-zero.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            ServerResponse::Failed { .. } | ServerResponse::Malformed { .. }
        )
    }
}

/// Build the pipe name for a task id.
///
/// The id is validated again here even though the registry already validated
/// it, because this string becomes a kernel object name: a stray `\` or `.`
/// could redirect a client to a different pipe.
pub fn pipe_name_for(prefix: &str, task_id: &str) -> Result<String, ProtocolError> {
    validate_task_id(task_id)?;

    let prefix = if prefix.trim().is_empty() {
        DEFAULT_PIPE_PREFIX
    } else {
        prefix.trim()
    };

    Ok(format!("{prefix}.{task_id}"))
}

/// A task id must be safe to embed in a pipe name and a file name.
///
/// Allowed: ASCII alphanumerics, `-`, `_`, `.`. Length 1..=64. Rejecting `.`
/// sequences specifically prevents `..` from being used to traverse a pipe
/// namespace or an output directory.
pub fn validate_task_id(task_id: &str) -> Result<(), ProtocolError> {
    let bad = |reason: &str| ProtocolError::InvalidTaskId(task_id.to_owned(), reason.to_owned());

    if task_id.is_empty() {
        return Err(bad("must not be empty"));
    }
    if task_id.len() > 64 {
        return Err(bad("must be at most 64 bytes"));
    }
    if task_id == "." || task_id == ".." {
        return Err(bad("must not be a path traversal component"));
    }
    if !task_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(bad(
            "must contain only ASCII letters, digits, '-', '_' or '.'",
        ));
    }
    if task_id.contains("..") {
        return Err(bad("must not contain '..'"));
    }
    Ok(())
}

/// Generate a task id that satisfies [`validate_task_id`].
///
/// Deliberately dependency-free: a monotonically increasing component plus a
/// nanosecond timestamp plus the process id. Collisions require two ids
/// generated in the same nanosecond by the same process, which cannot happen,
/// or by different processes, which the pid component separates.
pub fn new_task_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();

    format!("t-{nanos:x}-{pid:x}-{seq:x}")
}

/// Encode a request as one line, ready to write to the pipe.
pub fn encode_request(req: &ClientRequest) -> Result<String, ProtocolError> {
    let mut line =
        serde_json::to_string(req).map_err(|e| ProtocolError::Serialization(e.to_string()))?;
    line.push('\n');
    Ok(line)
}

/// Decode one line into a request. Oversized input is rejected before parsing.
pub fn decode_request(line: &str) -> Result<ClientRequest, ServerResponse> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.len() > MAX_REQUEST_BYTES {
        return Err(ServerResponse::Malformed {
            detail: format!(
                "request is {} bytes, over the {MAX_REQUEST_BYTES}-byte limit",
                trimmed.len()
            ),
        });
    }
    serde_json::from_str(trimmed).map_err(|e| ServerResponse::Malformed {
        detail: format!("request is not a valid J.E.A.N. IPC message: {e}"),
    })
}

pub fn encode_response(resp: &ServerResponse) -> Result<String, ProtocolError> {
    let mut line =
        serde_json::to_string(resp).map_err(|e| ProtocolError::Serialization(e.to_string()))?;
    line.push('\n');
    Ok(line)
}

pub fn decode_response(line: &str) -> Result<ServerResponse, ProtocolError> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
        .map_err(|e| ProtocolError::Serialization(format!("bad server response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_valid_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = new_task_id();
            validate_task_id(&id).unwrap_or_else(|e| panic!("{id:?} rejected: {e}"));
            assert!(seen.insert(id), "duplicate id generated");
        }
    }

    #[test]
    fn task_id_validation_rejects_unsafe_input() {
        for bad in [
            "",
            ".",
            "..",
            "a..b",
            "../etc/passwd",
            r"\\.\pipe\evil",
            "has space",
            "semi;colon",
            "pipe|char",
            "quote\"char",
            "nul\0char",
            &"x".repeat(65),
        ] {
            assert!(
                validate_task_id(bad).is_err(),
                "{bad:?} must be rejected: it ends up in a kernel object name"
            );
        }
    }

    #[test]
    fn task_id_validation_accepts_the_shapes_we_generate() {
        for good in ["t-1", "task_2", "a.b.c", "ABC-123_xyz", &"y".repeat(64)] {
            validate_task_id(good).unwrap_or_else(|e| panic!("{good:?} rejected: {e}"));
        }
    }

    #[test]
    fn pipe_names_are_prefixed_and_namespaced() {
        let n = pipe_name_for("", "t-123").unwrap();
        assert_eq!(n, r"\\.\pipe\jean.task.t-123");

        let n = pipe_name_for(r"\\.\pipe\custom", "t-123").unwrap();
        assert_eq!(n, r"\\.\pipe\custom.t-123");
    }

    #[test]
    fn pipe_name_construction_refuses_an_unsafe_id() {
        // The id is validated at the point of use, not only at creation.
        assert!(pipe_name_for("", r"..\..\evil").is_err());
        assert!(pipe_name_for("", "").is_err());
    }

    #[test]
    fn requests_round_trip() {
        for req in [
            ClientRequest::Cancel {
                task_id: "t-1".to_owned(),
            },
            ClientRequest::Status {
                task_id: "t-1".to_owned(),
            },
            ClientRequest::Shutdown,
        ] {
            let line = encode_request(&req).unwrap();
            assert!(line.ends_with('\n'), "must be newline framed");
            assert_eq!(decode_request(&line).unwrap(), req);
        }
    }

    #[test]
    fn responses_round_trip() {
        for resp in [
            ServerResponse::ok("cancelled"),
            ServerResponse::failed("job termination returned an error"),
            ServerResponse::NotFound {
                task_id: "t-9".to_owned(),
            },
            ServerResponse::AlreadyFinished {
                task_id: "t-9".to_owned(),
                state: "succeeded".to_owned(),
            },
            ServerResponse::Malformed {
                detail: "bad json".to_owned(),
            },
        ] {
            let line = encode_response(&resp).unwrap();
            assert_eq!(decode_response(&line).unwrap(), resp);
        }
    }

    #[test]
    fn the_wire_format_is_stable() {
        // Changing these strings breaks every already-installed CLI, so the
        // change must be deliberate and paired with a config_version bump.
        let line = encode_request(&ClientRequest::Cancel {
            task_id: "t-1".to_owned(),
        })
        .unwrap();
        assert_eq!(line, "{\"op\":\"cancel\",\"task_id\":\"t-1\"}\n");

        let line = encode_response(&ServerResponse::NotFound {
            task_id: "t-1".to_owned(),
        })
        .unwrap();
        assert_eq!(line, "{\"result\":\"not_found\",\"task_id\":\"t-1\"}\n");
    }

    #[test]
    fn oversized_requests_are_rejected_before_parsing() {
        let huge = format!("{{\"op\":\"cancel\",\"task_id\":\"{}\"}}", "a".repeat(9000));
        let resp = decode_request(&huge).expect_err("must reject");
        let rendered = format!("{resp:?}");
        assert!(
            matches!(resp, ServerResponse::Malformed { .. }),
            "{rendered}"
        );
        assert!(
            rendered.contains("limit"),
            "the refusal must explain itself: {rendered}"
        );
    }

    #[test]
    fn garbage_input_yields_malformed_not_a_panic() {
        for bad in ["", "   ", "not json", "{\"op\":42}", "{}", "[1,2,3]"] {
            let resp = decode_request(bad).expect_err("must reject");
            assert!(
                matches!(resp, ServerResponse::Malformed { .. }),
                "{bad:?} -> {resp:?}"
            );
        }
    }

    #[test]
    fn failure_classification_drives_the_exit_code() {
        assert!(!ServerResponse::ok("x").is_failure());
        assert!(ServerResponse::failed("x").is_failure());
        assert!(ServerResponse::Malformed { detail: "x".into() }.is_failure());
        // NotFound and AlreadyFinished are not CLI failures: the user asked to
        // cancel something that is already not running, which is the desired end
        // state. The CLI reports it and exits zero.
        assert!(!ServerResponse::NotFound {
            task_id: "t".into()
        }
        .is_failure());
        assert!(!ServerResponse::AlreadyFinished {
            task_id: "t".into(),
            state: "succeeded".into()
        }
        .is_failure());
    }
}
