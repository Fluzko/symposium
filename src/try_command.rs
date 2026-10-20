use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use symposium_install::UpdateLevel;

use crate::config::{Symposium, UseEntry};
use crate::crate_sources::normalize_crate_name;
use crate::pm::PluginKind;
use crate::report::ReportEvent;

pub async fn try_plugin(
    sym: &mut Symposium,
    cwd: &Path,
    path: &Path,
    update: UpdateLevel,
) -> Result<()> {
    if !sym.config.defaults.user_plugins {
        bail!("`[defaults] user-plugins` is off, so a tried plugin would never load");
    }

    let dir = path
        .canonicalize()
        .with_context(|| format!("cannot read `{}`", path.display()))?;
    let unvalidated = crate::plugins::entry_plugin(&dir, Path::new(""), "user-plugins")
        .with_context(|| format!("no Symposium.toml or SKILL.md in `{}`", dir.display()))??;
    let plugin = crate::plugins::validate_plugin(unvalidated, PluginKind::Registry)
        .with_context(|| format!("run `cargo agents plugin validate` on `{}`", dir.display()))?;
    let name = plugin.manifest.name.clone();

    if !crate::skills::is_safe_dir_name(&name) {
        bail!("plugin name `{name}` cannot be used as a directory name");
    }
    if sym.config.plugins.is_disabled(&name) {
        bail!("`{name}` is disabled in `[plugins] disable`; remove that entry to try it");
    }

    let plugins_dir = user_plugins_dir(sym);
    let link = plugins_dir.join(&name);
    match slot(&link) {
        Slot::Free => {
            fs::create_dir_all(&plugins_dir)
                .with_context(|| format!("cannot create `{}`", plugins_dir.display()))?;
            symlink_dir(&dir, &link)
                .with_context(|| format!("cannot link `{}`", link.display()))?;
        }
        Slot::Trial(target) if target == dir => {}
        Slot::Trial(target) => bail!(
            "`{name}` is already being tried from `{}`; run `cargo agents plugin try --remove {name}` first",
            target.display()
        ),
        Slot::Taken => bail!(
            "`{}` already exists and is not a trial; move it to try `{name}`",
            link.display()
        ),
    }

    let entry = UseEntry::Global(name.clone());
    if !sym.config.plugins.used.contains(&entry) {
        sym.config.plugins.used.push(entry);
        sym.save_config().context("failed to write user config")?;
    }

    tracing::info!(
        report = %ReportEvent::PluginTried { name: name.clone() },
    );
    if !plugin.manifest.requires_use {
        tracing::info!(
            report = %ReportEvent::Info {
                message: format!(
                    "`{name}` has its own activation conditions and applies only where they hold; see `cargo agents status`"
                ),
            },
        );
    }

    sync_here(sym, cwd, update).await
}

pub async fn remove_trial(
    sym: &mut Symposium,
    cwd: &Path,
    target: &Path,
    update: UpdateLevel,
) -> Result<()> {
    let plugins_dir = user_plugins_dir(sym);
    let name = trial_name(&plugins_dir, target)?;
    let link = plugins_dir.join(&name);

    remove_symlink_dir(&link).with_context(|| format!("cannot remove `{}`", link.display()))?;
    let _ = fs::remove_dir(&plugins_dir);

    let normalized = normalize_crate_name(&name);
    let used = &mut sym.config.plugins.used;
    let before = used.len();
    used.retain(
        |entry| !matches!(entry, UseEntry::Global(n) if normalize_crate_name(n) == normalized),
    );
    if used.len() != before {
        sym.save_config().context("failed to write user config")?;
    }

    tracing::info!(report = %ReportEvent::PluginTrialRemoved { name });

    sync_here(sym, cwd, update).await
}

async fn sync_here(sym: &Symposium, cwd: &Path, update: UpdateLevel) -> Result<()> {
    let deps = sym.workspace_deps(cwd);
    if deps.load().is_some() {
        return crate::sync::sync(sym, &deps, update).await;
    }
    tracing::info!(
        report = %ReportEvent::Info {
            message: "not in a Rust workspace; agents pick the change up when a session starts in one".to_string(),
        },
    );
    Ok(())
}

fn user_plugins_dir(sym: &Symposium) -> PathBuf {
    sym.config_dir().join("plugins")
}

fn trial_name(plugins_dir: &Path, target: &Path) -> Result<String> {
    if let Ok(dir) = target.canonicalize()
        && dir.is_dir()
    {
        let entries = fs::read_dir(plugins_dir).into_iter().flatten().flatten();
        for entry in entries {
            if matches!(slot(&entry.path()), Slot::Trial(t) if t == dir) {
                return Ok(entry.file_name().to_string_lossy().into_owned());
            }
        }
        bail!("`{}` is not being tried", target.display());
    }

    let Some(name) = target.file_name().and_then(|n| n.to_str()) else {
        bail!("`{}` is not being tried", target.display());
    };
    match slot(&plugins_dir.join(name)) {
        Slot::Trial(_) => Ok(name.to_string()),
        Slot::Free => bail!("`{name}` is not being tried"),
        Slot::Taken => bail!("`{name}` is a user plugin, not a trial; it was left alone"),
    }
}

enum Slot {
    Free,
    Trial(PathBuf),
    Taken,
}

fn slot(link: &Path) -> Slot {
    match fs::symlink_metadata(link) {
        Err(_) => Slot::Free,
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::read_link(link).map_or(Slot::Taken, Slot::Trial)
        }
        Ok(_) => Slot::Taken,
    }
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(unix)]
fn remove_symlink_dir(link: &Path) -> std::io::Result<()> {
    fs::remove_file(link)
}

#[cfg(windows)]
fn remove_symlink_dir(link: &Path) -> std::io::Result<()> {
    fs::remove_dir(link)
}
