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

Validate a plugin source directory, an [Agent Plugins package](./agent-plugins.md) folder, or a single TOML manifest file. Useful when authoring plugins. Each item is reported with its kind: `plugin`, `agent plugin` or `skill`.

| Flag | Description |
|------|-------------|
| `<PATH>` | Path to a directory, a package's `plugin.json`, or a single `.toml` file |
| `--no-check-crates` | Skip checking that crate names in predicates exist on crates.io |

Besides errors, it warns about a plugin that loads but does not work the same on every agent:

- a plugin name containing `.`, which Antigravity rejects;
- an Agent Plugins package with no `version`, which agents that cache plugins by version, such as Codex, may not refresh;
- a hook event handled only by agent-format hooks, which fire on that agent alone (see [Agent-specific hooks](../crate-authors/authoring-a-plugin.md#agent-specific-hooks));
- a skill file or folder that is not installed: a link that resolves outside its package, a broken link, or a link back into the skill.
