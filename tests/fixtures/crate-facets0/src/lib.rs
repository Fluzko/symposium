// facet-host depends on crate-f; the vouch-f plugin loads crate-f's plugin
// through a `[[plugins]]` chained reference. crate-f's manifest declares
// non-skill extensions (an MCP server and a subcommand), which now dispatch
// through the active plugin set exactly like a registry plugin's.
