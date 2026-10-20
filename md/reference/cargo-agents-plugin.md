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

### `cargo agents plugin compile`

```bash
cargo agents plugin compile <PATH> --out <DIR>
```

Compile a plugin into an [Agent Plugins](https://agent-plugins.org/) directory that agents load on their own, without Symposium: `<DIR>/<name>/` with `plugin.json`, the same manifest in `.claude-plugin/plugin.json` for Claude Code, and its skills under `skills/`. `<PATH>` is a plugin directory (with a `Symposium.toml`) or a skill directory (with a `SKILL.md`). The plugin is validated first, and its skills are compiled as if every `depends-on` held. Only skills are compiled: hooks, MCP servers, subcommands and `[[plugins]]` references are left out, with a warning. `<name>` is the plugin name in lowercase letters, digits and hyphens.

The command prints how to load the result in each agent, for example `claude --plugin-dir <DIR>/<name>` for Claude Code. Running it again replaces its own earlier output; a directory at `<DIR>/<name>` that it did not write is left alone and the command fails. Nothing else under `<DIR>` is touched.

| Flag | Description |
|------|-------------|
| `<PATH>` | Path to the plugin or skill directory |
| `--out <DIR>` | Directory to write the compiled plugin into |
