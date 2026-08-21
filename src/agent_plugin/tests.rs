use std::collections::BTreeMap;

use super::*;
use crate::config::UseEntry;
use crate::plugins::{Plugin, PluginSource, SkillGroup};
use crate::pm::{ANY_VERSION, PackageId};
use crate::predicate::{Predicate, PredicateSet};
use crate::skills::Skill;

fn wildcard() -> PredicateSet {
    PredicateSet::from_depends_on("*").expect("wildcard")
}

fn on_serde() -> PredicateSet {
    PredicateSet::from_depends_on("serde").expect("serde")
}

fn registry_plugin(name: &str, predicates: PredicateSet) -> ParsedPlugin {
    ParsedPlugin {
        plugin: Plugin {
            name: name.to_string(),
            predicates,
            ..Default::default()
        },
        workspace_member: false,
        canonical: PackageId::new("user-plugins", name, ANY_VERSION),
    }
}

fn skill_of(plugin: &ParsedPlugin, name: &str, path: &str) -> SkillWithGroupContext {
    SkillWithGroupContext {
        skill: Skill {
            frontmatter: BTreeMap::from([("name".to_string(), name.to_string())]),
            predicates: PredicateSet::default(),
            path: PathBuf::from(path),
        },
        origin_hash: crate::skills::hash_origin_key(&path),
        plugin: plugin.plugin.name.clone(),
        plugin_id: plugin.canonical.clone(),
    }
}

fn no_config() -> PluginsConfig {
    PluginsConfig::default()
}

// ── scope ────────────────────────────────────────────────────────────

#[test]
fn wildcard_registry_plugin_is_global() {
    let plugin = registry_plugin("pdf-tools", wildcard());
    assert_eq!(Scope::of(&plugin, &[], &no_config()), Scope::Global);
}

#[test]
fn a_concrete_dependency_gate_keeps_a_plugin_project_scoped() {
    let plugin = registry_plugin("pdf-tools", on_serde());
    assert_eq!(Scope::of(&plugin, &[], &no_config()), Scope::Project);
}

#[test]
fn workspace_members_and_crate_plugins_are_project_scoped() {
    let mut member = registry_plugin("house-style", wildcard());
    member.workspace_member = true;
    assert_eq!(Scope::of(&member, &[], &no_config()), Scope::Project);

    let mut from_crate = registry_plugin("widget", wildcard());
    from_crate.canonical = PackageId::new("cargo", "widget", "1.0.0");
    assert_eq!(Scope::of(&from_crate, &[], &no_config()), Scope::Project);
}

#[test]
fn a_dormant_plugin_goes_global_only_when_used_globally() {
    let mut dormant = registry_plugin("pdf-tools", PredicateSet::default());
    dormant.plugin.requires_use = true;

    assert_eq!(Scope::of(&dormant, &[], &no_config()), Scope::Project);

    let workspace_scoped = PluginsConfig {
        used: vec![UseEntry::Workspace {
            name: "pdf-tools".into(),
            workspace: PathBuf::from("/work/reporter"),
        }],
        ..Default::default()
    };
    assert_eq!(
        Scope::of(&dormant, &[], &workspace_scoped),
        Scope::Project,
        "a workspace `use` entry is workspace-dependent by definition"
    );

    let globally = PluginsConfig {
        used: vec![UseEntry::Global("pdf_tools".into())],
        ..Default::default()
    };
    assert_eq!(
        Scope::of(&dormant, &[], &globally),
        Scope::Global,
        "global `use` names match hyphen/underscore-insensitively"
    );
}

#[test]
fn a_dependency_gated_group_or_skill_keeps_the_plugin_project_scoped() {
    let mut grouped = registry_plugin("pdf-tools", wildcard());
    grouped.plugin.skills = vec![SkillGroup {
        predicates: on_serde(),
        source: PluginSource::Path(PathBuf::from("skills")),
        source_label: None,
        workspace_member: false,
    }];
    assert_eq!(Scope::of(&grouped, &[], &no_config()), Scope::Project);

    let plugin = registry_plugin("pdf-tools", wildcard());
    let mut gated = skill_of(&plugin, "extract-tables", "/reg/pdf/skills/x/SKILL.md");
    gated.skill.predicates = on_serde();
    assert_eq!(
        Scope::of(&plugin, &[&gated], &no_config()),
        Scope::Project,
        "a dep-gated skill makes the compiled content vary by workspace"
    );
}

#[test]
fn shell_and_path_predicates_are_treated_as_workspace_dependent() {
    let set = PredicateSet {
        predicates: vec![Predicate::Shell("true".into())],
    };
    assert!(!set.is_workspace_independent());

    assert!(wildcard().is_workspace_independent());
    assert!(PredicateSet::default().is_workspace_independent());
    assert!(!on_serde().is_workspace_independent());
}

// ── compile ──────────────────────────────────────────────────────────

#[test]
fn skills_are_grouped_under_the_plugin_that_contributed_them() {
    let one = registry_plugin("pdf-tools", wildcard());
    let two = registry_plugin("csv-tools", wildcard());
    let skills = vec![
        skill_of(&one, "extract-tables", "/reg/pdf/skills/extract/SKILL.md"),
        skill_of(&one, "read-forms", "/reg/pdf/skills/forms/SKILL.md"),
        skill_of(&two, "split-rows", "/reg/csv/skills/split/SKILL.md"),
    ];

    let compiled = compile(&[one, two], &skills, &no_config());
    let names: Vec<(&str, usize)> = compiled
        .iter()
        .map(|p| (p.dir_name.as_str(), p.skills.len()))
        .collect();
    assert_eq!(names, vec![("pdf-tools", 2), ("csv-tools", 1)]);
}

#[test]
fn a_plugin_with_no_applicable_skills_compiles_to_nothing() {
    let plugin = registry_plugin("pdf-tools", wildcard());
    assert!(compile(&[plugin], &[], &no_config()).is_empty());
}

#[test]
fn names_that_slug_alike_are_both_suffixed() {
    let underscored = registry_plugin("pdf_tools", wildcard());
    let hyphenated = registry_plugin("pdf-tools", wildcard());
    let skills = vec![
        skill_of(&underscored, "a", "/reg/one/skills/a/SKILL.md"),
        skill_of(&hyphenated, "b", "/reg/two/skills/b/SKILL.md"),
    ];

    let compiled = compile(&[underscored, hyphenated], &skills, &no_config());
    assert_eq!(compiled.len(), 2);
    for plugin in &compiled {
        assert!(
            plugin.dir_name.starts_with("pdf-tools-"),
            "expected a suffixed name, got {}",
            plugin.dir_name
        );
        assert!(manifest::is_valid_name(&plugin.dir_name));
    }
    assert_ne!(compiled[0].dir_name, compiled[1].dir_name);
    assert_eq!(
        compiled[0].manifest.name, "pdf-tools",
        "the manifest keeps the undisambiguated name; only the directory moves"
    );
}

#[test]
fn one_skill_reached_twice_through_a_plugin_is_compiled_once() {
    let plugin = registry_plugin("pdf-tools", wildcard());
    let once = skill_of(&plugin, "extract", "/reg/pdf/skills/extract/SKILL.md");
    let twice = skill_of(&plugin, "extract", "/reg/pdf/skills/extract/SKILL.md");
    let compiled = compile(&[plugin], &[once, twice], &no_config());
    assert_eq!(compiled[0].skills.len(), 1);
}

#[test]
fn same_named_skills_from_different_paths_both_survive_with_suffixes() {
    let plugin = registry_plugin("pdf-tools", wildcard());
    let skills = vec![
        skill_of(&plugin, "extract", "/reg/pdf/a/SKILL.md"),
        skill_of(&plugin, "extract", "/reg/pdf/b/SKILL.md"),
    ];
    let compiled = compile(&[plugin], &skills, &no_config());
    let dirs: Vec<&str> = compiled[0]
        .skills
        .iter()
        .map(|s| s.dir_name.as_str())
        .collect();
    assert_eq!(dirs.len(), 2);
    assert!(dirs.iter().all(|d| d.starts_with("extract-")), "{dirs:?}");
    assert_ne!(dirs[0], dirs[1]);
}

#[test]
fn the_version_comes_from_the_manifest_then_the_resolved_crate() {
    let mut declared = registry_plugin("pdf-tools", wildcard());
    declared.plugin.version = Some("1.2.0".into());
    assert_eq!(version_of(&declared).as_deref(), Some("1.2.0"));

    let mut from_crate = registry_plugin("widget", wildcard());
    from_crate.canonical = PackageId::new("cargo", "widget", "0.3.1");
    assert_eq!(version_of(&from_crate).as_deref(), Some("0.3.1"));

    let placeholder = registry_plugin("pdf-tools", wildcard());
    assert_eq!(
        version_of(&placeholder),
        None,
        "the `*` placeholder is not a version"
    );
}

// ── write and reap ───────────────────────────────────────────────────

fn skill_on_disk(dir: &Path, name: &str, body: &str) -> PathBuf {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: d\n---\n{body}\n"),
    )
    .expect("write SKILL.md");
    skill_dir.join("SKILL.md")
}

#[test]
fn write_produces_a_manifest_beside_the_skills() {
    let tmp = tempfile::tempdir().expect("tmp");
    let source = tmp.path().join("source");
    let skill_md = skill_on_disk(&source, "extract", "body");

    let compiled = CompiledPlugin {
        dir_name: "pdf-tools".into(),
        manifest: Manifest::new("pdf-tools".into(), Some("1.2.0".into()), None),
        scope: Scope::Global,
        skills: vec![CompiledSkill {
            dir_name: "extract".into(),
            source_dir: skill_md.parent().unwrap().to_path_buf(),
        }],
    };

    let root = tmp.path().join("staging");
    let dest = write(&compiled, &root, tmp.path(), Duration::ZERO).expect("write");

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dest.join("plugin.json")).expect("read manifest"))
            .expect("parse manifest");
    assert_eq!(manifest["name"], "pdf-tools");
    assert_eq!(manifest["version"], "1.2.0");
    assert_eq!(manifest["$schema"], manifest::SCHEMA_URL);

    assert!(dest.join("skills/extract/SKILL.md").is_file());
    assert!(
        dest.join(crate::sync::MARKER_FILE).is_file(),
        "compiled dirs carry the ownership marker so cleanup can find them"
    );
    assert!(
        !dest.join(".gitignore").exists(),
        "the staging root carries the only .gitignore"
    );
}

#[test]
fn rewriting_identical_content_leaves_the_directory_untouched() {
    let tmp = tempfile::tempdir().expect("tmp");
    let source = tmp.path().join("source");
    let skill_md = skill_on_disk(&source, "extract", "body");
    let compiled = CompiledPlugin {
        dir_name: "pdf-tools".into(),
        manifest: Manifest::new("pdf-tools".into(), None, None),
        scope: Scope::Global,
        skills: vec![CompiledSkill {
            dir_name: "extract".into(),
            source_dir: skill_md.parent().unwrap().to_path_buf(),
        }],
    };
    let root = tmp.path().join("staging");

    let dest = write(&compiled, &root, tmp.path(), Duration::ZERO).expect("first write");
    let installed = dest.join("skills/extract/SKILL.md");
    let before = fs::metadata(&installed)
        .and_then(|m| m.modified())
        .expect("mtime");

    write(&compiled, &root, tmp.path(), Duration::ZERO).expect("second write");
    let after = fs::metadata(&installed)
        .and_then(|m| m.modified())
        .expect("mtime");
    assert_eq!(before, after, "unchanged content must not be recopied");

    fs::write(
        skill_md,
        "---\nname: extract\ndescription: d\n---\nchanged\n",
    )
    .expect("edit");
    write(&compiled, &root, tmp.path(), Duration::ZERO).expect("third write");
    assert!(
        fs::read_to_string(&installed)
            .expect("read")
            .contains("changed"),
        "changed content must be recopied"
    );
}

#[test]
fn reap_removes_marked_directories_and_leaves_user_ones_alone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("staging");
    let kept = root.join("kept");
    let stale = root.join("stale");
    let user = root.join("user-authored");
    for dir in [&kept, &stale, &user] {
        fs::create_dir_all(dir).expect("create");
    }
    for dir in [&kept, &stale] {
        fs::write(dir.join(crate::sync::MARKER_FILE), "").expect("marker");
    }

    reap(&root, &std::collections::BTreeSet::from([kept.clone()]));

    assert!(kept.is_dir(), "a directory written this run stays");
    assert!(
        !stale.exists(),
        "a marked directory we did not write is reaped"
    );
    assert!(user.is_dir(), "an unmarked directory is never touched");
}
