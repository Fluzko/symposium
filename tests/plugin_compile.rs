use std::path::{Path, PathBuf};

use symposium_testlib::{TestContext, TestMode, with_fixture};

fn authoring(ctx: &TestContext, name: &str) -> String {
    ctx.tempdir
        .join("authoring")
        .join(name)
        .display()
        .to_string()
}

fn out_dir(ctx: &TestContext) -> PathBuf {
    ctx.tempdir.join("out")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn compile_writes_both_manifests_and_the_skills() {
    with_fixture(
        TestMode::SimulationOnly,
        &["plugin-compile0"],
        async |mut ctx| {
            let out = out_dir(&ctx);
            ctx.symposium(&[
                "plugin",
                "compile",
                &authoring(&ctx, "kit"),
                "--out",
                &out.display().to_string(),
            ])
            .await?;

            let dir = out.join("kit");
            let manifest = read(&dir.join("plugin.json"));
            assert_eq!(manifest, read(&dir.join(".claude-plugin/plugin.json")));
            let manifest: serde_json::Value = serde_json::from_str(&manifest)?;
            assert_eq!(
                manifest,
                serde_json::json!({
                    "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
                    "name": "kit",
                    "version": "0.0.0",
                })
            );
            assert_eq!(
                read(&dir.join("skills/kit-guidance/SKILL.md")),
                read(
                    &ctx.tempdir
                        .join("authoring/kit/skills/kit-guidance/SKILL.md")
                )
            );
            assert_eq!(entries(&out), ["kit"]);
            Ok(())
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn compile_takes_a_bare_skill_and_a_gated_plugin() {
    with_fixture(
        TestMode::SimulationOnly,
        &["plugin-compile0"],
        async |mut ctx| {
            let out = out_dir(&ctx).display().to_string();
            for (name, skill) in [
                ("solo-skill", "solo-skill"),
                ("gated-kit", "gated-guidance"),
            ] {
                ctx.symposium(&["plugin", "compile", &authoring(&ctx, name), "--out", &out])
                    .await?;
                let skill_md = out_dir(&ctx)
                    .join(name)
                    .join("skills")
                    .join(skill)
                    .join("SKILL.md");
                assert!(skill_md.is_file(), "{name}");
            }
            Ok(())
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn compile_again_replaces_its_own_output_and_reaps_nothing() {
    with_fixture(
        TestMode::SimulationOnly,
        &["plugin-compile0"],
        async |mut ctx| {
            let out = out_dir(&ctx);
            let unrelated = out.join("unrelated");
            std::fs::create_dir_all(&unrelated)?;
            std::fs::write(unrelated.join(".symposium"), "")?;
            let args = |ctx: &TestContext| {
                [
                    "plugin".to_string(),
                    "compile".to_string(),
                    authoring(ctx, "kit"),
                    "--out".to_string(),
                    out.display().to_string(),
                ]
            };

            let first = args(&ctx);
            ctx.symposium(&first.iter().map(String::as_str).collect::<Vec<_>>())
                .await?;
            let source = ctx
                .tempdir
                .join("authoring/kit/skills/kit-guidance/SKILL.md");
            std::fs::write(
                &source,
                read(&source).replace("First draft.", "Second draft."),
            )?;
            ctx.symposium(&first.iter().map(String::as_str).collect::<Vec<_>>())
                .await?;

            assert_eq!(
                read(&out.join("kit/skills/kit-guidance/SKILL.md")),
                read(&source)
            );
            assert!(unrelated.is_dir());
            Ok(())
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn compile_refuses_a_directory_it_did_not_write() {
    with_fixture(
        TestMode::SimulationOnly,
        &["plugin-compile0"],
        async |mut ctx| {
            let out = out_dir(&ctx);
            let own = out.join("kit");
            std::fs::create_dir_all(&own)?;
            std::fs::write(own.join("notes.txt"), "mine")?;

            let err = ctx
                .symposium(&[
                    "plugin",
                    "compile",
                    &authoring(&ctx, "kit"),
                    "--out",
                    &out.display().to_string(),
                ])
                .await
                .expect_err("a directory symposium did not write must stay");
            assert!(err.to_string().contains("nothing was compiled"), "{err}");
            assert_eq!(entries(&own), ["notes.txt"]);
            Ok(())
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn compile_warns_about_what_agent_plugins_cannot_carry() {
    use symposium::report::{ReportLayer, ReportMode};
    use tracing_subscriber::layer::SubscriberExt;

    with_fixture(TestMode::SimulationOnly, &["plugin-compile0"], async |mut ctx| {
        let out = out_dir(&ctx).display().to_string();
        let kit = authoring(&ctx, "hooked-kit");

        symposium_testlib::install_tracing_baseline();
        let (layer, handle) = ReportLayer::new(ReportMode::Json, tracing::Level::INFO);
        let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
        ctx.symposium(&["plugin", "compile", &kit, "--out", &out]).await?;
        drop(guard);

        let warned = handle.drain().iter().any(|event| {
            event["message"].as_str()
                == Some("`hooked-kit` also has hooks; an Agent Plugins directory carries only skills, so they are left out")
        });
        assert!(warned);
        assert!(out_dir(&ctx).join("hooked-kit/skills/hooked-guidance/SKILL.md").is_file());
        assert!(!out_dir(&ctx).join("hooked-kit/check.sh").exists());
        Ok(())
    })
    .await
    .unwrap();
}
