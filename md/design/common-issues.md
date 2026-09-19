# Common issues

## Known hook implementation gaps

The following issues were identified by auditing our hook implementations against the agent reference docs (`md/design/agent-details/`). They don't cause crashes (the fallback path handles events without agent-specific handlers) but mean some features are incomplete.

### `permissionDecision` dropped (Copilot)

`CopilotPreToolUseOutput::from_hook_output()` never maps `permissionDecision` or `permissionDecisionReason` from the builtin hook output. If a builtin handler wants to deny a tool call, the decision is silently lost in Copilot output.

### Gemini `SessionStart` matcher

`ensure_gemini_hook_entry` uses `"matcher": ".*"` for all events including `SessionStart`. Per the Gemini reference, lifecycle events use exact-string matchers, not regex. Likely harmless in practice since `".*"` matches anything.

## Windows portability (tests)

The test suite runs on `windows-latest`, where CI passes `--no-fail-fast` so that one failing test binary does not hide the failures in every binary cargo would otherwise skip. A few patterns recur when writing tests that touch paths or scripts:

- **Paths in TOML/JSON string literals.** A Windows path like `C:\Users\...` is invalid inside a TOML or JSON string (the backslashes read as escapes). When substituting a real path into fixture text, convert to forward slashes first; Windows accepts `/` in paths. See `setup_fixture` in `symposium-testlib`.
- **Paths inside `sh` script bodies.** On Windows `sh` is git-bash's MSYS shell, which reads `C:\a\b` as escapes plus an illegal `:`. Rewrite to the `/c/a/b` form and quote the value. See `sh_path` in `predicate.rs` tests.
- **`.sh` files must use `script`, not `executable`.** A shell script cannot be spawned directly as a process on Windows (no shebang support). In fixtures, reference it via `script = "..."` so it is run through `sh`, never `executable = "..."`.
- **Canonicalized paths carry a `\\?\` prefix.** `fs::canonicalize` on Windows returns an extended-length path that `cargo`'s output lacks. Canonicalize both sides before comparing.
- **Paths inside asserted messages.** A report event or error that embeds a path renders it with backslashes on Windows, so an assertion matching a `/`-separated substring fails there. Normalize with `replace('\\', "/")` before matching, the way `normalize_paths` does for snapshots.
- **Snapshot tests and home-abbreviated paths.** `display_path` (in `output.rs`) abbreviates `$HOME` to `~/`. On Windows the test temp dir lives under `$HOME`, so printed config paths come out home-relative, not absolute. `normalize_paths` (in `symposium-testlib`) replaces both the absolute and the `~/` form; a snapshot leaking a random `.tmpXXXX/` path means one form was missed. Do not `UPDATE_EXPECT` your way past it: that bakes the volatile temp path into the snapshot and it fails on the next run.

## Capturing report events in tests

A test that asserts on `ReportEvent`s installs a `ReportLayer` with `tracing::subscriber::set_default`, which is thread-local. tracing caches callsite interest and the global max level process-wide, recomputing both from whichever dispatchers are alive the first time a callsite is reached. Tests run in parallel, so a thread with no subscriber can reach a reporting callsite first and pin it to `Interest::never()` for every thread; the capturing test then drains zero events, which looks like the command reporting nothing. The failure is timing-dependent, so it shows up as a platform-specific flake rather than a reproducible failure.

`install_tracing_baseline` (in `symposium-testlib`) installs a permissive global default subscriber once per test process to keep that cache open, and a thread-local `set_default` still takes precedence over it. `setup_fixture` calls it, and so must any helper that installs its own report layer.
