//! What the workspace's tools look like to the model.
//!
//! `list_tools` answers with an index by default — one line per tool — and
//! full TypeScript declarations only when asked. Truncating a full dump would
//! be lossy and would vary with the workspace; an index is lossless and
//! strictly smaller. A single mainstream server's schemas can fill a whole
//! declaration budget on their own, so this is the common case, not a
//! precaution.
//!
//! Descriptions require a live `tools/list`, so the first call starts the
//! servers it needs. Cold start is paid at first disclosure rather than at
//! session start.

use std::sync::Arc;
use std::time::Duration;

use crate::config::Symposium;

use rmcp::model::Tool;
use serde_json::Value;
use tokio::sync::Mutex;

use super::declarations::{ToolDecl, binding_table, render_server};
use super::dispatch::{Binding, Namespace};
use super::resolve::{Rejection, Resolution, ResolvedServer};
use super::supervisor::{RestartPolicy, Supervisor};

/// How much to say about each tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Detail {
    /// Name and description. Enough to choose; not enough to call.
    #[default]
    Names,
    /// One TypeScript signature per tool.
    Signatures,
    /// Full declarations, including the types parameters refer to.
    Full,
}

impl Detail {
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("signatures") => Self::Signatures,
            Some("full") => Self::Full,
            _ => Self::Names,
        }
    }
}

/// Which tools to describe.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub servers: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub pattern: Option<String>,
    pub detail: Detail,
}

impl Query {
    /// Read a query from the tool's arguments, ignoring anything unrecognized.
    pub fn from_arguments(args: &Value) -> Self {
        Self {
            servers: string_list(args.get("servers")),
            tools: string_list(args.get("tools")),
            pattern: args
                .get("pattern")
                .and_then(Value::as_str)
                .map(str::to_string),
            detail: Detail::parse(args.get("detail").and_then(Value::as_str)),
        }
    }

    /// Naming specific servers or tools implies wanting their details.
    fn effective_detail(&self) -> Detail {
        if self.detail == Detail::Names && (self.servers.is_some() || self.tools.is_some()) {
            Detail::Full
        } else {
            self.detail
        }
    }

    fn wants_server(&self, name: &str) -> bool {
        self.servers
            .as_ref()
            .is_none_or(|only| only.iter().any(|s| s == name))
    }

    fn wants_tool(&self, name: &str) -> bool {
        if let Some(only) = &self.tools {
            if !only.iter().any(|t| t == name) {
                return false;
            }
        }
        match &self.pattern {
            Some(pattern) => glob_matches(pattern, name),
            None => true,
        }
    }
}

/// The workspace's backing servers, described on demand.
pub struct Catalog {
    entries: Vec<Entry>,
    read_only: bool,
    /// Servers that could not be used at all. Reported to the model rather
    /// than only logged: a server silently missing looks like a workspace
    /// that never declared it.
    rejected: Vec<Rejection>,
    policy: RestartPolicy,
    /// Needed to acquire an installation-backed server on first use.
    sym: Arc<Symposium>,
    /// Filter entries the caller named that match no server.
    known_names: Vec<String>,
}

struct Entry {
    resolved: ResolvedServer,
    /// Absent until first use. Building it may acquire an installation, which
    /// must not happen at startup — a client may spawn a throwaway copy of the
    /// meta-server just to probe it.
    supervisor: Mutex<Option<Supervisor>>,
}

impl Catalog {
    pub fn new(
        sym: Arc<Symposium>,
        resolution: Resolution,
        policy: RestartPolicy,
        read_only: bool,
    ) -> Self {
        let Resolution { servers, rejected } = resolution;
        let known_names = servers.iter().map(|s| s.name.clone()).collect();
        let entries = servers
            .into_iter()
            .map(|resolved| Entry {
                supervisor: Mutex::new(None),
                resolved,
            })
            .collect();
        Self {
            entries,
            read_only,
            rejected,
            policy,
            sym,
            known_names,
        }
    }

    /// The supervisor for an entry, acquiring what it needs the first time.
    async fn supervisor_for<'a>(
        &self,
        entry: &'a Entry,
    ) -> Result<tokio::sync::MutexGuard<'a, Option<Supervisor>>, String> {
        let mut guard = entry.supervisor.lock().await;
        if guard.is_none() {
            let spec = entry
                .resolved
                .spawn_spec(&self.sym)
                .await
                .map_err(|e| format!("{e:#}"))?;
            *guard = Some(Supervisor::new(spec, self.policy));
        }
        Ok(guard)
    }

    pub fn server_names(&self) -> Vec<String> {
        self.known_names.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Describe the matching tools.
    pub async fn describe(&self, query: &Query) -> String {
        if self.entries.is_empty() && self.rejected.is_empty() {
            return "No MCP servers apply to this workspace.".to_string();
        }

        let mut sections = Vec::new();
        // Refusals first: they explain an absence the model would otherwise
        // have to infer.
        let mut problems: Vec<String> = self
            .rejected
            .iter()
            .map(|r| format!("{}: {}", r.server, r.reason))
            .collect();

        for entry in &self.entries {
            if !query.wants_server(entry.resolved.name.as_str()) {
                continue;
            }

            let tools = match self.supervisor_for(entry).await {
                Ok(mut guard) => match guard.as_mut() {
                    Some(supervisor) => supervisor.list_tools().await.map_err(|e| e.to_string()),
                    None => unreachable!("supervisor_for leaves it present"),
                },
                Err(e) => Err(e),
            };
            let tools = match tools {
                Ok(tools) => tools,
                Err(e) => {
                    // A server that will not start is reported in place, so
                    // the absence of its tools has a visible reason.
                    problems.push(format!("{}: {e}", entry.resolved.name.as_str()));
                    continue;
                }
            };

            let visible: Vec<&Tool> = tools
                .iter()
                .filter(|t| entry.resolved.exposes(t.name.as_ref()))
                .filter(|t| !self.read_only || is_read_only(t))
                .filter(|t| query.wants_tool(t.name.as_ref()))
                .collect();

            if visible.is_empty() {
                // Silence here reads as a broken connection, so say why.
                if !tools.is_empty() && !query_narrows(query) {
                    problems.push(format!(
                        "{}: no tools visible ({} hidden by filters)",
                        entry.resolved.name.as_str(),
                        tools.len()
                    ));
                }
                continue;
            }

            sections.push(render(
                entry.resolved.name.as_str(),
                &visible,
                query.effective_detail(),
            ));
        }

        // Naming a server that does not exist is a mistake worth surfacing
        // rather than answering with silence.
        if let Some(requested) = &query.servers {
            for name in requested {
                if !self.known_names.iter().any(|k| k == name) {
                    problems.push(format!(
                        "no server named `{name}`. Available: {}",
                        self.known_names.join(", ")
                    ));
                }
            }
        }

        if sections.is_empty() && problems.is_empty() {
            return "No tools matched.".to_string();
        }

        let mut out = sections.join("\n");
        if !problems.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("Problems:\n");
            for problem in problems {
                out.push_str(&format!("  {problem}\n"));
            }
        }
        out
    }

    /// The namespaces a script sees, one per server.
    ///
    /// Building these needs each server's tool list, so this starts the
    /// servers that are not already running.
    pub async fn namespaces(&self) -> (Vec<Namespace>, Vec<String>) {
        let mut namespaces = Vec::new();
        let mut problems = Vec::new();

        for entry in &self.entries {
            let tools = match self.supervisor_for(entry).await {
                Ok(mut guard) => match guard.as_mut() {
                    Some(supervisor) => supervisor.list_tools().await.map_err(|e| e.to_string()),
                    None => unreachable!("supervisor_for leaves it present"),
                },
                Err(e) => Err(e),
            };
            let tools = match tools {
                Ok(tools) => tools,
                Err(e) => {
                    problems.push(format!("{}: {e}", entry.resolved.name.as_str()));
                    continue;
                }
            };

            // The table the declarations are rendered from, so every name the
            // model was shown dispatches.
            let visible: Vec<&str> = tools
                .iter()
                .filter(|t| entry.resolved.exposes(t.name.as_ref()))
                .filter(|t| !self.read_only || is_read_only(t))
                .map(|t| t.name.as_ref())
                .collect();

            let bindings: Vec<Binding> = binding_table(visible)
                .into_iter()
                .flat_map(|b| {
                    b.keys.into_iter().map(move |key| Binding {
                        key,
                        wire_name: b.wire_name.clone(),
                    })
                })
                .collect();

            if bindings.is_empty() {
                continue;
            }
            namespaces.push(Namespace {
                key: namespace_key(entry.resolved.name.as_str()),
                server: entry.resolved.name.as_str().to_string(),
                bindings,
            });
        }

        (namespaces, problems)
    }

    /// Call a tool on a backing server, honoring its filters.
    pub async fn call(&self, server: &str, tool: &str, args: Value) -> Result<Value, String> {
        let Some(entry) = self.entries.iter().find(|e| e.resolved.name == server) else {
            return Err(format!(
                "no server named `{server}`. Available: {}",
                self.known_names.join(", ")
            ));
        };
        if !entry.resolved.exposes(tool) {
            return Err(format!(
                "`{server}.{tool}` is not exposed by this workspace"
            ));
        }

        let timeout = entry.resolved.tool_call_timeout;
        let mut guard = self.supervisor_for(entry).await?;
        let supervisor = guard.as_mut().expect("supervisor_for leaves it present");
        supervisor
            .call(tool, args, timeout)
            .await
            .map_err(|e| e.to_string())
    }

    /// Close every running server.
    pub async fn shutdown(&self) {
        for entry in &self.entries {
            if let Some(supervisor) = entry.supervisor.lock().await.as_mut() {
                supervisor.shutdown().await;
            }
        }
    }

    /// How long a script may run against this catalog's servers.
    pub fn max_call_timeout(&self) -> Duration {
        self.entries
            .iter()
            .map(|e| e.resolved.tool_call_timeout)
            .max()
            .unwrap_or_default()
    }
}

/// A server's name as a JavaScript global.
fn namespace_key(server: &str) -> String {
    let mut out: String = server
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn query_narrows(query: &Query) -> bool {
    query.tools.is_some() || query.pattern.is_some()
}

fn is_read_only(tool: &Tool) -> bool {
    tool.annotations
        .as_ref()
        .and_then(|a| a.read_only_hint)
        .unwrap_or(false)
}

fn render(server: &str, tools: &[&Tool], detail: Detail) -> String {
    match detail {
        Detail::Names => {
            let mut out = format!("{server}:\n");
            for tool in tools {
                match tool.description.as_deref().map(str::trim) {
                    Some(text) if !text.is_empty() => {
                        out.push_str(&format!("  {} - {}\n", tool.name, first_line(text)));
                    }
                    _ => out.push_str(&format!("  {}\n", tool.name)),
                }
            }
            out
        }
        Detail::Signatures | Detail::Full => {
            // The schemas are owned so the declaration renderer, which works
            // in plain JSON, never sees a protocol type.
            let schemas: Vec<Option<Value>> = tools
                .iter()
                .map(|tool| {
                    (detail == Detail::Full).then(|| Value::Object((*tool.input_schema).clone()))
                })
                .collect();
            let decls: Vec<ToolDecl> = tools
                .iter()
                .zip(&schemas)
                .map(|(tool, schema)| ToolDecl {
                    name: tool.name.as_ref(),
                    description: tool.description.as_deref(),
                    // Signatures name the parameter; full spells out its shape.
                    input_schema: schema.as_ref(),
                })
                .collect();
            render_server(server, &decls)
        }
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    let array = value?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// Match a name against a pattern where `*` stands for any run of characters.
///
/// A dependency for this would be more machinery than the feature is worth.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match index {
            // A pattern not starting with `*` must match from the front.
            0 => match rest.strip_prefix(part) {
                Some(tail) => rest = tail,
                None => return false,
            },
            _ if index == parts.len() - 1 => {
                // The final piece must land at the end.
                return rest.ends_with(part) && rest.len() >= part.len();
            }
            _ => match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            },
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The global a script uses must be a legal identifier even when the
    /// server's declared name is not.
    #[test]
    fn namespace_keys_are_identifiers() {
        assert_eq!(namespace_key("sqlx"), "sqlx");
        assert_eq!(namespace_key("sea-orm"), "sea_orm");
        assert_eq!(namespace_key("2fa"), "_2fa");
    }

    #[test]
    fn detail_defaults_to_names() {
        assert_eq!(Detail::parse(None), Detail::Names);
        assert_eq!(Detail::parse(Some("nonsense")), Detail::Names);
        assert_eq!(Detail::parse(Some("full")), Detail::Full);
        assert_eq!(Detail::parse(Some("signatures")), Detail::Signatures);
    }

    /// Asking about specific servers or tools is a request for detail; making
    /// the caller also pass `detail` would be a needless second round trip.
    #[test]
    fn naming_a_target_implies_wanting_detail() {
        let query = Query {
            servers: Some(vec!["sqlx".into()]),
            ..Query::default()
        };
        assert_eq!(query.effective_detail(), Detail::Full);

        let unfiltered = Query::default();
        assert_eq!(unfiltered.effective_detail(), Detail::Names);
    }

    #[test]
    fn explicit_detail_is_respected_even_when_filtering() {
        let query = Query {
            servers: Some(vec!["sqlx".into()]),
            detail: Detail::Signatures,
            ..Query::default()
        };
        assert_eq!(query.effective_detail(), Detail::Signatures);
    }

    #[test]
    fn arguments_are_read_leniently() {
        let query = Query::from_arguments(&serde_json::json!({
            "servers": ["a", "b"],
            "pattern": "get_*",
            "detail": "full",
            "unrecognized": 1
        }));
        assert_eq!(query.servers, Some(vec!["a".into(), "b".into()]));
        assert_eq!(query.pattern.as_deref(), Some("get_*"));
        assert_eq!(query.detail, Detail::Full);
        assert_eq!(query.tools, None);
    }

    // -- glob --

    #[test]
    fn glob_matches_prefixes_suffixes_and_middles() {
        assert!(glob_matches("get_*", "get_user"));
        assert!(!glob_matches("get_*", "set_user"));
        assert!(glob_matches("*_user", "get_user"));
        assert!(!glob_matches("*_user", "get_team"));
        assert!(glob_matches("get_*_by_*", "get_user_by_id"));
        assert!(glob_matches("*", "anything"));
    }

    #[test]
    fn glob_without_a_wildcard_is_an_exact_match() {
        assert!(glob_matches("query", "query"));
        assert!(!glob_matches("query", "query2"));
    }
}
