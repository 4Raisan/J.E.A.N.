//! J.E.A.N. as the MCP **server**: a client on the other end of this process's stdio, and a list of
//! tools the operator chose.
//!
//! ## Two boundaries this module keeps
//!
//! * **What is advertised.** Only the tools in `tools` — built by `jean-core` from
//!   `[mcp] expose` — ever appear in `tools/list`. A tool that is not on that list is not offered and
//!   cannot be called, because the router refuses names it was not given rather than passing them to
//!   the host.
//! * **Who decides.** [`ToolHost::invoke`] is where the policy engine, the audit writer and the task
//!   records live. This crate asks, reports the answer, and has no way to make one itself — which is
//!   the same dependency direction the ACP connectors run under.
//!
//! ## Why the transport is this process's standard streams
//!
//! A client that speaks MCP to J.E.A.N. connects to a J.E.A.N. process the operator started, over the
//! pipes that process already owns. There is no listener, no port, and no session that outlives the
//! command. That is the roadmap's "local stdio initially" made literal: the only way in is the one a
//! person opened.

use std::fmt::Display;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleServer, ServerHandler, ServiceExt};

use crate::error::{McpError, Result};
use crate::Bounds;

/// One tool J.E.A.N. is willing to name to a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposedTool {
    /// The tool id, exactly as the operator wrote it in `[mcp] expose`.
    pub id: String,
    pub description: String,
    /// The argument schema the client is told about. Built by `jean-core` from the registry entry,
    /// so a description cannot promise a tool the registry does not have.
    pub input_schema: serde_json::Value,
}

impl ExposedTool {
    /// The wire form, with the schema converted once and refused if it is not an object schema.
    fn into_tool(self) -> std::result::Result<Tool, String> {
        let schema: rmcp::model::JsonObject = serde_json::from_value(self.input_schema.clone())
            .map_err(|e| format!("{}: the input schema is not a JSON object ({e})", self.id))?;
        Ok(Tool::new(self.id, self.description, Arc::new(schema)))
    }
}

/// The decisions and the side effects, which live in `jean-core` and not here.
///
/// `Send + Sync` is a compile-time requirement of the SDK's handler trait, not a design choice: the
/// implementation is expected to serialize whatever it does internally, because two clients on the
/// same stdio pair is not a thing this transport has, but the type system does not know that.
pub trait ToolHost: Send + Sync + 'static {
    /// Run one exposed tool and return the text a client should see.
    ///
    /// `Err(text)` is a tool-level failure — an MCP `isError` result — and is what a policy refusal
    /// must produce: a refusal the client can read, not a dropped connection.
    fn invoke(
        &self,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> std::result::Result<String, String>;
}

/// Serve the MCP over this process's stdio until the client disconnects.
///
/// The runtime is a current-thread one owned by this call: `rmcp` needs an async context, and
/// `block_on` keeps the whole session on the thread that started it, so a J.E.A.N. command that
/// becomes a server stays a single-threaded program.
pub fn serve<H: ToolHost>(
    server_name: &str,
    tools: Vec<ExposedTool>,
    host: H,
    bounds: &Bounds,
) -> Result<()> {
    let mut routed = Vec::with_capacity(tools.len());
    for tool in tools {
        routed.push(tool.into_tool().map_err(|reason| McpError::Protocol {
            server: server_name.to_owned(),
            reason,
        })?);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|e| McpError::Runtime {
            reason: e.to_string(),
        })?;

    let handler = JeanServer {
        host: Arc::new(host),
        tools: routed,
    };
    // `stdio()` is this process's standard input and output, driven through tokio's blocking pool.
    // `max_message_bytes` is applied here rather than left at the SDK's 16 MiB default.
    let (read, write) = rmcp::transport::stdio();
    let transport =
        AsyncRwTransport::new_server(read, write).with_max_line_length(bounds.max_message_bytes);
    let running = runtime
        .block_on(handler.serve(transport))
        .map_err(display_error(server_name, "the handshake"))?;
    runtime
        .block_on(running.waiting())
        .map_err(display_error(server_name, "the session"))?;
    Ok(())
}

fn display_error<'a, E: Display>(
    server: &'a str,
    stage: &'a str,
) -> impl FnOnce(E) -> McpError + 'a {
    move |e| McpError::Protocol {
        server: server.to_owned(),
        reason: format!("{stage} failed: {e}"),
    }
}

/// The handler: a fixed tool list and one host behind it.
struct JeanServer<H> {
    host: Arc<H>,
    tools: Vec<Tool>,
}

impl<H: ToolHost> ServerHandler for JeanServer<H> {
    fn get_info(&self) -> ServerConfig {
        let mut info = Implementation::new("jean", env!("CARGO_PKG_VERSION"));
        info.title = Some("J.E.A.N.".to_owned());
        info.description = Some(
            "J.E.A.N. as an MCP server. Every call is decided by the operator's policy engine."
                .to_owned(),
        );
        let mut config = InitializeResult::default();
        config.server_info = info;
        config.capabilities = ServerCapabilities::builder().enable_tools().build();
        config.protocol_version = ProtocolVersion::V_2025_11_25;
        config.instructions = Some(
            "Tools listed here are the ones the operator exposed. A call is decided by J.E.A.N.'s \
             policy engine before it runs, and a refusal is returned as a tool error rather than a \
             closed connection."
                .to_owned(),
        );
        config
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, rmcp::ErrorData> {
        Ok(self.advertised())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, rmcp::ErrorData> {
        Ok(self.dispatch(&request.name, request.arguments))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }
}

impl<H: ToolHost> JeanServer<H> {
    /// What `tools/list` answers: the exposed set, in the order the operator listed it.
    fn advertised(&self) -> ListToolsResult {
        ListToolsResult::with_all_items(self.tools.clone())
    }

    /// One `tools/call`, decided by name before the host is reached.
    ///
    /// The exposed list *is* the allowlist: a name that is not in it never reaches the code that can
    /// run something, whether or not a host would have known how to handle it.
    fn dispatch(&self, name: &str, arguments: Option<rmcp::model::JsonObject>) -> CallToolResponse {
        if !self.tools.iter().any(|tool| tool.name == name) {
            let reason = format!(
                "`{name}` is not exposed by this J.E.A.N. build; add it to [mcp] expose if the \
                 operator wants it published"
            );
            return CallToolResult::error(vec![ContentBlock::text(reason)]).into();
        }
        let arguments = arguments
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| serde_json::json!({}));
        match self.host.invoke(name, &arguments) {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]).into(),
            Err(reason) => CallToolResult::error(vec![ContentBlock::text(reason)]).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RefusingHost;

    impl ToolHost for RefusingHost {
        fn invoke(
            &self,
            tool: &str,
            _arguments: &serde_json::Value,
        ) -> std::result::Result<String, String> {
            Err(format!("the host refused {tool}"))
        }
    }

    struct EchoHost;

    impl ToolHost for EchoHost {
        fn invoke(
            &self,
            tool: &str,
            arguments: &serde_json::Value,
        ) -> std::result::Result<String, String> {
            Ok(format!(
                "{tool}:{}",
                serde_json::to_string(arguments).unwrap_or_default()
            ))
        }
    }

    fn schema() -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}})
    }

    fn tools() -> Vec<ExposedTool> {
        vec![ExposedTool {
            id: "echo.tool".to_owned(),
            description: "Echoes".to_owned(),
            input_schema: schema(),
        }]
    }

    fn served<H: ToolHost>(host: H) -> JeanServer<H> {
        JeanServer {
            host: Arc::new(host),
            tools: tools()
                .into_iter()
                .map(|tool| tool.into_tool().expect("the fixture schema is an object"))
                .collect(),
        }
    }

    fn text_of(response: CallToolResponse) -> String {
        let CallToolResponse::Complete(result) = response else {
            panic!("this build answers a tool call with a complete result");
        };
        result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .map(|text| text.text.clone())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn a_tool_is_refused_by_name_before_the_host_ever_sees_it() {
        // The allowlist is enforced at the call, not only in the list a client is shown: a name that
        // was never exposed must not reach the code that can run something.
        let server = served(EchoHost);
        let refused = text_of(server.dispatch("delete.tool", None));
        assert!(
            refused.contains("not exposed"),
            "an unpublished tool must be refused by name: {refused}"
        );
        let allowed = text_of(server.dispatch("echo.tool", None));
        assert!(allowed.contains("echo.tool"), "{allowed}");
    }

    #[test]
    fn a_host_refusal_comes_back_as_a_tool_error_the_client_can_read() {
        let server = served(RefusingHost);
        let response = server.dispatch("echo.tool", None);
        let CallToolResponse::Complete(result) = &response else {
            panic!("a refusal is a complete result");
        };
        assert_eq!(
            result.is_error,
            Some(true),
            "a refusal is the server's own tool-level no"
        );
        assert_eq!(text_of(response), "the host refused echo.tool");
    }

    #[test]
    fn a_schema_that_is_not_a_json_object_is_refused_before_the_server_starts() {
        let broken = ExposedTool {
            id: "broken.tool".to_owned(),
            description: String::new(),
            input_schema: serde_json::json!("a string is not a schema"),
        };
        let reason = broken.into_tool().expect_err("a schema must be an object");
        assert!(reason.contains("broken.tool"), "{reason}");
    }

    #[test]
    fn the_server_announces_only_tools_and_the_jean_identity() {
        let server = JeanServer {
            host: Arc::new(EchoHost),
            tools: Vec::new(),
        };
        let info = server.get_info();
        assert_eq!(info.server_info.name, "jean");
        assert!(
            info.capabilities.tools.is_some(),
            "a server that publishes nothing is not advertising tools/list"
        );
        assert_eq!(info.protocol_version, ProtocolVersion::V_2025_11_25);
        assert!(
            server.advertised().tools.is_empty(),
            "an empty exposure list is an honest empty answer, not a failure"
        );
        assert!(server.get_tool("echo.tool").is_none());
        let listed = served(EchoHost).advertised();
        assert_eq!(listed.tools.len(), 1);
        assert_eq!(listed.tools[0].name, "echo.tool");
    }
}
