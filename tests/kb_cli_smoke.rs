//! `rsclaw kb` CLI smoke tests. Invokes the compiled binary via the
//! cargo-provided `CARGO_BIN_EXE_rsclaw` path so the whole
//! cli → cmd → library stack is exercised. Library tests cover
//! correctness; these tests guard against shell-facing regressions
//! (output formatting, exit codes, arg parsing).
//!
//! Tests run sequentially per crate, but each test gets its own
//! `--base-dir` so they don't share state.

use std::{path::Path, process::Command};

use tempfile::TempDir;

fn rsclaw() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rsclaw"))
}

fn run(cmd: &mut Command) -> (String, String, i32) {
    let out = cmd.output().expect("spawn rsclaw");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let code = out.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

#[test]
fn kb_help_lists_all_subcommands() {
    let (stdout, _stderr, code) = run(rsclaw().args(["kb", "--help"]));
    assert_eq!(code, 0);
    for sub in &[
        "add",
        "ls",
        "rm",
        "search",
        "show",
        "visibility",
        "compact",
        "stats",
        "export",
    ] {
        assert!(
            stdout.contains(sub),
            "expected `kb --help` to list `{sub}`, got:\n{stdout}"
        );
    }
}

#[test]
fn kb_add_then_search_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let fixture = tmp.path().join("note.md");
    std::fs::write(
        &fixture,
        "# Title\n\nThe yellow dwarf star is in the Milky Way.",
    )
    .unwrap();

    // add
    let (stdout, stderr, code) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        fixture.to_str().unwrap(),
        "--tags",
        "demo",
    ]));
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("\"added\":1") || stdout.contains("\"docs_added\": 1"),
        "add output: {stdout}"
    );

    // search
    let (stdout, _stderr, code) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "search",
        "yellow dwarf",
    ]));
    assert_eq!(code, 0);
    assert!(
        stdout.contains("yellow dwarf") || stdout.contains("Title"),
        "search did not surface the doc: {stdout}"
    );
}

#[test]
fn kb_ls_after_add_shows_doc() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let fixture = tmp.path().join("doc.md");
    std::fs::write(&fixture, "# Demo\n\nbody.").unwrap();

    run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        fixture.to_str().unwrap(),
        "--tags",
        "lstest",
    ]));
    let (stdout, _stderr, code) =
        run(rsclaw().args(["--base-dir", base.to_str().unwrap(), "kb", "ls"]));
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Demo") && stdout.contains("lstest"),
        "kb ls output missing doc: {stdout}"
    );
}

#[test]
fn kb_stats_reports_json() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let fixture = tmp.path().join("s.md");
    std::fs::write(&fixture, "# S\n\nbody.").unwrap();

    run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        fixture.to_str().unwrap(),
    ]));
    let (stdout, _stderr, code) =
        run(rsclaw().args(["--base-dir", base.to_str().unwrap(), "kb", "stats"]));
    assert_eq!(code, 0);
    // Look for the last line (JSON); upstream prefix line is the
    // "profile:" banner from rsclaw's --base-dir handling.
    let last = stdout.lines().last().unwrap_or_default();
    assert!(
        last.starts_with('{') && last.ends_with('}'),
        "kb stats last line should be JSON, got: {last}"
    );
    assert!(last.contains("\"docs_active\":1"), "stats: {last}");
    assert!(last.contains("\"kb_chunks\":1"), "stats: {last}");
}

#[test]
fn kb_rm_tombstones_then_search_hides_it() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let fixture = tmp.path().join("rm.md");
    std::fs::write(&fixture, "# RM\n\ndelete me yellow dwarf star.").unwrap();
    run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        fixture.to_str().unwrap(),
    ]));
    let (ls_stdout, _, _) = run(rsclaw().args(["--base-dir", base.to_str().unwrap(), "kb", "ls"]));
    let doc_id = ls_stdout
        .lines()
        .find_map(|l| {
            // Lines start with the ULID (26 chars).
            let first = l.split_whitespace().next()?;
            if first.len() == 26 && first.chars().all(|c| c.is_ascii_alphanumeric()) {
                Some(first.to_string())
            } else {
                None
            }
        })
        .expect("doc_id not found in ls output");

    let (_, _, code) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "rm",
        &doc_id,
        "--yes",
    ]));
    assert_eq!(code, 0);

    let (stdout, _, _) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "search",
        "yellow dwarf",
    ]));
    assert!(
        stdout.contains("(no hits)"),
        "tombstoned doc still surfacing: {stdout}"
    );
}

#[test]
fn kb_export_writes_body_to_path() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let fixture = tmp.path().join("export.md");
    std::fs::write(&fixture, "# Export Test\n\nbody body body").unwrap();
    run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        fixture.to_str().unwrap(),
    ]));
    let (ls_stdout, _, _) = run(rsclaw().args(["--base-dir", base.to_str().unwrap(), "kb", "ls"]));
    let doc_id = ls_stdout
        .lines()
        .find_map(|l| {
            let first = l.split_whitespace().next()?;
            if first.len() == 26 && first.chars().all(|c| c.is_ascii_alphanumeric()) {
                Some(first.to_string())
            } else {
                None
            }
        })
        .expect("doc_id not found in ls output");
    let out_path = tmp.path().join("out.md");
    let (_, stderr, code) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "export",
        &doc_id,
        "--to",
        out_path.to_str().unwrap(),
    ]));
    assert_eq!(code, 0, "stderr: {stderr}");
    let body = std::fs::read_to_string(&out_path).unwrap();
    assert!(body.contains("body body body"), "exported file: {body}");
}

#[test]
fn kb_add_recursive_ingests_directory() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let nested = tmp.path().join("notes/sub");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("a.md"), "# A\n\ncats are nocturnal").unwrap();
    std::fs::write(tmp.path().join("notes/b.md"), "# B\n\ndogs love walks").unwrap();
    std::fs::write(tmp.path().join("notes/c.bin"), b"\x00\x01\x02").unwrap();

    let (stdout, stderr, code) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        tmp.path().join("notes").to_str().unwrap(),
        "--recursive",
        "--tags",
        "batch",
    ]));
    assert_eq!(code, 0, "stderr: {stderr}");
    // The summary JSON line should report 2 added (a.md + b.md).
    let summary = stdout.lines().last().unwrap_or_default();
    assert!(summary.contains("\"added\":2"), "summary: {summary}");
    assert!(summary.contains("\"files_seen\":2"), "summary: {summary}");
}

#[test]
fn kb_sync_all_dry_run_reports_zero_for_file_only_kb() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let fixture = tmp.path().join("local.md");
    std::fs::write(&fixture, "# Local\n\nbody.").unwrap();
    run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        fixture.to_str().unwrap(),
    ]));
    let (stdout, _stderr, code) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "sync-all",
        "--dry-run",
    ]));
    assert_eq!(code, 0);
    let last = stdout.lines().last().unwrap_or_default();
    assert!(
        last.contains("\"dry_run\":true") && last.contains("\"candidates\":0"),
        "sync-all dry-run: {last}"
    );
}

#[test]
fn kb_search_json_emits_full_output_struct() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let fixture = tmp.path().join("j.md");
    std::fs::write(
        &fixture,
        "# JSON\n\nThe yellow dwarf star is in the Milky Way.",
    )
    .unwrap();
    run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        fixture.to_str().unwrap(),
    ]));
    let (stdout, _stderr, code) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "search",
        "yellow dwarf",
        "--json",
    ]));
    assert_eq!(code, 0);
    // Drop the "profile:" prefix line, parse the rest as JSON.
    let json_body = stdout
        .lines()
        .skip_while(|l| !l.trim_start().starts_with('{'))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed: serde_json::Value =
        serde_json::from_str(&json_body).unwrap_or_else(|_| panic!("invalid JSON: {json_body}"));
    assert!(parsed.get("results").is_some(), "missing results: {parsed}");
    assert!(
        parsed.get("entity_alignment").is_some(),
        "missing entity_alignment: {parsed}"
    );
    assert!(
        parsed.get("warnings").is_some(),
        "missing warnings: {parsed}"
    );
}

#[test]
fn kb_visibility_private_hides_doc_from_default_scope() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let fixture = tmp.path().join("v.md");
    std::fs::write(&fixture, "# Secret\n\nclassified yellow dwarf data.").unwrap();
    run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "add",
        fixture.to_str().unwrap(),
    ]));
    // Default scope can see Global docs.
    let (before, _, _) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "search",
        "yellow dwarf",
    ]));
    assert!(
        before.contains("classified") || before.contains("Secret"),
        "doc should be visible as Global before visibility flip: {before}"
    );

    let (ls_stdout, _, _) = run(rsclaw().args(["--base-dir", base.to_str().unwrap(), "kb", "ls"]));
    let doc_id = ls_stdout
        .lines()
        .find_map(|l| {
            let first = l.split_whitespace().next()?;
            if first.len() == 26 && first.chars().all(|c| c.is_ascii_alphanumeric()) {
                Some(first.to_string())
            } else {
                None
            }
        })
        .expect("doc_id not in ls");

    let (_, _, code) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "visibility",
        &doc_id,
        "private",
    ]));
    assert_eq!(code, 0);

    // After visibility=private, default scope (no user_id) sees nothing.
    let (after, _, _) = run(rsclaw().args([
        "--base-dir",
        base.to_str().unwrap(),
        "kb",
        "search",
        "yellow dwarf",
    ]));
    assert!(
        after.contains("(no hits)"),
        "private doc must be hidden from default scope: {after}"
    );
}

// Bare-bones sanity: invoking the binary at all returns help.
#[test]
fn rsclaw_prints_help_with_no_args() {
    let out = Command::new(env!("CARGO_BIN_EXE_rsclaw"))
        .arg("--help")
        .output()
        .expect("spawn rsclaw");
    assert_eq!(out.status.code().unwrap_or(-1), 0);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("kb"),
        "kb subcommand not in --help: {stdout}"
    );
}

#[allow(dead_code)]
fn _silence_unused_imports(_: &Path) {}
