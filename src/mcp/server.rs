//! The meta-server itself.
//!
//! One MCP server is registered with the agent, exposing two tools rather
//! than every plugin server's tools directly: `list_tools` describes what is
//! available, `execute` runs a script against it.
//!
//! Two constraints shape the process:
//!
//! * **stdout carries JSON-RPC.** Anything else written there corrupts the
//!   stream, so all reporting goes to stderr.
//! * **Startup has no side effects.** Some clients probe a server by
//!   spawning a throwaway copy before the real session, so anything done at
//!   startup happens twice, from a process nobody will talk to.

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServiceExt};
use serde_json::{Map, Value, json};

/// Tool that describes what is reachable.
pub const LIST_TOOLS: &str = "list_tools";
/// Tool that runs a script.
pub const EXECUTE: &str = "execute";

/// Serves the two meta-tools over stdio.
#[derive(Debug, Default, Clone)]
pub struct MetaServer {
    /// Names of the backing servers this workspace makes available.
    servers: Arc<Vec<String>>,
}

impl MetaServer {
    pub fn new(servers: Vec<String>) -> Self {
        Self {
            servers: Arc::new(servers),
        }
    }

    /// The inventory carried in `execute`'s description.
    ///
    /// Naming the servers costs a few tokens each and saves a round trip: a
    /// small workspace needs no discovery call at all. Their tools are not
    /// listed here, so the cost grows with servers rather than with schemas.
    fn execute_description(&self) -> String {
        let mut text = String::from(
            "Run a JavaScript program with the workspace's MCP tools in scope.\n\n\
             Each server is an object whose methods return promises, so a program \
             can call several tools, filter between them, and return only what \
             matters:\n\n  \
             const rows = await sqlx.query({ sql: \"SELECT id FROM users\" });\n  \
             return rows.filter(r => r.id > 100);\n\n\
             Write an async arrow function or a statement body that returns a value. \
             Write plain JavaScript: no type annotations, interfaces, or generics.\n\n",
        );
        if self.servers.is_empty() {
            text.push_str(
                "No MCP servers apply to this workspace. Nothing is in scope for `execute`.",
            );
        } else {
            text.push_str(&format!(
                "Servers in scope: {}.\nCall `{LIST_TOOLS}` for their tools and signatures.",
                self.servers.join(", ")
            ));
        }
        text
    }

    fn tool_definitions(&self) -> Vec<Tool> {
        let list = Tool::new(
            LIST_TOOLS,
            "Describe the MCP tools available in this workspace. Returns names and \
             descriptions by default; pass a filter or a higher detail level for full \
             TypeScript signatures.",
            object_schema(json!({
                "type": "object",
                "properties": {
                    "servers": {
                        "type": "array", "items": {"type": "string"},
                        "description": "Restrict to these servers."
                    },
                    "tools": {
                        "type": "array", "items": {"type": "string"},
                        "description": "Restrict to these tool names."
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Glob matched against tool names."
                    },
                    "detail": {
                        "type": "string", "enum": ["names", "signatures", "full"],
                        "description": "How much to return. Defaults to names."
                    }
                }
            })),
        )
        .annotate(ToolAnnotations::new().read_only(true));

        let execute = Tool::new(
            EXECUTE,
            self.execute_description(),
            object_schema(json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "The JavaScript program to run."
                    }
                },
                "required": ["script"]
            })),
        )
        // A script can call any tool in scope, so the annotation has to
        // describe the worst of them. Marking it read-only would let a
        // filtering client treat arbitrary tool use as safe.
        .annotate(
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .open_world(true),
        );

        vec![list, execute]
    }
}

impl ServerHandler for MetaServer {
    fn get_info(&self) -> ServerInfo {
        // The negotiated version is left to the SDK: answering with a version
        // the client did not offer is a hard error that closes the transport.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("symposium", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Symposium exposes this workspace's MCP tools through two tools. \
                 Start with `execute`; its description names the servers in scope.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match request.name.as_ref() {
            LIST_TOOLS => {
                let body = if self.servers.is_empty() {
                    "No MCP servers apply to this workspace.".to_string()
                } else {
                    format!("Servers in scope: {}.", self.servers.join(", "))
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(body)]).into())
            }
            EXECUTE => Ok(CallToolResult::error(vec![ContentBlock::text(
                "No MCP servers apply to this workspace, so there is nothing to call.",
            )])
            .into()),
            other => Err(McpError::invalid_params(
                format!("no such tool: {other}. Available: {LIST_TOOLS}, {EXECUTE}"),
                None,
            )),
        }
    }
}

fn object_schema(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Serve until the client disconnects.
pub async fn serve(servers: Vec<String>) -> anyhow::Result<()> {
    let service = MetaServer::new(servers)
        .serve(rmcp::transport::io::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_exactly_two_tools() {
        let names: Vec<String> = MetaServer::default()
            .tool_definitions()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names, vec![LIST_TOOLS.to_string(), EXECUTE.to_string()]);
    }

    /// A client filtering on read-only annotations must not conclude that
    /// running arbitrary code is safe.
    #[test]
    fn execute_is_annotated_as_destructive() {
        let tools = MetaServer::default().tool_definitions();
        let execute = tools.iter().find(|t| t.name == EXECUTE).unwrap();
        let annotations = execute.annotations.as_ref().expect("annotations");
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(true));
        assert_eq!(annotations.open_world_hint, Some(true));
    }

    #[test]
    fn list_tools_is_annotated_read_only() {
        let tools = MetaServer::default().tool_definitions();
        let list = tools.iter().find(|t| t.name == LIST_TOOLS).unwrap();
        assert_eq!(
            list.annotations.as_ref().and_then(|a| a.read_only_hint),
            Some(true)
        );
    }

    /// The inventory rides in the description so a small workspace needs no
    /// discovery round trip.
    #[test]
    fn execute_description_names_servers_in_scope() {
        let server = MetaServer::new(vec!["sqlx".into(), "sea_orm".into()]);
        let text = server.execute_description();
        assert!(text.contains("Servers in scope: sqlx, sea_orm."), "{text}");
        assert!(text.contains(LIST_TOOLS), "should point at the detail tool");
    }

    #[test]
    fn execute_description_is_explicit_when_nothing_applies() {
        let text = MetaServer::default().execute_description();
        assert!(text.contains("No MCP servers apply"), "{text}");
    }

    /// The model is shown TypeScript declarations, so it has to be told not
    /// to reply in TypeScript.
    #[test]
    fn execute_description_warns_against_type_annotations() {
        let text = MetaServer::default().execute_description();
        assert!(text.contains("no type annotations"), "{text}");
    }
}
