# Configuration loading

## Directory resolution

User-wide paths are resolved using the [`directories`](https://crates.io/crates/directories) crate, which handles XDG Base Directory conventions automatically. If XDG environment variables are set, they are respected; otherwise paths fall back to `~/.symposium/`.

See the [configuration reference](../reference/configuration.md#directory-resolution) for the full resolution table.

## Config loading

The user config (`~/.symposium/config.toml`) is loaded once at startup into the `Symposium` struct. The file is deserialized into `RawConfig`, then validated into the runtime `Config` used by the rest of the code. If the file is missing or empty, defaults are used.

If the file exists but cannot be read or parsed, a warning is printed and the defaults are used for that run only, with `auto-update` forced off. The error is kept on `Symposium` (`config_error`), and while it is set `save_config` refuses to write: a command that would save the config (`init`, `use`, `telemetry enable|disable`, the consent prompt in `sync`) fails with an error naming the file and leaves it unchanged.

A file Symposium could not read says nothing about what the user chose, so nothing decided from the defaults may outlive the run. Auto-update is forced off because it is the one default that acts outside Symposium's own files; the rest apply as usual. The failure is not fatal, since hooks run on every agent event and must keep working.
