use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use crate::agents::{Agent, CompiledPlugin};
use crate::config::Symposium;
use crate::output::display_path;
use crate::report::ReportEvent;

pub(super) fn deliver_copies(
    sym: &Symposium,
    agent: Agent,
    plugins_dir: &Path,
    plugins: &[CompiledPlugin],
) -> Vec<String> {
    let agent = agent.config_name();
    let debounce = Duration::from_secs(sym.config.sync_debounce_secs);
    let mut installed = BTreeSet::new();
    let mut updated = Vec::new();
    for plugin in plugins {
        let dest = plugins_dir.join(&plugin.name);
        match crate::sync::sync_plugin_dir(&plugin.dir, &dest, debounce) {
            Ok(changed) => {
                if changed {
                    tracing::info!(
                        report = %ReportEvent::SkillInstalled {
                            skill: plugin.name.clone(),
                            agent: agent.to_string(),
                            dest: display_path(&dest),
                            plugin: true,
                        },
                    );
                    updated.push(plugin.name.clone());
                }
                installed.insert(dest);
            }
            Err(e) => tracing::info!(
                report = %ReportEvent::Warning {
                    message: format!("failed to install plugin {} for {agent}: {e:#}", plugin.name),
                },
            ),
        }
    }
    crate::sync::reap_unlisted(plugins_dir, |dir| {
        installed.contains(dir) || !crate::compile::is_plugin_dir(dir)
    });
    updated
}

#[cfg(test)]
mod tests;
