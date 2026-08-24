# Running tests

## Quick start

```bash
cargo test              # simulation + configured agents
```

By default, `cargo test` runs simulation tests and then re-runs agent-mode tests against each agent listed in `test-agents.toml`. On a fresh clone (no file), the defaults are `claude-sdk` and `kiro-cli-acp`.

## Configuring test agents

Create `test-agents.toml` in the repo root (gitignored):

```toml
# Run against these agents. Use `acpr --list` to see ACP registry agents.
test-agents = ["claude-sdk"]
```

Set to `[]` to skip agent tests entirely (used in CI):

```toml
test-agents = []
```

Available agent names:

| Name | Backend | Notes |
|------|---------|-------|
| `claude-sdk` | Claude Agent SDK (Python) | Requires `uv` + `ANTHROPIC_API_KEY` |
| `kiro-cli-acp` | Kiro CLI via ACP | Requires `kiro-cli` in PATH |
| Any name from `acpr --list` | ACP registry via `acpr` | Auto-downloaded |

## Filtering to a single agent

Override with the `SYMPOSIUM_TEST_AGENT` env var:

```bash
SYMPOSIUM_TEST_AGENT=kiro-cli-acp cargo test --test hook_agent
```

This ignores `test-agents.toml` and runs only the specified agent.

## Running specific test files

```bash
cargo test --test hook_agent       # just the agent integration tests
cargo test --test init_sync        # just the init/sync tests
cargo test --test dispatch         # just the CLI dispatch tests
```

## Agent contract tests

`tests/agent_plugin_contract.rs` answers one question the rest of the suite cannot: does the agent
*actually load and use* what symposium installed? Asserting on the bytes we write is not enough — a
Copilot install once passed a unit test that reproduced its own CLI's output exactly, and Copilot
still ignored the plugin, because the missing piece was a record Copilot keeps for itself.

These are **opt-in**, because they write into your real agent configuration:

```bash
SYMPOSIUM_AGENT_CONTRACT=1 cargo test --test agent_plugin_contract -- --test-threads=1
```

Real configuration is the only place a real agent reads. Redirecting `HOME` would isolate it but also
cut the agent off from its credentials, which live under that same directory. So every file the tests
touch is snapshotted first and restored on the way out, including on panic, and every directory they
copy into is removed.

Both scopes are covered, because they take different paths: a global plugin is enabled in the user's
own settings and compiled under the symposium home, while a project-scoped one is enabled in the
*project's* settings, compiled under the project, and registered under a marketplace named for that
workspace. Claude Code is the only agent that can bound a plugin to a project; for the rest, the suite
checks that a project-scoped plugin still reaches them as individual skills.

Each agent runs the same three checks — install, update, remove — driven through that agent's own CLI:

| Check | Assertion |
|---|---|
| install | the agent invokes the skill and returns its token |
| update | after the source changes, the agent returns the *new* token |
| remove | once the plugin stops applying, the agent no longer has the skill |

The probe plugin is deliberately dormant, activated only by a `use` entry, so that removing that entry
genuinely deactivates it. A `depends-on = ["*"]` probe would stay active regardless and the removal
check would prove nothing.

An agent whose CLI is missing or cannot authenticate is **skipped with the reason printed**, never
failed: a missing login is not a symposium bug. A green run therefore does not by itself mean every
agent was covered — read the skips.

## Debugging test failures

Add `--nocapture` to see test output (agent messages, hook traces):

```bash
cargo test --test hook_agent -- --nocapture
```

On failure, the test's temporary directory is preserved and its path is printed to stderr so you can inspect the fixture state.

## Windows

CI runs the full test suite on `windows-latest` as part of the `test` matrix (see `.github/workflows/ci.yml`). To run the tests locally on Windows:

- Install Git for Windows and make sure `sh` is on `PATH`. Git ships it at `C:\Program Files\Git\usr\bin`. Several tests spawn `sh` to run script-based hooks and predicates, so a missing `sh` shows up as unrelated-looking hook failures.
- The repo's `.gitattributes` normalizes checked-out text files to LF. This keeps shebang'd fixtures and shell scripts runnable regardless of `core.autocrlf`.

The self-update tests run on Windows: `set_mock_cargo` runs the `#!/bin/sh` mock through `sh` via a one-line `.cmd` shim (production spawns the cargo override directly, so no production code changes). The two `auto_update_re_execs_*` tests are `#[ignore]`d on Windows (`#[cfg_attr(windows, ignore)]`): they overwrite the running binary with a shebang stand-in and re-exec into it, which needs Windows-native process replacement. They still compile on Windows, so they are skipped (not compiled out) and their helpers need no `#[cfg]`. Porting them to run on Windows is a tracked follow-up.
