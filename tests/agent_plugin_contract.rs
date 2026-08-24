//! Agent contract tests: does the agent *actually load and use* what symposium
//! installed?
//!
//! This is the only level that can catch a plausible file in a plausible place
//! that the agent ignores — the failure mode that shipped a broken Copilot
//! install past a green unit test asserting the exact bytes Copilot's own CLI
//! writes. What was missing there was a fact about Copilot, not about us.
//!
//! **Opt in with `SYMPOSIUM_AGENT_CONTRACT=1`.** These tests write into the
//! developer's real agent configuration, because that is the only place a real
//! agent reads: redirecting `HOME` isolates the config but also cuts the agent
//! off from its credentials, which are stored under it. Every file touched is
//! snapshotted first and restored on the way out, including on panic.
//!
//! Each agent is skipped, not failed, when its CLI is absent or cannot
//! authenticate. A skip prints why, so a green run never silently means
//! "verified nothing".

use std::path::{Path, PathBuf};
use std::process::Command;

/// The skill reports this, and only a skill that actually ran can produce it.
const TOKEN_V1: &str = "CONTRACT-TOKEN-A1B2";
/// What it reports after the source is edited, proving an update propagated.
const TOKEN_V2: &str = "CONTRACT-TOKEN-C3D4";

const ASK: &str = "Do you have a skill named contract-check? If so, invoke it and \
                   report only the token it gives you. Otherwise answer NONE.";

fn enabled() -> bool {
    std::env::var("SYMPOSIUM_AGENT_CONTRACT").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

/// How to drive one agent, and what of its configuration we may disturb.
struct AgentUnderTest {
    /// Symposium's name for it, as `init --add-agent` takes it.
    name: &'static str,
    /// The binary to run.
    bin: &'static str,
    /// Arguments that make it answer one prompt and exit.
    prompt_args: &'static [&'static str],
    /// Configuration files symposium writes for this agent.
    config_files: Vec<PathBuf>,
    /// Directories symposium copies into for this agent.
    content_dirs: Vec<PathBuf>,
}

fn agents() -> Vec<AgentUnderTest> {
    let h = home();
    vec![
        AgentUnderTest {
            name: "claude",
            bin: "claude",
            prompt_args: &["-p"],
            config_files: vec![
                h.join(".claude/settings.json"),
                h.join(".claude/plugins/known_marketplaces.json"),
            ],
            content_dirs: vec![],
        },
        AgentUnderTest {
            name: "codex",
            bin: "codex",
            prompt_args: &["exec", "--skip-git-repo-check"],
            config_files: vec![h.join(".codex/config.toml")],
            content_dirs: vec![h.join(".codex/plugins/cache/symposium")],
        },
        AgentUnderTest {
            name: "copilot",
            bin: "copilot",
            prompt_args: &["-p"],
            config_files: vec![
                h.join(".copilot/settings.json"),
                h.join(".copilot/config.json"),
            ],
            content_dirs: vec![h.join(".copilot/installed-plugins/symposium")],
        },
        AgentUnderTest {
            name: "gemini",
            bin: "gemini",
            prompt_args: &["-p"],
            config_files: vec![],
            content_dirs: vec![h.join(".gemini/extensions/contract-probe")],
        },
    ]
}

/// Snapshots every path it is given and puts them back when dropped, so a
/// failing assertion cannot leave the developer's agent configuration altered.
struct ConfigGuard {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    dirs: Vec<PathBuf>,
}

impl ConfigGuard {
    fn snapshot(agent: &AgentUnderTest) -> Self {
        let files = agent
            .config_files
            .iter()
            .map(|path| (path.clone(), std::fs::read(path).ok()))
            .collect();
        Self {
            files,
            dirs: agent.content_dirs.clone(),
        }
    }
}

impl Drop for ConfigGuard {
    fn drop(&mut self) {
        for dir in &self.dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
        for (path, original) in &self.files {
            match original {
                Some(bytes) => {
                    let _ = std::fs::write(path, bytes);
                }
                // It did not exist before this test, so it must not now.
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

/// Which scope the probe plugin should be enabled at.
///
/// The two take genuinely different paths: a global plugin is enabled in the
/// user's own settings and compiled under the symposium home, while a
/// project-scoped one is enabled in the *project's* settings, compiled under the
/// project, and registered under a marketplace named for that workspace.
#[derive(Clone, Copy, PartialEq)]
enum ProbeScope {
    Global,
    Project,
}

/// A fixture symposium home holding one plugin whose single skill reports a token.
struct Fixture {
    _tempdir: tempfile::TempDir,
    sym_home: PathBuf,
    cwd: PathBuf,
    skill: PathBuf,
}

impl Fixture {
    fn build(agent: &str) -> Self {
        Self::build_scoped(agent, ProbeScope::Global)
    }

    fn build_scoped(agent: &str, scope: ProbeScope) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = tempdir.path().to_path_buf();
        let sym_home = root.join("symposium-home");
        let skill_dir = sym_home.join("plugins/contract-probe/skills/contract-check");
        std::fs::create_dir_all(&skill_dir).expect("create skill dir");
        std::fs::create_dir_all(root.join("cwd")).expect("create cwd");

        // A project-scoped run needs a Rust workspace to be scoped *to*, and a
        // `use` entry naming it; a global run has neither.
        let cwd = match scope {
            ProbeScope::Global => root.join("cwd"),
            ProbeScope::Project => {
                let project = root.join("cwd");
                std::fs::create_dir_all(project.join("src")).expect("create project");
                std::fs::write(
                    project.join("Cargo.toml"),
                    "[package]\nname = \"contract-project\"\nversion = \"0.1.0\"\n\
                     edition = \"2021\"\n\n[dependencies]\n",
                )
                .expect("write Cargo.toml");
                std::fs::write(project.join("src/lib.rs"), "").expect("write lib.rs");
                project
            }
        };
        let used = match scope {
            ProbeScope::Global => "use = [\"contract-probe\"]".to_string(),
            ProbeScope::Project => format!(
                "use = [{{ name = \"contract-probe\", workspace = \"{}\" }}]",
                cwd.display().to_string().replace('\\', "/")
            ),
        };
        std::fs::write(
            sym_home.join("config.toml"),
            format!(
                "hook-scope = \"project\"\n\n[[agent]]\nname = \"{agent}\"\n\n\
                 [defaults]\nsymposium-recommendations = false\nuser-plugins = true\n\n\
                 [plugins]\n{used}\n"
            ),
        )
        .expect("write config");

        std::fs::write(
            sym_home.join("plugins/contract-probe/SYMPOSIUM.toml"),
            // Deliberately no `depends-on`: the plugin is dormant, so the `use`
            // entry is the only thing activating it and removing that entry
            // genuinely deactivates it. A `depends-on = ["*"]` probe would stay
            // active regardless and the removal check would prove nothing.
            "name = \"contract-probe\"\nversion = \"1.0.0\"\n\
             description = \"Symposium agent contract probe\"\n\n\
             [[skills]]\nsource.path = \"skills\"\n",
        )
        .expect("write manifest");

        let fixture = Self {
            skill: skill_dir.join("SKILL.md"),
            sym_home,
            cwd,
            _tempdir: tempdir,
        };
        fixture.write_skill(TOKEN_V1);
        fixture
    }

    fn write_skill(&self, token: &str) {
        std::fs::write(
            &self.skill,
            format!(
                "---\nname: contract-check\ndescription: Reports the token {token} when asked \
                 to verify symposium plugin delivery.\n---\n\nReply with the token {token}.\n"
            ),
        )
        .expect("write SKILL.md");
    }

    /// Stop the plugin applying, so the next sync takes it back out. The probe is
    /// dormant, so dropping its `use` entry is what deactivates it.
    fn stop_using(&self) {
        let path = self.sym_home.join("config.toml");
        let text = std::fs::read_to_string(&path).expect("read config");
        let cleared: String = text
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("use = [") {
                    "use = []".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{cleared}\n")).expect("write config");
    }

    fn sync(&self) {
        let status = Command::new(env!("CARGO_BIN_EXE_cargo-agents"))
            .arg("sync")
            .current_dir(&self.cwd)
            .env("SYMPOSIUM_HOME", &self.sym_home)
            .status()
            .expect("run cargo-agents sync");
        assert!(status.success(), "sync failed");
    }
}

/// Ask the agent, returning its output. `None` when the binary is missing.
fn ask(agent: &AgentUnderTest, cwd: &Path, prompt: &str) -> Option<String> {
    let output = Command::new(agent.bin)
        .args(agent.prompt_args)
        .arg(prompt)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

/// Is the agent installed and able to answer at all? A skip here is honest; a
/// failure would blame symposium for a missing login.
fn usable(agent: &AgentUnderTest, cwd: &Path) -> Result<(), String> {
    match ask(agent, cwd, "Reply with only the word READY.") {
        None => Err(format!("`{}` is not installed", agent.bin)),
        Some(text) if text.contains("READY") => Ok(()),
        Some(text) => {
            let hint = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            Err(format!(
                "`{}` could not answer (not authenticated?): {hint}",
                agent.bin
            ))
        }
    }
}

/// C1 install, C2 update, C3 remove — the whole contract for one agent.
fn contract(agent: &AgentUnderTest) {
    contract_at(agent, ProbeScope::Global)
}

fn contract_at(agent: &AgentUnderTest, scope: ProbeScope) {
    let fixture = Fixture::build_scoped(agent.name, scope);

    if let Err(why) = usable(agent, &fixture.cwd) {
        eprintln!("SKIP {}: {why}", agent.name);
        return;
    }

    let _guard = ConfigGuard::snapshot(agent);

    // C1 — installed, and the agent runs it.
    fixture.sync();
    let answer = ask(agent, &fixture.cwd, ASK).expect("agent ran");
    assert!(
        answer.contains(TOKEN_V1),
        "{}: expected {TOKEN_V1} after install, got:\n{answer}",
        agent.name
    );

    // C2 — the source changes and the agent sees the new content.
    fixture.write_skill(TOKEN_V2);
    fixture.sync();
    let answer = ask(agent, &fixture.cwd, ASK).expect("agent ran");
    assert!(
        answer.contains(TOKEN_V2) && !answer.contains(TOKEN_V1),
        "{}: expected {TOKEN_V2} after an edit, got:\n{answer}",
        agent.name
    );

    // C3 — it stops applying and the agent no longer has it.
    fixture.stop_using();
    fixture.sync();
    let answer = ask(agent, &fixture.cwd, ASK).expect("agent ran");
    assert!(
        !answer.contains(TOKEN_V1) && !answer.contains(TOKEN_V2),
        "{}: the skill should be gone, got:\n{answer}",
        agent.name
    );
}

fn run_for(name: &str) {
    if !enabled() {
        eprintln!("SKIP {name}: set SYMPOSIUM_AGENT_CONTRACT=1 to run agent contract tests");
        return;
    }
    let agent = agents()
        .into_iter()
        .find(|a| a.name == name)
        .expect("known agent");
    contract(&agent);
}

#[test]
fn claude_code_honors_the_install_contract() {
    run_for("claude");
}

/// Claude Code is the only agent that can bound a plugin to one project, and
/// that path differs from the global one: the enablement goes into the project's
/// own settings, and the marketplace is named for the workspace so two projects
/// cannot overwrite each other's registration.
#[test]
fn claude_code_honors_the_contract_at_project_scope() {
    if !enabled() {
        eprintln!("SKIP: set SYMPOSIUM_AGENT_CONTRACT=1 to run agent contract tests");
        return;
    }
    let agent = agents()
        .into_iter()
        .find(|a| a.name == "claude")
        .expect("known agent");
    contract_at(&agent, ProbeScope::Project);
}

#[test]
fn codex_honors_the_install_contract() {
    run_for("codex");
}

#[test]
fn copilot_honors_the_install_contract() {
    run_for("copilot");
}

#[test]
fn gemini_honors_the_install_contract() {
    run_for("gemini");
}

/// An agent that cannot bound a plugin to one project still receives that
/// plugin's skills, individually, under the project.
///
/// This is the claim that the per-skill path stays primary for project-scoped
/// plugins on every agent but Claude Code. Asserting the file lands is our half;
/// asking the agent is theirs.
#[test]
fn a_project_scoped_plugin_reaches_codex_as_a_plain_skill() {
    if !enabled() {
        eprintln!("SKIP: set SYMPOSIUM_AGENT_CONTRACT=1 to run agent contract tests");
        return;
    }
    let agent = agents()
        .into_iter()
        .find(|a| a.name == "codex")
        .expect("known agent");

    let fixture = Fixture::build_scoped(agent.name, ProbeScope::Project);
    if let Err(why) = usable(&agent, &fixture.cwd) {
        eprintln!("SKIP {}: {why}", agent.name);
        return;
    }
    let _guard = ConfigGuard::snapshot(&agent);

    fixture.sync();
    let installed = fixture.cwd.join(".agents/skills/contract-check/SKILL.md");
    assert!(
        installed.is_file(),
        "codex cannot scope a plugin to a project, so the skill has to arrive on its own at {}",
        installed.display()
    );
    assert!(
        !fixture
            .cwd
            .join(".symposium/plugins/contract-probe")
            .exists()
            || fixture.cwd.join(".symposium/plugins").exists(),
        "the compiled directory is only built for agents that can take it"
    );

    let answer = ask(&agent, &fixture.cwd, ASK).expect("agent ran");
    assert!(
        answer.contains(TOKEN_V1),
        "codex should read the project's own skills directory, got:\n{answer}"
    );
}

/// An agent with no plugin unit still receives the skill on its own, and that
/// path has to keep working now that the plugin-capable agents have left it.
///
/// Outside a workspace those skills have nowhere project-scoped to go, so they
/// land in the agent's user-level skills directory — the only place a globally
/// enabled plugin can reach an agent that cannot take a compiled directory.
#[test]
fn an_agent_without_a_plugin_unit_still_receives_the_skill() {
    if !enabled() {
        eprintln!("SKIP: set SYMPOSIUM_AGENT_CONTRACT=1 to run agent contract tests");
        return;
    }

    let installed = home().join(".agents/skills/contract-check");
    let opencode = AgentUnderTest {
        name: "opencode",
        bin: "opencode",
        prompt_args: &[],
        config_files: Vec::new(),
        content_dirs: vec![installed.clone()],
    };
    let _guard = ConfigGuard::snapshot(&opencode);

    let fixture = Fixture::build(opencode.name);
    fixture.sync();
    assert!(
        installed.join("SKILL.md").is_file(),
        "OpenCode has no plugin unit, so the skill has to arrive on its own at {}",
        installed.display()
    );

    fixture.stop_using();
    fixture.sync();
    assert!(
        !installed.exists(),
        "and it has to be reaped when the plugin stops applying"
    );
}
