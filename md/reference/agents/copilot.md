# GitHub Copilot

Config name: `copilot`

## Skills

| Scope | Path |
|-------|------|
| Project | `.agents/skills/<name>/SKILL.md` |
| Global | *(none)* |

Copilot has no global skills path.

## Hooks

Symposium creates a `symposium.json` file in the project hooks directory, and merges entries into the `hooks` key of the global settings.

| Scope | File |
|-------|------|
| Project | `.github/hooks/symposium.json` |
| Global | `~/.copilot/settings.json` |

Events registered: `preToolUse`, `postToolUse`, `userPromptSubmitted`, `sessionStart` (camelCase).

Output format: JSON. Uses `"bash"` key instead of `"command"` for platform-specific dispatch. Any non-zero exit code denies (not just exit 2).

**Caveat:** Copilot loads a project's hooks only once you trust the folder (it asks the first time you open it).

**Caveat:** Copilot also runs the hooks in `.claude/settings.json`. In a project where Claude Code is configured too, each Copilot event runs `cargo agents hook` twice, once as each agent.

## MCP servers

| Scope | File | Key |
|-------|------|-----|
| Global | `~/.copilot/mcp-config.json` | `mcpServers.<name>` |

Copilot reads no project-level MCP file, so a project-scoped registration goes to the global file.
