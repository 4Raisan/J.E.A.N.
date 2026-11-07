//! J.E.A.N. as the MCP **client**: it starts a server, speaks over the pipes it made, and stops it.
//!
//! ## Why the session owns the child, the runtime and the timeout
//!
//! `rmcp` is async, so this module owns a current-thread tokio runtime — one per session, on the
//! calling thread, driven by `block_on`. It is not a second scheduler: nothing here spawns onto a
//! worker pool, the protocol task lives and dies inside one call, and the crate boundary
//! (`jean-mcp` → `tokio`) is exactly where the roadmap put the runtime.
//!
//! The server process comes from [`jean_process::piped::spawn`] with a `LaunchSpec` **someone else
//! built**: this crate cannot resolve an executable, choose a working directory, or assemble an
//! environment. That is `jean-core`'s job, and it is the same rule the ACP connectors run under — a
//! protocol library does not get to decide what binary runs on the machine.
//!
//! ## What the bounds do
//!
//! `max_message_bytes` is handed to the transport as its line ceiling. The SDK default is 16 MiB per
//! frame; leaving it there would make `[mcp] max_message_bytes` a setting with no effect, and a
//! setting nobody can see working is a setting nobody audits. `max_result_bytes` caps what one tool
//! result contributes to the caller, and `request_timeout` bounds every round trip including the
//! handshake.

use std::fmt::Display;
use std::time::{Duration, Instant};

use jean_process::piped::PipedChild;
use jean_process::spec::LaunchSpec;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, ProtocolVersion,
};
use rmcp::service::RunningService;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleClient, ServiceExt};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

use crate::error::{McpError, Result};
use crate::Bounds;

/// What one server tool is, as the server itself describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// The server's own JSON Schema for the arguments, kept as data.
    ///
    /// This build does not validate arguments against it before sending. That is deliberate: a
    /// schema the server published is the server's claim about itself, and J.E.A.N.'s authority over
    /// a call comes from the policy decision, not from a document the peer wrote.
    pub input_schema: serde_json::Value,
}

/// What a call returned, as observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutcome {
    pub tool: String,
    /// The text content of the result, capped at `max_result_bytes`.
    pub text: String,
    /// The server's own tool-level failure. A real answer, not a transport problem.
    pub is_error: bool,
    /// Bytes the server actually returned, before any truncation.
    pub bytes: u64,
    pub truncated: bool,
}

/// One open MCP session: the child, the runtime that speaks to it, and the connection.
///
/// Dropping it closes the protocol session, then takes the job down. A server left running because a
/// caller forgot to shut it down is a process the operator cannot account for, so teardown lives in
/// [`Drop`] and not in a method someone might skip.
pub struct McpSession {
    server: String,
    bounds: Bounds,
    runtime: tokio::runtime::Runtime,
    child: PipedChild,
    client: Option<RunningService<RoleClient, JeanClient>>,
}

impl McpSession {
    /// Start `launch` and complete the MCP handshake.
    pub fn open(server: &str, launch: &LaunchSpec, bounds: &Bounds) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|e| McpError::Runtime {
                reason: e.to_string(),
            })?;
        let child = jean_process::piped::spawn(launch)?;
        let stdin = child.stdin_file()?;
        let stdout = child.stdout_file()?;

        // `blocking::Unblock` keeps a blocked pipe read off the runtime's single worker: the reader
        // parks on its own thread and wakes the task when bytes arrive. Without it, a server that
        // answers slowly would idle the whole runtime and `request_timeout` could not fire.
        let incoming = blocking::Unblock::new(stdout).compat();
        let outgoing = blocking::Unblock::new(stdin).compat_write();
        let transport = AsyncRwTransport::new_client(incoming, outgoing)
            .with_max_line_length(bounds.max_message_bytes);

        let server = server.to_owned();
        let handler = JeanClient {
            server: server.clone(),
        };
        let client = runtime.block_on(bounded(
            bounds.request_timeout,
            handler.serve(transport),
            &server,
            "the handshake",
        ))?;

        Ok(Self {
            server,
            bounds: bounds.clone(),
            runtime,
            child,
            client: Some(client),
        })
    }

    /// The tools this server offers.
    pub fn list_tools(&mut self) -> Result<Vec<ToolInfo>> {
        let (timeout, server) = (self.bounds.request_timeout, self.server.clone());
        let client = self.client.as_mut().ok_or_else(|| McpError::Protocol {
            server: server.clone(),
            reason: "the session is already closed".to_owned(),
        })?;
        let tools = self.runtime.block_on(bounded(
            timeout,
            client.list_all_tools(),
            &server,
            "tools/list",
        ))?;
        Ok(tools
            .into_iter()
            .map(|tool| ToolInfo {
                name: tool.name.to_string(),
                description: tool
                    .description
                    .map(|text| text.to_string())
                    .unwrap_or_default(),
                input_schema: serde_json::to_value(tool.input_schema.as_ref())
                    .unwrap_or_else(|_| serde_json::json!({})),
            })
            .collect())
    }

    /// Call one tool and return the observed result, capped at `max_result_bytes`.
    ///
    /// A tool-level failure stays in `CallOutcome::is_error` rather than becoming `Err`, because the
    /// caller has to report which of the two happened: the server refusing the operation is evidence,
    /// the server not answering is a fault.
    pub fn call(&mut self, tool: &str, arguments: serde_json::Value) -> Result<CallOutcome> {
        let (timeout, server) = (self.bounds.request_timeout, self.server.clone());
        let client = self.client.as_mut().ok_or_else(|| McpError::Protocol {
            server: server.clone(),
            reason: "the session is already closed".to_owned(),
        })?;
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Some(map) = arguments.as_object() {
            params = params.with_arguments(map.clone());
        }
        let request = format!("tools/call {tool}");
        let result = self.runtime.block_on(bounded(
            timeout,
            client.call_tool(params),
            &server,
            &request,
        ))?;

        let full = result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .map(|text| text.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let bytes = full.len();
        let truncated = bytes > self.bounds.max_result_bytes;
        let mut text = full;
        if truncated {
            // Cut at a boundary that cannot split a character: text truncated mid-codepoint is
            // something the operator can neither read nor quote.
            let mut keep = self.bounds.max_result_bytes;
            while keep > 0 && !text.is_char_boundary(keep) {
                keep -= 1;
            }
            text.truncate(keep);
        }
        // Anything that is not text — an image, a resource link — is counted, not silently dropped,
        // so "the tool returned nothing" and "the tool returned a picture" stay different sentences.
        let non_text = result
            .content
            .iter()
            .filter(|block| block.as_text().is_none())
            .count();
        if non_text > 0 {
            text.push_str(&format!("\n[{non_text} non-text content block(s) omitted]"));
        }

        Ok(CallOutcome {
            tool: tool.to_owned(),
            text,
            is_error: result.is_error == Some(true),
            bytes: bytes as u64,
            truncated,
        })
    }

    /// The pid of the server process, for the run row that says which child did the work.
    pub fn pid(&self) -> u32 {
        self.child.pid()
    }

    /// Close the session and take the job down with it.
    pub fn close(&mut self) -> Option<i32> {
        if let Some(client) = self.client.as_mut() {
            let grace = self.bounds.shutdown_grace;
            let _ = self
                .runtime
                .block_on(async { tokio::time::timeout(grace, client.close()).await });
        }
        self.client = None;
        let control = self.child.control();
        let until = Instant::now() + self.bounds.shutdown_grace;
        while control.is_running() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        if control.is_running() {
            // It did not leave politely. The job object takes the whole tree, which is precisely why
            // this crate does not use the SDK's own process handling.
            let _ = control.terminate();
        }
        control.exit_code_if_settled()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        if self.client.is_some() {
            self.close();
        }
    }
}

/// The client identity sent in the handshake.
///
/// Capabilities are deliberately empty. Sampling, roots and elicitation are things a *server* may ask
/// a client to do, and J.E.A.N. does not give a child process a route to its operator from inside a
/// tool call; declining them at the handshake is clearer than answering each request later, and the
/// SDK's default handler refuses whatever arrives anyway.
#[derive(Clone)]
pub struct JeanClient {
    server: String,
}

impl rmcp::ClientHandler for JeanClient {
    fn get_info(&self) -> ClientConfig {
        let mut info = Implementation::new("jean", env!("CARGO_PKG_VERSION"));
        info.title = Some("J.E.A.N.".to_owned());
        // Which server this session belongs to, in the peer's own handshake record: if a server logs
        // its clients, it should be able to see it was J.E.A.N. and not something unnamed.
        info.description = Some(format!("MCP client session with server {:?}", self.server));
        let mut config = ClientConfig::new(ClientCapabilities::default(), info);
        // The newest version that still performs an `initialize` handshake: a stdio session with no
        // handshake is a session where neither side has said who it is.
        config.protocol_version = ProtocolVersion::V_2025_11_25;
        config
    }
}

/// One future, bounded by wall clock, with the caller's labels on the error.
async fn bounded<T, E>(
    timeout: Duration,
    future: impl std::future::Future<Output = std::result::Result<T, E>>,
    server: &str,
    request: &str,
) -> Result<T>
where
    E: Display,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(McpError::Protocol {
            server: server.to_owned(),
            reason: e.to_string(),
        }),
        Err(_) => Err(McpError::Timeout {
            server: server.to_owned(),
            request: request.to_owned(),
            timeout,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(max_result_bytes: usize) -> Bounds {
        Bounds {
            max_message_bytes: 4096,
            request_timeout: Duration::from_secs(1),
            max_result_bytes,
            shutdown_grace: Duration::from_millis(50),
        }
    }

    #[test]
    fn a_result_over_the_cap_is_cut_at_a_character_boundary_and_marked() {
        // The truncation rule is the one place this module touches bytes it did not produce, so it is
        // tested without a server: a cut that splits a character yields text nobody can print.
        // "h?llo " where ? is the two-byte e-acute. Byte 2 is inside that character, so a cut there
        // must back off to byte 1 rather than emit half of it.
        let full =
            String::from_utf8(vec![b'h', 0xc3, 0xa9, b'l', b'l', b'o', b' ']).expect("utf-8");
        assert_eq!(full.len(), 7);
        assert_eq!(full.chars().count(), 6);
        for wanted in [2usize, 3, 5, 6, 7] {
            let mut text = full.clone();
            let mut keep = wanted;
            while keep > 0 && !text.is_char_boundary(keep) {
                keep -= 1;
            }
            text.truncate(keep);
            assert!(
                text.is_char_boundary(text.len()),
                "a cut at {wanted} produced a partial character"
            );
            assert!(text.len() <= wanted);
        }
        // The specific case the loop exists for.
        assert!(!full.is_char_boundary(2));
        let mut text = full.clone();
        text.truncate(1);
        assert_eq!(text, "h");
    }

    #[test]
    fn the_client_identity_names_jean_and_offers_no_capabilities() {
        use rmcp::ClientHandler as _;
        let client = JeanClient {
            server: "fixture".to_owned(),
        };
        let config = client.get_info();
        assert_eq!(config.client_info.name, "jean");
        assert_eq!(config.client_info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(config.protocol_version, ProtocolVersion::V_2025_11_25);
        assert_eq!(
            config.capabilities,
            ClientCapabilities::default(),
            "no sampling, no roots, no elicitation: a child does not get to ask the operator"
        );
    }

    /// A `cmd.exe` child with `args`, carrying the minimum environment Windows needs to start it.
    fn cmd_launch(args: &[&str]) -> LaunchSpec {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:/Windows".to_owned());
        let root = std::path::Path::new(&system_root);
        let mut env = std::collections::BTreeMap::new();
        env.insert("SystemRoot".to_owned(), system_root.clone());
        env.insert(
            "PATH".to_owned(),
            root.join("System32").display().to_string(),
        );
        LaunchSpec {
            executable: root.join("System32").join("cmd.exe"),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            working_dir: None,
            env,
            timeout: None,
            output_cap_bytes: 4096,
            stdout_path: None,
            stderr_path: None,
            terminal: jean_process::TerminalMode::Pipes,
        }
    }

    #[test]
    fn a_server_that_speaks_plain_text_is_refused_instead_of_hanging() {
        // `cmd.exe` answering with a line that is not JSON-RPC is this transport's malformed-frame
        // case: the handshake has to fail with a reportable reason rather than wait for a peer that
        // will never say anything. A test that only read a configuration flag would prove nothing.
        let launch = cmd_launch(&["/c", "echo", "this is not json-rpc"]);
        let text = match McpSession::open("text-speaker", &launch, &bounds(1024)) {
            Err(e) => e.to_string(),
            Ok(mut session) => {
                session.close();
                panic!("a peer that prints text is not an MCP server")
            }
        };
        assert!(
            text.contains("text-speaker"),
            "the failure must name the server that failed it: {text}"
        );
    }

    #[test]
    fn a_server_that_never_answers_is_stopped_by_the_request_timeout() {
        // A live peer that says nothing is the stalled-worker case. `request_timeout_secs` bounds the
        // handshake, so the command ends with an explanation instead of hanging until the operator
        // gives up, and the child — along with the `ping` it spawned — goes with the job.
        let launch = cmd_launch(&["/c", "ping", "-n", "30", "127.0.0.1"]);
        let started = Instant::now();
        let text = match McpSession::open("quiet-server", &launch, &bounds(1024)) {
            Err(e) => e.to_string(),
            Ok(mut session) => {
                session.close();
                panic!("a peer that never spoke cannot have completed a handshake")
            }
        };
        assert!(
            text.contains("did not answer") && text.contains("the handshake"),
            "the failure must be the timeout, in the operator's words: {text}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the bound is 1 s; this waited {:?}",
            started.elapsed()
        );
    }
}
