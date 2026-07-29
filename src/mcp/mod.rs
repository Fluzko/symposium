//! The MCP meta-server.
//!
//! Symposium registers a single MCP server with each configured agent. That
//! server exposes the tools of every applicable plugin MCP server, instead of
//! each plugin being registered separately.
//!
//! See the [MCP meta-server RFD](../../md/rfds/mcp-meta-server/README.md).

pub mod declarations;
pub mod schema_to_ts;

#[cfg(test)]
mod corpus_tests;
