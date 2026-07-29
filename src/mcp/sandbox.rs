//! The JavaScript sandbox scripts run in.
//!
//! A script is model-written and untrusted in the sense that matters here: it
//! may loop forever, allocate without bound, or recurse until the stack gives
//! out. None of those may take the agent's session with them.
//!
//! Two properties do the work:
//!
//! * **Deny by construction.** A bare QuickJS context has no filesystem,
//!   network, process or module loader — not because they are switched off,
//!   but because nothing registers them. There is no allowlist to get wrong.
//! * **Two layers of deadline.** An interrupt handler stops a script spinning
//!   in the interpreter; an outer timeout catches one blocked awaiting a host
//!   call, where the interpreter is not running and the interrupt cannot fire.
//!   Neither alone is sufficient.
//!
//! The interrupt raises an exception the script cannot catch, so
//! `try { while(true){} } catch(e) {}` still terminates.

use std::time::{Duration, Instant};

use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Ctx};
use serde::Serialize;
use serde_json::Value;

use super::normalize;

/// Extra room beyond the JavaScript stack limit for the interpreter's own
/// frames. Without it a script that hits the JS limit would instead overflow
/// the OS thread stack, which is a crash rather than an exception.
const STACK_HEADROOM: usize = 1 << 20;

/// How long the outer deadline waits past the interrupt deadline.
///
/// The interrupt produces a precise error, so give it a moment to win before
/// the blunt outer timeout fires.
const OUTER_GRACE: Duration = Duration::from_millis(250);

/// Bounds on one script execution.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_bytes: usize,
    pub stack_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            memory_bytes: 64 << 20,
            stack_bytes: 1 << 20,
        }
    }
}

/// Why a script produced no value.
///
/// Serializes to a tagged object so the model can tell a limit it exceeded
/// from a mistake in its own code, and retry accordingly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum SandboxError {
    ScriptTimeout { limit_secs: u64 },
    MemoryExhausted { limit_mb: u64 },
    ScriptError { message: String },
    Internal { message: String },
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ScriptTimeout { limit_secs } => {
                write!(f, "script exceeded its {limit_secs}s deadline")
            }
            Self::MemoryExhausted { limit_mb } => {
                write!(f, "script exceeded its {limit_mb}MB memory limit")
            }
            Self::ScriptError { message } | Self::Internal { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Runs one script under [`Limits`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Sandbox {
    limits: Limits,
}

impl Sandbox {
    pub fn new(limits: Limits) -> Self {
        Self { limits }
    }

    /// Evaluate `script` and return its value as JSON.
    ///
    /// The engine runs on its own thread: QuickJS is not `Send`, and the
    /// thread lets the stack be sized against the configured limit. A fresh
    /// runtime per call means no state survives from one script to the next.
    pub async fn eval(&self, script: &str) -> Result<Value, SandboxError> {
        self.eval_inner(script, None).await
    }

    /// Run a model-written script.
    ///
    /// The source is handed to the engine as a value rather than spliced into
    /// program text.
    pub async fn run_script(&self, source: &str) -> Result<Value, SandboxError> {
        self.eval_inner(normalize::PROGRAM, Some(&normalize::prepare(source)))
            .await
    }

    async fn eval_inner(&self, script: &str, source: Option<&str>) -> Result<Value, SandboxError> {
        let limits = self.limits;
        let script = script.to_string();
        let source = source.map(str::to_string);
        let (tx, rx) = tokio::sync::oneshot::channel();

        let spawned = std::thread::Builder::new()
            .name("mcp-sandbox".to_string())
            .stack_size(limits.stack_bytes + STACK_HEADROOM)
            .spawn(move || {
                let outcome = match tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                {
                    Ok(rt) => rt.block_on(run(&script, source.as_deref(), limits)),
                    Err(e) => Err(SandboxError::Internal {
                        message: format!("could not start sandbox runtime: {e}"),
                    }),
                };
                // A closed receiver means the outer deadline already fired.
                let _ = tx.send(outcome);
            });

        if let Err(e) = spawned {
            return Err(SandboxError::Internal {
                message: format!("could not start sandbox thread: {e}"),
            });
        }

        // The outer layer. A script awaiting a host call that never resolves
        // leaves the interpreter idle, so the interrupt handler never runs and
        // only this can end it.
        match tokio::time::timeout(limits.timeout + OUTER_GRACE, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(SandboxError::Internal {
                message: "sandbox thread ended without reporting".to_string(),
            }),
            Err(_) => Err(SandboxError::ScriptTimeout {
                limit_secs: limits.timeout.as_secs(),
            }),
        }
    }
}

async fn run(script: &str, source: Option<&str>, limits: Limits) -> Result<Value, SandboxError> {
    let runtime = AsyncRuntime::new().map_err(internal)?;
    runtime.set_memory_limit(limits.memory_bytes).await;
    runtime.set_max_stack_size(limits.stack_bytes).await;

    // The inner layer. Fires while the interpreter is running, and raises an
    // exception the script cannot catch.
    let deadline = Instant::now() + limits.timeout;
    runtime
        .set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
        .await;

    let context = AsyncContext::full(&runtime).await.map_err(internal)?;
    let outcome =
        AsyncContext::async_with(&context, async |ctx| evaluate(ctx, script, source).await).await;

    // Settle any promises the script left pending before deciding the result.
    runtime.idle().await;

    let Err(message) = outcome else {
        return outcome.map_err(|e| classify(e, deadline, limits, 0));
    };
    let allocated = runtime.memory_usage().await.malloc_size.max(0) as usize;
    Err(classify(message, deadline, limits, allocated))
}

async fn evaluate<'js>(ctx: Ctx<'js>, script: &str, source: Option<&str>) -> Result<Value, String> {
    if let Some(source) = source {
        ctx.globals()
            .set(normalize::SOURCE_GLOBAL, source)
            .catch(&ctx)
            .map_err(|e| e.to_string())?;
    }

    // Evaluated as a plain script rather than a module: scripts arrive
    // normalized into a self-calling async function, so the value is already a
    // promise and top-level `await` is never needed.
    let value: rquickjs::Value<'js> = ctx
        .eval(script.as_bytes().to_vec())
        .catch(&ctx)
        .map_err(|e| e.to_string())?;

    // A script is normally an async function call, so the value is a promise.
    let resolved = match value.as_promise() {
        Some(promise) => promise
            .clone()
            .into_future::<rquickjs::Value>()
            .await
            .catch(&ctx)
            .map_err(|e| e.to_string())?,
        None => value,
    };

    to_json(&ctx, resolved)
}

/// Convert a JavaScript value to JSON via the engine's own serializer, so
/// `toJSON` and nested structures behave as the script author expects.
fn to_json<'js>(ctx: &Ctx<'js>, value: rquickjs::Value<'js>) -> Result<Value, String> {
    if value.is_undefined() {
        return Ok(Value::Null);
    }
    let encoded = ctx
        .json_stringify(value)
        .catch(ctx)
        .map_err(|e| e.to_string())?;
    let Some(encoded) = encoded else {
        // `JSON.stringify` yields nothing for values with no JSON form.
        return Ok(Value::Null);
    };
    let text = encoded.to_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("result was not valid JSON: {e}"))
}

/// Decide which limit, if any, a failure represents.
///
/// QuickJS surfaces both an interrupt and an allocation failure as ordinary
/// exceptions, so the cause has to be recovered from the surrounding state
/// rather than the message. An exhausted heap in particular throws a null
/// value, because there is no memory left to build an error object with.
fn classify(message: String, deadline: Instant, limits: Limits, allocated: usize) -> SandboxError {
    if Instant::now() >= deadline {
        return SandboxError::ScriptTimeout {
            limit_secs: limits.timeout.as_secs(),
        };
    }
    // Still holding most of the budget when the exception surfaced.
    if allocated * 10 >= limits.memory_bytes * 8 {
        return SandboxError::MemoryExhausted {
            limit_mb: (limits.memory_bytes / (1 << 20)) as u64,
        };
    }
    SandboxError::ScriptError { message }
}

fn internal(e: impl std::fmt::Display) -> SandboxError {
    SandboxError::Internal {
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fast() -> Limits {
        Limits {
            timeout: Duration::from_millis(300),
            ..Limits::default()
        }
    }

    async fn eval(script: &str) -> Result<Value, SandboxError> {
        Sandbox::new(fast()).eval(script).await
    }

    #[tokio::test]
    async fn evaluates_an_expression() {
        assert_eq!(eval("1 + 1").await.unwrap(), json!(2));
    }

    #[tokio::test]
    async fn returns_structured_values() {
        let out = eval(r#"({ rows: [{ id: 1 }], ok: true })"#).await.unwrap();
        assert_eq!(out, json!({"rows": [{"id": 1}], "ok": true}));
    }

    #[tokio::test]
    async fn awaits_a_returned_promise() {
        let out = eval("(async () => 42)()").await.unwrap();
        assert_eq!(out, json!(42));
    }

    #[tokio::test]
    async fn undefined_becomes_null() {
        assert_eq!(eval("undefined").await.unwrap(), Value::Null);
    }

    // -- deadlines --

    #[tokio::test]
    async fn cpu_bound_loop_hits_the_deadline() {
        let err = eval("while (true) {}").await.unwrap_err();
        assert_eq!(err, SandboxError::ScriptTimeout { limit_secs: 0 });
    }

    /// The interrupt raises an exception the script cannot catch. Were it
    /// catchable, a model could sit inside `try`/`catch` and never yield.
    #[tokio::test]
    async fn script_cannot_catch_the_deadline() {
        let err =
            eval(r#"(() => { try { while (true) {} } catch (e) { return "swallowed"; } })()"#)
                .await
                .unwrap_err();
        assert!(
            matches!(err, SandboxError::ScriptTimeout { .. }),
            "deadline must not be catchable, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn deadline_is_enforced_promptly() {
        let started = Instant::now();
        let _ = eval("while (true) {}").await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
    }

    // -- memory --

    /// Guards against the limit silently becoming a no-op: rquickjs documents
    /// `set_memory_limit` as inert when a custom allocator is in use, which a
    /// future feature change could enable without any other visible effect.
    #[tokio::test]
    async fn allocation_is_bounded() {
        let sandbox = Sandbox::new(Limits {
            memory_bytes: 1 << 20,
            ..fast()
        });
        let err = sandbox
            .eval("const a = []; while (true) { a.push(new Array(10000)); }")
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SandboxError::MemoryExhausted { .. } | SandboxError::ScriptTimeout { .. }
            ),
            "unbounded allocation must not succeed, got: {err:?}"
        );
    }

    /// Deep recursion must raise a JavaScript exception rather than overflow
    /// the thread's own stack, which would abort the process.
    #[tokio::test]
    async fn recursion_is_bounded() {
        let err = eval("(function f() { return f(); })()").await.unwrap_err();
        assert!(
            matches!(err, SandboxError::ScriptError { .. }),
            "got: {err:?}"
        );
    }

    // -- errors --

    #[tokio::test]
    async fn script_errors_carry_their_message() {
        let err = eval(r#"throw new Error("boom")"#).await.unwrap_err();
        let SandboxError::ScriptError { message } = &err else {
            panic!("expected a script error, got: {err:?}");
        };
        assert!(message.contains("boom"), "got: {message}");
    }

    #[tokio::test]
    async fn syntax_errors_are_reported() {
        let err = eval("this is not javascript").await.unwrap_err();
        assert!(
            matches!(err, SandboxError::ScriptError { .. }),
            "got: {err:?}"
        );
    }

    /// The model is told what it hit, in a form it can act on.
    #[tokio::test]
    async fn errors_serialize_as_tagged_objects() {
        let err = SandboxError::ScriptTimeout { limit_secs: 120 };
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"error": "script_timeout", "limit_secs": 120})
        );
    }

    // -- ambient capabilities --

    /// Nothing registers these; the assertion guards against a dependency
    /// bump quietly introducing one.
    #[tokio::test]
    async fn host_capabilities_are_absent() {
        for global in [
            "fetch",
            "require",
            "process",
            "XMLHttpRequest",
            "WebSocket",
            "globalThis.os",
            "globalThis.std",
        ] {
            let out = eval(&format!("typeof {global}")).await.unwrap();
            assert_eq!(out, json!("undefined"), "{global} should not exist");
        }
    }

    #[tokio::test]
    async fn modules_cannot_be_imported() {
        let err = eval(r#"import("fs")"#).await.unwrap_err();
        assert!(
            matches!(err, SandboxError::ScriptError { .. }),
            "got: {err:?}"
        );
    }

    /// Each call gets a fresh runtime, so nothing a script leaves behind can
    /// influence the next one.
    #[tokio::test]
    async fn state_does_not_leak_between_scripts() {
        let sandbox = Sandbox::new(fast());
        sandbox.eval("globalThis.leaked = 1").await.unwrap();
        let out = sandbox.eval("typeof globalThis.leaked").await.unwrap();
        assert_eq!(out, json!("undefined"));
    }
}
