//! Opt-in e2e for self-evolution skill generation.
//!
//! Drives the FULL production workflow-crystallization path
//! (`skill::workflow_distill::crystallize_workflow`) against a REAL
//! provider built from the user's config — prompt build → LLM distill →
//! repair → validate → write SKILL.md to disk — and asserts a valid
//! skill landed.
//!
//! Ignored by default (hits a paid LLM + network). Run explicitly:
//!   cargo test --test skill_evolution_e2e -- --ignored --nocapture
//! Override the distill model (default: rsclaw/rsclaw-agent-v1):
//!   DISTILL_MODEL=doubao/doubao-... cargo test --test skill_evolution_e2e --
//! --ignored --nocapture

use std::sync::Arc;

use rsclaw::{
    config, provider::build::build_providers, skill::workflow_distill::crystallize_workflow,
};
use rsclaw_types::turn_metrics::TurnMetrics;

#[tokio::test]
#[ignore = "hits a real LLM + network; run with --ignored"]
async fn evolution_generates_skill_from_hard_turn() {
    // 1. Real config + provider registry (reads ~/.rsclaw/rsclaw.json5).
    let cfg = config::load().expect("load config");
    let providers = Arc::new(build_providers(&cfg));

    let model =
        std::env::var("DISTILL_MODEL").unwrap_or_else(|_| "rsclaw/rsclaw-agent-v1".to_owned());
    eprintln!("[e2e] distill model = {model}");

    // 2. Fabricate a "hard turn" — many tool calls + an error — the kind of
    //    experience the agent should codify into a SKILL.md.
    let mut m = TurnMetrics::new();
    m.record_tool(
        "web_fetch",
        "url=api.example.com/odds".into(),
        "200 OK, 4KB json".into(),
        false,
    );
    m.record_tool(
        "web_fetch",
        "url=api.example.com/fixtures".into(),
        "timeout after 20s".into(),
        true,
    );
    m.record_tool(
        "web_fetch",
        "url=api.example.com/fixtures (retry, 60s timeout)".into(),
        "200 OK".into(),
        false,
    );
    m.record_tool(
        "image_gen",
        "prompt=match poster, size=1024x1024".into(),
        "wrote /tmp/poster.png".into(),
        false,
    );
    m.final_text_len = 800;

    let user_text = "查一下今晚世界杯墨西哥vs南非的赔率和赛程，再给我做一张比赛海报";
    let reply_text = "赔率/赛程已取（fixtures 接口要 60s 超时，20s 会失败），海报已生成。";

    // 3. Temp skills dir so we don't pollute ~/.rsclaw/skills.
    let skills_dir = std::env::temp_dir().join("rsclaw_skill_evo_e2e");
    let _ = std::fs::remove_dir_all(&skills_dir);
    std::fs::create_dir_all(&skills_dir).expect("mkdir skills_dir");

    // 4. Full production path.
    let result = crystallize_workflow(
        user_text,
        reply_text,
        &m,
        0xDEAD_BEEF,
        &providers,
        &model,
        &skills_dir,
    )
    .await
    .expect("crystallize_workflow should not hard-error");

    // 5. Assert a skill was actually written + is valid.
    let path = result.expect(
        "expected a generated SKILL.md path (None = evolution disabled, no model, \
         or the LLM output failed validation twice — check logs above)",
    );
    eprintln!("[e2e] generated skill at: {}", path.display());

    let body = std::fs::read_to_string(&path).expect("read generated SKILL.md");
    eprintln!("\n----- generated SKILL.md -----\n{body}\n------------------------------");

    assert!(body.starts_with("---"), "must start with YAML frontmatter");
    assert!(body.contains("name:"), "frontmatter must have name:");
    assert!(
        body.contains("description:"),
        "frontmatter must have description:"
    );
    assert!(
        path.file_name().unwrap().to_string_lossy() == "SKILL.md",
        "file must be named SKILL.md"
    );
    assert!(
        path.parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("auto-"),
        "generated skill dir should be auto-prefixed"
    );
}
