//! Reaching backing servers from inside the sandbox.
//!
//! A script calls `await sqlx.query({...})`. That name is an object installed
//! by the host, whose methods are Rust closures. Each closure hands the call
//! to whoever is driving the sandbox and waits for the answer.
//!
//! The call crosses a runtime boundary. The engine runs on its own thread
//! with its own current-thread runtime, while backing servers are child
//! processes owned by the main runtime, and their I/O can only be polled
//! there. A channel is what keeps each on the runtime it belongs to.
//!
//! Namespaces are built through the object API rather than by generating
//! JavaScript, so server and tool names — which come from plugin manifests —
//! never reach a code position.

use rquickjs::function::{Async, Opt};
use rquickjs::{CatchResultExt, Ctx, Function, Object};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// A tool call made by a script, waiting on its result.
#[derive(Debug)]
pub struct ToolCall {
    /// Server name as declared by the plugin, not the sanitized spelling.
    pub server: String,
    /// Tool name as it goes on the wire.
    pub tool: String,
    /// The single argument the script passed, or null.
    pub args: Value,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// Channel a sandbox sends its tool calls to.
pub type CallSender = mpsc::UnboundedSender<ToolCall>;
/// The receiving half, serviced by whoever drives the sandbox.
pub type CallReceiver = mpsc::UnboundedReceiver<ToolCall>;

pub fn channel() -> (CallSender, CallReceiver) {
    mpsc::unbounded_channel()
}

/// One tool reachable on a namespace.
#[derive(Debug, Clone)]
pub struct Binding {
    /// Property name in JavaScript. A tool whose wire name is not an
    /// identifier is bound twice, under the quoted name and a sanitized one.
    pub key: String,
    /// Name to send to the backing server.
    pub wire_name: String,
}

/// One backing server as the script sees it.
#[derive(Debug, Clone)]
pub struct Namespace {
    /// Global the object is installed on.
    pub key: String,
    /// Server name to send with each call.
    pub server: String,
    pub bindings: Vec<Binding>,
}

/// Install one global object per namespace.
pub fn install<'js>(
    ctx: &Ctx<'js>,
    namespaces: &[Namespace],
    calls: &CallSender,
) -> Result<(), String> {
    for namespace in namespaces {
        let object = Object::new(ctx.clone())
            .catch(ctx)
            .map_err(|e| e.to_string())?;

        for binding in &namespace.bindings {
            let function = tool_function(ctx, &namespace.server, &binding.wire_name, calls)?;
            object
                .set(binding.key.as_str(), function)
                .catch(ctx)
                .map_err(|e| e.to_string())?;
        }

        ctx.globals()
            .set(namespace.key.as_str(), object)
            .catch(ctx)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn tool_function<'js>(
    ctx: &Ctx<'js>,
    server: &str,
    tool: &str,
    calls: &CallSender,
) -> Result<Function<'js>, String> {
    let server = server.to_string();
    let tool = tool.to_string();
    let calls = calls.clone();

    Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, args: Opt<rquickjs::Value<'js>>| {
            let server = server.clone();
            let tool = tool.clone();
            let calls = calls.clone();
            async move {
                let args = match args.0 {
                    Some(value) => to_json(&ctx, value)?,
                    None => Value::Null,
                };

                let (reply, answer) = oneshot::channel();
                calls
                    .send(ToolCall {
                        server: server.clone(),
                        tool: tool.clone(),
                        args,
                        reply,
                    })
                    .map_err(|_| throw(&ctx, "tool dispatch is no longer available"))?;

                // A dropped sender means the caller abandoned the script;
                // surfacing it as an exception lets the script's own error
                // handling run.
                let result = answer
                    .await
                    .map_err(|_| throw(&ctx, &format!("{server}.{tool} did not answer")))?;

                // A failing tool throws, so a script uses ordinary try/catch
                // rather than inspecting a result shape.
                let value = result.map_err(|message| throw(&ctx, &message))?;
                from_json(&ctx, &value)
            }
        }),
    )
    .catch(ctx)
    .map_err(|e| e.to_string())
}

fn throw(ctx: &Ctx<'_>, message: &str) -> rquickjs::Error {
    rquickjs::Exception::throw_message(ctx, message)
}

/// Convert a JavaScript value to JSON through the engine's own serializer.
fn to_json<'js>(ctx: &Ctx<'js>, value: rquickjs::Value<'js>) -> rquickjs::Result<Value> {
    if value.is_undefined() || value.is_null() {
        return Ok(Value::Null);
    }
    let Some(encoded) = ctx.json_stringify(value)? else {
        return Ok(Value::Null);
    };
    let text = encoded.to_string()?;
    serde_json::from_str(&text).map_err(|_| rquickjs::Error::Unknown)
}

/// Convert JSON back into a JavaScript value.
fn from_json<'js>(ctx: &Ctx<'js>, value: &Value) -> rquickjs::Result<rquickjs::Value<'js>> {
    ctx.json_parse(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::sandbox::{Limits, Sandbox};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn namespace(server: &str, tools: &[&str]) -> Namespace {
        Namespace {
            key: server.to_string(),
            server: server.to_string(),
            bindings: tools
                .iter()
                .map(|t| Binding {
                    key: t.to_string(),
                    wire_name: t.to_string(),
                })
                .collect(),
        }
    }

    /// Drive a script, answering every tool call with `responder`, and record
    /// what was asked.
    async fn run_with(
        source: &str,
        namespaces: Vec<Namespace>,
        responder: impl Fn(&ToolCall) -> Result<Value, String> + Send + 'static,
    ) -> (Result<Value, String>, Vec<(String, String, Value)>) {
        let (calls, mut receiver) = channel();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&seen);
        let pump = tokio::spawn(async move {
            while let Some(call) = receiver.recv().await {
                recorded.lock().unwrap().push((
                    call.server.clone(),
                    call.tool.clone(),
                    call.args.clone(),
                ));
                let answer = responder(&call);
                let _ = call.reply.send(answer);
            }
        });

        let sandbox = Sandbox::new(Limits {
            timeout: Duration::from_secs(5),
            ..Limits::default()
        });
        let outcome = match sandbox.run_script_with(source, &namespaces, calls).await {
            Ok(o) => o.value().map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };

        pump.await.unwrap();
        let asked = seen.lock().unwrap().clone();
        (outcome, asked)
    }

    #[tokio::test]
    async fn script_calls_a_tool_and_receives_its_result() {
        let (out, asked) = run_with(
            r#"await sqlx.query({ sql: "SELECT 1" })"#,
            vec![namespace("sqlx", &["query"])],
            |_| Ok(json!({"rows": [{"n": 1}]})),
        )
        .await;

        assert_eq!(out.unwrap(), json!({"rows": [{"n": 1}]}));
        assert_eq!(
            asked,
            vec![(
                "sqlx".to_string(),
                "query".to_string(),
                json!({"sql": "SELECT 1"})
            )]
        );
    }

    /// The point of running code rather than proxying one call at a time:
    /// several calls, and the filtering between them, happen without the
    /// intermediate results ever leaving the sandbox.
    #[tokio::test]
    async fn script_composes_several_calls() {
        let (out, asked) = run_with(
            r#"
            const all = await sqlx.query({ sql: "SELECT" });
            const keep = all.rows.filter(r => r.n > 1);
            const out = [];
            for (const row of keep) {
              out.push(await sqlx.explain({ n: row.n }));
            }
            return out;
            "#,
            vec![namespace("sqlx", &["query", "explain"])],
            |call| match call.tool.as_str() {
                "query" => Ok(json!({"rows": [{"n": 1}, {"n": 2}, {"n": 3}]})),
                _ => Ok(json!({"plan": call.args["n"]})),
            },
        )
        .await;

        assert_eq!(out.unwrap(), json!([{"plan": 2}, {"plan": 3}]));
        assert_eq!(asked.len(), 3, "one query and two explains");
    }

    #[tokio::test]
    async fn tool_failure_throws_into_the_script() {
        let (out, _) = run_with(
            r#"
            try {
              await sqlx.query({});
              return "no throw";
            } catch (e) {
              return "caught: " + e.message;
            }
            "#,
            vec![namespace("sqlx", &["query"])],
            |_| Err("table not found".to_string()),
        )
        .await;

        assert_eq!(out.unwrap(), json!("caught: table not found"));
    }

    /// An uncaught tool failure ends the script rather than resolving to a
    /// value the model might mistake for success.
    #[tokio::test]
    async fn uncaught_tool_failure_fails_the_script() {
        let (out, _) = run_with(
            r#"await sqlx.query({})"#,
            vec![namespace("sqlx", &["query"])],
            |_| Err("boom".to_string()),
        )
        .await;

        let err = out.unwrap_err();
        assert!(err.contains("boom"), "got: {err}");
    }

    #[tokio::test]
    async fn calling_without_arguments_sends_null() {
        let (out, asked) = run_with(
            "await clock.now()",
            vec![namespace("clock", &["now"])],
            |_| Ok(json!(123)),
        )
        .await;

        assert_eq!(out.unwrap(), json!(123));
        assert_eq!(asked[0].2, Value::Null);
    }

    /// A tool whose wire name is not a JavaScript identifier is reachable
    /// under both spellings, and both dispatch to the same wire name.
    #[tokio::test]
    async fn both_spellings_dispatch_to_the_wire_name() {
        let ns = Namespace {
            key: "sqlx".to_string(),
            server: "sqlx".to_string(),
            bindings: vec![
                Binding {
                    key: "migrate-status".to_string(),
                    wire_name: "migrate-status".to_string(),
                },
                Binding {
                    key: "migrate_status".to_string(),
                    wire_name: "migrate-status".to_string(),
                },
            ],
        };
        let (out, asked) = run_with(
            r#"
            const a = await sqlx["migrate-status"]();
            const b = await sqlx.migrate_status();
            return [a, b];
            "#,
            vec![ns],
            |_| Ok(json!("ok")),
        )
        .await;

        assert_eq!(out.unwrap(), json!(["ok", "ok"]));
        assert!(
            asked.iter().all(|(_, tool, _)| tool == "migrate-status"),
            "both spellings must send the wire name, got: {asked:?}"
        );
    }

    #[tokio::test]
    async fn several_servers_are_separate_namespaces() {
        let (out, asked) = run_with(
            "return [await a.ping(), await b.ping()];",
            vec![namespace("a", &["ping"]), namespace("b", &["ping"])],
            |call| Ok(json!(call.server)),
        )
        .await;

        assert_eq!(out.unwrap(), json!(["a", "b"]));
        assert_eq!(asked.len(), 2);
    }

    /// Nothing beyond the declared namespaces appears.
    #[tokio::test]
    async fn undeclared_servers_are_absent() {
        let (out, _) = run_with("typeof other", vec![namespace("sqlx", &["query"])], |_| {
            Ok(Value::Null)
        })
        .await;
        assert_eq!(out.unwrap(), json!("undefined"));
    }
}
