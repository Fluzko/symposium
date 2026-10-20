# `cargo agents plugin`

Manage plugin sources.

## Usage

```bash
cargo agents plugin <SUBCOMMAND>
```

## Subcommands

### `cargo agents plugin sync`

```bash
cargo agents plugin sync [PROVIDER]
```

Fetch or update git-based plugin sources. If a provider name is given, syncs only that provider (ignoring `auto-update` settings). If omitted, syncs all providers that have `auto-update = true`.

### `cargo agents plugin list`

```bash
cargo agents plugin list
```

List all configured plugin sources and the plugins they provide.

### `cargo agents plugin show`

```bash
cargo agents plugin show <PLUGIN>
```

Show a plugin: its id and its effective configuration as TOML, after validation has filled in defaults and promoted inline installations.

### `cargo agents plugin validate`

```bash
cargo agents plugin validate <PATH> [--no-check-crates]
```

Validate a plugin source directory or a single TOML manifest file. Useful when authoring plugins.

| Flag | Description |
|------|-------------|
| `<PATH>` | Path to a directory or a single `.toml` file |
| `--no-check-crates` | Skip checking that crate names in predicates exist on crates.io |

### `cargo agents plugin try`

```bash
cargo agents plugin try <PATH>
cargo agents plugin try --remove <PATH or NAME>
```

Install a plugin you are writing in your agents, the way a user would have it, so you can open an agent and use it. `<PATH>` is a plugin directory (with a `Symposium.toml`) or a skill directory (with a `SKILL.md`).

The plugin is linked into `~/.symposium/plugins/` under its name and enabled in every workspace, then the current workspace is synced. Symposium reads your directory in place, so an edit reaches your agents on the next sync: `cargo agents sync`, or the automatic one when an agent starts a session. Most agents then need a new session (Claude Code also takes `/reload-plugins`, Copilot CLI `/restart`).

A plugin with its own `depends-on` or predicates still follows them: it is active only in workspaces where they hold.

After syncing, `try` asks each configured agent what it loaded, using the agent's own listing command (none of them calls a model), and reports the plugin's skills it does not list:

| Agent | Command |
|-------|---------|
| Codex CLI | `codex debug prompt-input hi` |
| GitHub Copilot | `copilot skill list --json` |
| Goose | `goose skills list` |
| OpenCode | `opencode debug skill` |

Claude Code, Antigravity CLI and Kiro have no such command: start a new session in the project and run `/skills`. An agent whose command is not on `PATH` is skipped.

`--remove` discards the trial: it removes the link and the enablement, and syncs, which removes everything installed for the plugin in the current workspace. Other workspaces are cleaned on their next sync. A directory in `~/.symposium/plugins/` that `try` did not link is left alone.

| Flag | Description |
|------|-------------|
| `<PATH>` | Path to the plugin or skill directory; with `--remove`, its path or the plugin name |
| `--remove` | Discard the trial |
