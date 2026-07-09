//! Exposes the connector engine to the agent as an MCP server.
//!
//! `forge_web` hosts a Streamable-HTTP MCP endpoint (`/mcp`) with two tools —
//! `list_connectors` and `call_connector` — implemented directly on top of the
//! in-process connector engine ([`crate::connectors`]). Because Forge's own MCP
//! client is `rmcp` too, protocol compatibility is guaranteed. At startup the
//! server auto-registers this endpoint into Forge's MCP config so the agent
//! gets the tools with no manual setup.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars; // re-exported; the JsonSchema derive expands to `schemars::…`
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

/// The MCP server handler. Stateless — all state lives in the connector engine.
#[derive(Clone, Default)]
pub(crate) struct ConnectorsMcp;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CallArgs {
    /// The connector id from `list_connectors`, e.g. "gitlab".
    connector: String,
    /// The tool name on that connector, e.g. "list_my_issues".
    tool: String,
    /// Arguments for the tool, as a JSON object (may be empty).
    #[serde(default)]
    args: Value,
}

#[tool_router(server_handler)]
impl ConnectorsMcp {
    #[tool(
        description = "List the available connectors (declarative platform manifests) and the \
                       tools each one exposes. Call this first to discover what you can invoke."
    )]
    async fn list_connectors(&self) -> String {
        serde_json::to_string_pretty(&crate::connectors::catalog_json()).unwrap_or_default()
    }

    #[tool(
        description = "Invoke one tool of a connector against its platform's API. Provide the \
                       connector id, the tool name, and the tool's arguments as a JSON object. \
                       Returns { status, ok, data }."
    )]
    async fn call_connector(&self, Parameters(a): Parameters<CallArgs>) -> String {
        let out = match crate::connectors::dispatch_by_name(&a.connector, &a.tool, &a.args).await {
            Ok(v) => v,
            Err(e) => json!({ "error": e.message() }),
        };
        serde_json::to_string(&out).unwrap_or_default()
    }
}

/// Builds the `/mcp` Streamable-HTTP service to nest into the axum router.
pub(crate) fn service() -> StreamableHttpService<ConnectorsMcp, LocalSessionManager> {
    StreamableHttpService::new(
        || Ok(ConnectorsMcp),
        Default::default(),
        StreamableHttpServerConfig::default(),
    )
}
