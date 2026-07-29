//! Deciding which backing servers a workspace makes available.
//!
//! The same predicate filtering that decides which skills install decides
//! which MCP servers are in scope, so a workspace only ever sees tools
//! belonging to crates it actually depends on. That conditionality is the
//! thing no MCP primitive can express, and it is why the meta-server exists.
//!
//! Nothing is started here. Resolution is a read of the plugin registry;
//! processes begin on first use.

use std::path::Path;
use std::time::Duration;

use sacp::schema::McpServer;

use crate::config::Symposium;
use crate::mcp::client::SpawnSpec;
use crate::mcp::server::{EXECUTE, LIST_TOOLS};
use crate::plugins::McpServerOverrides;
use crate::pm::PackageManager;

/// A backing server, ready to be started on demand.
#[derive(Debug, Clone)]
pub struct ResolvedServer {
    pub spec: SpawnSpec,
    /// Ceiling on one call to this server, already reconciled with the
    /// user's script deadline.
    pub tool_call_timeout: Duration,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Option<Vec<String>>,
}

impl ResolvedServer {
    pub fn name(&self) -> &str {
        &self.spec.name
    }

    /// Whether a plugin's filters let this tool through.
    pub fn exposes(&self, tool: &str) -> bool {
        if let Some(allow) = &self.enabled_tools {
            return allow.iter().any(|t| t == tool);
        }
        if let Some(deny) = &self.disabled_tools {
            return !deny.iter().any(|t| t == tool);
        }
        true
    }
}

/// What resolution produced, including what it had to refuse.
#[derive(Debug, Default)]
pub struct Resolution {
    pub servers: Vec<ResolvedServer>,
    /// Servers that could not be used, and why. Reported rather than
    /// swallowed: a server silently missing looks like a broken workspace.
    pub rejected: Vec<Rejection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub server: String,
    pub reason: String,
}

/// Resolve the servers applicable to the workspace containing `cwd`.
pub fn resolve(sym: &Symposium, cwd: &Path) -> Resolution {
    let mut deps = sym.workspace_deps(cwd);
    let Some(loaded) = deps.load() else {
        // Outside a Rust workspace there is nothing to condition on.
        return Resolution::default();
    };
    let loaded = loaded.clone();
    let registry = crate::plugins::load_registry_with_workspace(sym, Some(&loaded));

    let dep_ids = crate::pm::CargoPm.list_deps(&loaded.crates);
    let mut ctx = crate::predicate::PredicateContext::new(&dep_ids);

    let mut entries: Vec<(&crate::plugins::PluginMcpServer, String)> = Vec::new();
    for plugin in &registry.plugins {
        if !plugin.applies(&mut ctx) {
            continue;
        }
        let owner = plugin.plugin.name.clone();
        for entry in plugin.plugin.applicable_mcp_entries(&mut ctx) {
            entries.push((entry, owner.clone()));
        }
    }

    build(entries, sym.config.mcp.script_timeout_secs)
}

/// Turn applicable manifest entries into runnable servers.
fn build(
    entries: Vec<(&crate::plugins::PluginMcpServer, String)>,
    script_timeout_secs: u64,
) -> Resolution {
    let mut resolution = Resolution::default();
    // Which plugin claimed each name, so a clash can name both sides.
    let mut claimed: Vec<(String, String)> = Vec::new();

    for (entry, owner) in entries {
        let name = server_name(&entry.server).to_string();

        // The meta-server's own tools live in the same namespace as the
        // servers it exposes; a backing server taking one would shadow it.
        if name == LIST_TOOLS || name == EXECUTE {
            resolution.rejected.push(Rejection {
                server: name,
                reason: format!("`{owner}` uses a name reserved by the meta-server"),
            });
            continue;
        }

        // First-wins would silently drop one plugin's server, and a warning
        // on a stdio server's stderr is invisible. Refusing names both.
        if let Some((_, first)) = claimed.iter().find(|(n, _)| *n == name) {
            resolution.rejected.push(Rejection {
                server: name.clone(),
                reason: format!("declared by both `{first}` and `{owner}`"),
            });
            continue;
        }

        let McpServer::Stdio(stdio) = &entry.server else {
            resolution.rejected.push(Rejection {
                server: name,
                reason: "only stdio servers are supported".to_string(),
            });
            continue;
        };

        claimed.push((name.clone(), owner));
        resolution.servers.push(ResolvedServer {
            spec: SpawnSpec {
                name: name.clone(),
                command: stdio.command.clone(),
                args: stdio.args.clone(),
                env: stdio
                    .env
                    .iter()
                    .map(|v| (v.name.clone(), v.value.clone()))
                    .collect(),
                startup_timeout: Duration::from_secs(
                    entry.overrides.startup_timeout_secs.unwrap_or(30),
                ),
            },
            tool_call_timeout: call_timeout(&entry.overrides, script_timeout_secs),
            enabled_tools: entry.overrides.enabled_tools.clone(),
            disabled_tools: entry.overrides.disabled_tools.clone(),
        });
    }

    resolution.servers.sort_by(|a, b| a.name().cmp(b.name()));
    resolution
}

/// Reconcile a plugin's call timeout with the user's script deadline.
///
/// A plugin author cannot see the user's configuration, so an override
/// longer than the whole script budget is clamped rather than rejected —
/// refusing to load a server because a user lowered their own limit would
/// punish the wrong person.
fn call_timeout(overrides: &McpServerOverrides, script_timeout_secs: u64) -> Duration {
    let requested = overrides.tool_call_timeout_secs.unwrap_or(60);
    // Leave the script deadline strictly larger, or the call timeout could
    // never fire.
    let ceiling = script_timeout_secs.saturating_sub(1).max(1);
    Duration::from_secs(requested.min(ceiling))
}

fn server_name(server: &McpServer) -> &str {
    match server {
        McpServer::Stdio(s) => &s.name,
        McpServer::Http(s) => &s.name,
        McpServer::Sse(s) => &s.name,
        _ => "<unknown>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginMcpServer;
    use sacp::schema::McpServerStdio;

    fn stdio(name: &str) -> PluginMcpServer {
        PluginMcpServer {
            predicates: Default::default(),
            overrides: McpServerOverrides::default(),
            server: McpServer::Stdio(McpServerStdio::new(name, "/usr/bin/true")),
        }
    }

    fn resolve_all(entries: Vec<(&PluginMcpServer, &str)>, script_secs: u64) -> Resolution {
        build(
            entries
                .into_iter()
                .map(|(e, owner)| (e, owner.to_string()))
                .collect(),
            script_secs,
        )
    }

    #[test]
    fn stdio_servers_become_spawnable() {
        let entry = stdio("sqlx");
        let out = resolve_all(vec![(&entry, "db-plugin")], 120);

        assert_eq!(out.servers.len(), 1);
        assert_eq!(out.servers[0].name(), "sqlx");
        assert!(out.rejected.is_empty());
    }

    /// A silently missing server looks like a broken workspace, so refusals
    /// are reported.
    #[test]
    fn http_servers_are_refused_with_a_reason() {
        let entry = PluginMcpServer {
            predicates: Default::default(),
            overrides: McpServerOverrides::default(),
            server: McpServer::Http(sacp::schema::McpServerHttp::new(
                "remote",
                "http://localhost:8080/mcp",
            )),
        };
        let out = resolve_all(vec![(&entry, "p")], 120);

        assert!(out.servers.is_empty());
        assert_eq!(out.rejected.len(), 1);
        assert!(out.rejected[0].reason.contains("stdio"));
    }

    /// First-wins would drop one plugin's server silently, and a warning on
    /// a stdio server's stderr is invisible.
    #[test]
    fn duplicate_names_are_refused_naming_both_plugins() {
        let a = stdio("sqlx");
        let b = stdio("sqlx");
        let out = resolve_all(vec![(&a, "first-plugin"), (&b, "second-plugin")], 120);

        assert_eq!(out.servers.len(), 1, "the first still works");
        assert_eq!(out.rejected.len(), 1);
        let reason = &out.rejected[0].reason;
        assert!(
            reason.contains("first-plugin") && reason.contains("second-plugin"),
            "both sides should be named, got: {reason}"
        );
    }

    /// A backing server called `execute` would shadow the meta-server's own
    /// tool.
    #[test]
    fn reserved_names_are_refused() {
        for name in [LIST_TOOLS, EXECUTE] {
            let entry = stdio(name);
            let out = resolve_all(vec![(&entry, "p")], 120);
            assert!(out.servers.is_empty(), "{name} should be refused");
            assert!(out.rejected[0].reason.contains("reserved"));
        }
    }

    #[test]
    fn per_server_timeouts_are_honored() {
        let mut entry = stdio("slow");
        entry.overrides.startup_timeout_secs = Some(45);
        entry.overrides.tool_call_timeout_secs = Some(90);
        let out = resolve_all(vec![(&entry, "p")], 300);

        assert_eq!(out.servers[0].spec.startup_timeout, Duration::from_secs(45));
        assert_eq!(out.servers[0].tool_call_timeout, Duration::from_secs(90));
    }

    /// A plugin author cannot see the user's configuration, so an override
    /// beyond the script budget is clamped rather than refused.
    #[test]
    fn call_timeout_is_clamped_below_the_script_deadline() {
        let mut entry = stdio("slow");
        entry.overrides.tool_call_timeout_secs = Some(600);
        let out = resolve_all(vec![(&entry, "p")], 30);

        assert_eq!(
            out.servers[0].tool_call_timeout,
            Duration::from_secs(29),
            "must stay strictly under the script deadline or it can never fire"
        );
    }

    // -- tool filters --

    #[test]
    fn an_allow_list_hides_everything_else() {
        let mut entry = stdio("sqlx");
        entry.overrides.enabled_tools = Some(vec!["query".into()]);
        let out = resolve_all(vec![(&entry, "p")], 120);

        assert!(out.servers[0].exposes("query"));
        assert!(!out.servers[0].exposes("drop_table"));
    }

    #[test]
    fn a_deny_list_hides_only_what_it_names() {
        let mut entry = stdio("sqlx");
        entry.overrides.disabled_tools = Some(vec!["drop_table".into()]);
        let out = resolve_all(vec![(&entry, "p")], 120);

        assert!(out.servers[0].exposes("query"));
        assert!(!out.servers[0].exposes("drop_table"));
    }

    /// An empty allow-list means nothing, which is different from declaring
    /// no filter at all.
    #[test]
    fn an_empty_allow_list_exposes_nothing() {
        let mut entry = stdio("sqlx");
        entry.overrides.enabled_tools = Some(vec![]);
        let out = resolve_all(vec![(&entry, "p")], 120);

        assert!(!out.servers[0].exposes("query"));
    }

    #[test]
    fn without_filters_every_tool_is_exposed() {
        let entry = stdio("sqlx");
        let out = resolve_all(vec![(&entry, "p")], 120);
        assert!(out.servers[0].exposes("anything"));
    }

    /// Order must not depend on registry iteration, or the inventory shown
    /// to the model would shift between sessions.
    #[test]
    fn servers_are_ordered_by_name() {
        let b = stdio("b-server");
        let a = stdio("a-server");
        let out = resolve_all(vec![(&b, "p"), (&a, "p")], 120);

        let names: Vec<&str> = out.servers.iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["a-server", "b-server"]);
    }
}
