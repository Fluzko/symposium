//! Snapshots of generated declarations for real MCP servers.
//!
//! The payloads in `testdata/` are verbatim `tools/list` responses captured
//! from published servers, chosen to span four independent schema generators:
//! two flavours of the TypeScript `zod` toolchain (draft-07 and 2020-12),
//! plus the reference servers' own hand-written schemas.
//!
//! These are the tests that catch a dialect we handle wrongly. A construct
//! that starts rendering as `unknown` shows up as a diff in the checked-in
//! `.d.ts`, which the synthetic unit tests cannot detect because they only
//! cover shapes we already thought of.
//!
//! Regenerate with `UPDATE_EXPECT=1 cargo test`.

use expect_test::expect_file;
use serde_json::Value;

use super::declarations::{ToolDecl, render_server};

/// Render a captured `tools/list` payload as one server's declarations.
fn render_corpus(payload: &str, server: &str) -> String {
    let parsed: Value = serde_json::from_str(payload).expect("payload should be valid JSON");
    let tools = parsed["tools"]
        .as_array()
        .expect("payload should carry a `tools` array");

    let decls: Vec<ToolDecl> = tools
        .iter()
        .map(|tool| ToolDecl {
            name: tool["name"].as_str().unwrap_or_default(),
            description: tool["description"].as_str(),
            input_schema: tool.get("inputSchema"),
        })
        .collect();

    render_server(server, &decls)
}

/// Every corpus entry renders something for every tool, and nothing panics.
///
/// The per-server snapshots below cover the content; this covers the
/// invariant that generation never fails, whatever a server sends.
#[test]
fn every_tool_in_the_corpus_produces_a_declaration() {
    for (payload, server) in [
        (include_str!("testdata/everything.tools.json"), "everything"),
        (include_str!("testdata/filesystem.tools.json"), "filesystem"),
        (include_str!("testdata/memory.tools.json"), "memory"),
        (include_str!("testdata/playwright.tools.json"), "playwright"),
        (
            include_str!("testdata/sequentialthinking.tools.json"),
            "sequentialthinking",
        ),
    ] {
        let parsed: Value = serde_json::from_str(payload).unwrap();
        let expected = parsed["tools"].as_array().unwrap().len();
        let rendered = render_corpus(payload, server);
        let found = rendered.matches("): Promise<unknown>;").count();
        assert!(
            found >= expected,
            "{server}: expected at least {expected} declarations, found {found}"
        );
    }
}

/// The protocol's own test server. Names 17 of its 18 tools with hyphens, so
/// this is also the identifier-handling snapshot.
#[test]
fn everything_server() {
    expect_file!["testdata/everything.d.ts"].assert_eq(&render_corpus(
        include_str!("testdata/everything.tools.json"),
        "everything",
    ));
}

/// Carries the corpus's only union-with-literal and declares output schemas
/// on every tool.
#[test]
fn filesystem_server() {
    expect_file!["testdata/filesystem.d.ts"].assert_eq(&render_corpus(
        include_str!("testdata/filesystem.tools.json"),
        "filesystem",
    ));
}

/// Deeply nested schemas: the most schema nodes per byte in the corpus.
#[test]
fn memory_server() {
    expect_file!["testdata/memory.d.ts"].assert_eq(&render_corpus(
        include_str!("testdata/memory.tools.json"),
        "memory",
    ));
}

/// The 2020-12 dialect, and the corpus's only typed `additionalProperties`
/// and `propertyNames`.
#[test]
fn playwright_server() {
    expect_file!["testdata/playwright.d.ts"].assert_eq(&render_corpus(
        include_str!("testdata/playwright.tools.json"),
        "playwright",
    ));
}

/// One tool with a very large description — the size-control case, where the
/// payload is dominated by prose rather than by schema.
#[test]
fn sequentialthinking_server() {
    expect_file!["testdata/sequentialthinking.d.ts"].assert_eq(&render_corpus(
        include_str!("testdata/sequentialthinking.tools.json"),
        "sequentialthinking",
    ));
}
