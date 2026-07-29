//! `cargo agents mcp-serve` spoken to as a real MCP client over stdio.
//!
//! The unit tests cover what the server decides; these cover that a client
//! can actually talk to it — process spawn, handshake, framing, and the
//! stdout discipline the transport depends on.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-agents"))
}

/// Point the binary at an empty config directory, so a developer's own
/// settings cannot change what a test sees.
fn isolated_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

async fn connect(home: &tempfile::TempDir) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let mut command = tokio::process::Command::new(binary());
    command
        .arg("mcp-serve")
        .env("SYMPOSIUM_HOME", home.path())
        .stderr(Stdio::null());

    let transport = TokioChildProcess::new(command).expect("spawn mcp-serve");
    tokio::time::timeout(Duration::from_secs(30), ().serve(transport))
        .await
        .expect("handshake timed out")
        .expect("handshake failed")
}

/// The point of the design: an agent sees two tools, not every plugin
/// server's tools.
#[tokio::test(flavor = "multi_thread")]
async fn advertises_two_tools() {
    let home = isolated_home();
    let client = connect(&home).await;

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .expect("tools/list")
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();

    assert_eq!(names, vec!["execute".to_string(), "list_tools".to_string()]);
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_itself_as_symposium() {
    let home = isolated_home();
    let client = connect(&home).await;

    let info = client.peer_info().expect("server info");
    let server_info = info.server_info.as_ref().expect("server implementation");
    assert_eq!(server_info.name, "symposium");
    assert!(
        info.capabilities.tools.is_some(),
        "tools capability must be advertised"
    );
    let _ = client.cancel().await;
}

/// A client filtering on read-only annotations must not mistake arbitrary
/// code execution for a safe operation.
#[tokio::test(flavor = "multi_thread")]
async fn execute_is_annotated_as_destructive() {
    let home = isolated_home();
    let client = connect(&home).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    let execute = tools
        .iter()
        .find(|t| t.name.as_ref() == "execute")
        .expect("execute tool");
    let annotations = execute.annotations.as_ref().expect("annotations");

    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(true));
    let _ = client.cancel().await;
}

/// With nothing applicable, the tools still answer — and say so, rather than
/// failing in a way that reads as a broken connection.
#[tokio::test(flavor = "multi_thread")]
async fn list_tools_answers_when_nothing_applies() {
    let home = isolated_home();
    let client = connect(&home).await;

    let result = client
        .call_tool(CallToolRequestParams::new("list_tools"))
        .await
        .expect("list_tools should answer");

    assert_ne!(result.is_error, Some(true), "answering is not an error");
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_tool_names_the_available_ones() {
    let home = isolated_home();
    let client = connect(&home).await;

    let err = client
        .call_tool(CallToolRequestParams::new("nonexistent"))
        .await
        .expect_err("an unknown tool is a protocol error");
    let text = err.to_string();

    assert!(
        text.contains("list_tools") && text.contains("execute"),
        "the error should name what is available, got: {text}"
    );
    let _ = client.cancel().await;
}

/// The transport is newline-delimited JSON, so anything else written to
/// stdout corrupts the stream. Reporting output is the likely offender, since
/// every other subcommand sends it there.
#[tokio::test(flavor = "multi_thread")]
async fn stdout_carries_only_json_rpc() {
    let home = isolated_home();

    let mut child = tokio::process::Command::new(binary())
        .arg("mcp-serve")
        // Verbose reporting would go to stdout for any other subcommand.
        .arg("--verbose")
        .env("SYMPOSIUM_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");

    use tokio::io::AsyncWriteExt;
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                "\n",
            )
            .as_bytes(),
        )
        .await
        .expect("write");
    drop(stdin);

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("server should exit on stdin close")
        .expect("output");

    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();

    assert_eq!(lines.len(), 2, "one response per request, got: {stdout}");
    for line in lines {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
    }
}
