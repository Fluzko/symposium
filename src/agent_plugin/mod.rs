//! Compiling a gated symposium plugin into an agent plugin directory.
//!
//! The directory is the unit agents themselves use: a manifest beside a
//! `skills/` folder. Compilation happens after every predicate has been
//! evaluated, so what lands on disk is only what applies — an agent never
//! receives a gate and never resolves one.

pub mod manifest;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::PluginsConfig;
use crate::plugins::ParsedPlugin;
use crate::pm::{ANY_VERSION, CARGO_PM};
use crate::skills::SkillWithGroupContext;
use manifest::Manifest;

/// The directory under a project root that symposium owns outright, so one
/// `.gitignore` at its root covers everything below it.
pub const PROJECT_OWNED_DIR: &str = ".symposium";

/// Staging directory for compiled plugins within [`PROJECT_OWNED_DIR`].
pub const PROJECT_STAGING_SUBDIR: &str = "plugins";

/// Staging directory under the user configuration directory.
///
/// Deliberately not `plugins/`, which is the builtin `user-plugins` *registry* —
/// a directory symposium reads entries from. Compiling into it would make
/// symposium ingest its own output as registry plugins on the next load.
pub const GLOBAL_STAGING_DIR: &str = "installed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    Global,
}

impl Scope {
    /// Where a plugin's compiled directory belongs.
    ///
    /// Global installation requires the decision to be reproducible from user
    /// config alone: a user-level directory is visible from every workspace, and
    /// cleanup reaps whatever it did not install this run, so a global set that
    /// varied by workspace would have two projects undoing each other. Anything
    /// whose activation *or content* depends on this workspace is therefore
    /// project-scoped, even when a global `use` entry named it.
    ///
    /// Content matters as much as activation: a plugin gated `depends-on(*)`
    /// whose skill group is gated `depends-on(serde)` would compile to different
    /// directories in different projects, which is the same churn by another
    /// route. So every gate in the chain has to hold workspace-independently —
    /// the plugin's, each declared group's, and each contributed skill's.
    pub fn of(
        parsed: &ParsedPlugin,
        contributed: &[&SkillWithGroupContext],
        plugins: &PluginsConfig,
    ) -> Scope {
        let workspace_bound = parsed.workspace_member
            || parsed.canonical.pm == CARGO_PM
            || !parsed.plugin.predicates.is_workspace_independent()
            || (parsed.plugin.requires_use && !plugins.is_used_globally(&parsed.plugin.name))
            || parsed
                .plugin
                .skills
                .iter()
                .any(|group| !group.predicates.is_workspace_independent())
            || contributed
                .iter()
                .any(|entry| !entry.skill.predicates.is_workspace_independent());
        if workspace_bound {
            Scope::Project
        } else {
            Scope::Global
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledSkill {
    pub dir_name: String,
    /// Directory holding the skill's `SKILL.md`, copied verbatim.
    pub source_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledPlugin {
    pub dir_name: String,
    pub manifest: Manifest,
    pub scope: Scope,
    pub skills: Vec<CompiledSkill>,
}

/// Group already-gated skills into one compiled plugin per contributing plugin.
///
/// A plugin with no applicable skills compiles to nothing: version one carries
/// only the format's skills component, so such a directory would be empty.
pub fn compile(
    active: &[ParsedPlugin],
    skills: &[SkillWithGroupContext],
    plugins: &PluginsConfig,
) -> Vec<CompiledPlugin> {
    let mut compiled: Vec<(String, CompiledPlugin)> = Vec::new();

    for parsed in active {
        let mine: Vec<&SkillWithGroupContext> = skills
            .iter()
            .filter(|s| s.plugin_id == parsed.canonical)
            .collect();
        if mine.is_empty() {
            continue;
        }

        let Some(name) = manifest::slug(&parsed.plugin.name) else {
            tracing::info!(
                report = %crate::report::ReportEvent::Warning {
                    message: format!(
                        "cannot compile plugin `{}`: no valid agent plugin name",
                        parsed.plugin.name
                    ),
                },
            );
            continue;
        };

        compiled.push((
            crate::skills::hash_origin_key(&parsed.canonical.to_string()),
            CompiledPlugin {
                dir_name: name.clone(),
                manifest: Manifest::new(
                    name,
                    version_of(parsed),
                    parsed.plugin.description.clone(),
                ),
                scope: Scope::of(parsed, &mine, plugins),
                skills: compile_skills(&mine),
            },
        ));
    }

    disambiguate(compiled)
}

/// Two plugin names can slug to the same directory name, so whenever more than
/// one plugin claims a slug, every claimant takes the suffixed form. Suffixing
/// all of them rather than all-but-one keeps a name stable when an unrelated
/// plugin appears or disappears.
fn disambiguate(compiled: Vec<(String, CompiledPlugin)>) -> Vec<CompiledPlugin> {
    let mut claims: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (_, plugin) in &compiled {
        *claims.entry(plugin.dir_name.as_str()).or_default() += 1;
    }
    let contested: std::collections::BTreeSet<String> = claims
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(name, _)| name.to_string())
        .collect();

    compiled
        .into_iter()
        .map(|(hash, mut plugin)| {
            if contested.contains(&plugin.dir_name) {
                plugin.dir_name = format!("{}-{hash}", plugin.dir_name);
            }
            plugin
        })
        .collect()
}

/// One skill directory per distinct origin. Skills sharing a name within one
/// plugin take the origin-hash suffix; across plugins they do not collide,
/// because the agent namespaces a plugin's skills under the plugin.
fn compile_skills(skills: &[&SkillWithGroupContext]) -> Vec<CompiledSkill> {
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut name_counts: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    let mut distinct: Vec<&&SkillWithGroupContext> = Vec::new();

    for skill in skills {
        if seen.insert(&skill.origin_hash) {
            *name_counts.entry(skill.skill.name()).or_default() += 1;
            distinct.push(skill);
        }
    }

    distinct
        .into_iter()
        .filter_map(|entry| {
            let name = entry.skill.name();
            let source_dir = entry.skill.path.parent()?.to_path_buf();
            let dir_name = if name_counts.get(name).copied().unwrap_or(0) == 1 {
                name.to_string()
            } else {
                format!("{name}-{}", entry.origin_hash)
            };
            Some(CompiledSkill {
                dir_name,
                source_dir,
            })
        })
        .collect()
}

/// The manifest's version wins; otherwise a crate plugin's resolved version
/// stands in. A registry or workspace plugin has no real package identity, so
/// its placeholder `*` is not a version and is dropped.
fn version_of(parsed: &ParsedPlugin) -> Option<String> {
    parsed.plugin.version.clone().or_else(|| {
        (parsed.canonical.version != ANY_VERSION).then(|| parsed.canonical.version.clone())
    })
}

/// Write a compiled plugin into `root`, returning its directory.
///
/// The content is assembled in a temporary directory and then handed to the
/// ordinary managed-directory sync, so the install is change-aware and
/// debounced exactly like a skill directory: recompiling identical content
/// leaves the destination untouched.
pub fn write(
    compiled: &CompiledPlugin,
    root: &Path,
    boundary: &Path,
    debounce: Duration,
) -> Result<PathBuf> {
    let staged = tempfile::tempdir().context("create staging dir")?;
    fs::write(
        staged.path().join("plugin.json"),
        compiled.manifest.to_json(),
    )
    .context("write plugin.json")?;

    for skill in &compiled.skills {
        let dest = staged.path().join("skills").join(&skill.dir_name);
        fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
        crate::sync::copy_dir_recursive(&skill.source_dir, &dest)
            .with_context(|| format!("copy skill {}", skill.dir_name))?;
    }

    let dest = root.join(&compiled.dir_name);
    crate::sync::sync_managed_dir(
        staged.path(),
        &dest,
        boundary,
        debounce,
        crate::sync::Marking::MarkerOnly,
    )?;
    Ok(dest)
}

/// Reap compiled directories under `root` that this sync did not write. Keyed on
/// the ownership marker, so a directory the user put there is left alone.
pub fn reap(root: &Path, written: &std::collections::BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || written.contains(&path) || !crate::sync::has_symposium_marker(&path) {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!(
                report = %crate::report::ReportEvent::SkillRemoved {
                    path: crate::output::display_path(&path),
                },
            ),
            Err(e) => tracing::info!(
                report = %crate::report::ReportEvent::Warning {
                    message: format!(
                        "failed to remove stale {}: {e}",
                        crate::output::display_path(&path)
                    ),
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests;
