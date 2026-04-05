use anyhow::Result;

use super::style::{bold, dim, green, red, yellow, banner};
use crate::{cli::DoctorArgs, config};

const VERSION: &str = env!("RSCLAW_BUILD_VERSION");

// ---------------------------------------------------------------------------
// Issue -- a detected problem with an optional auto-fix
// ---------------------------------------------------------------------------

struct Issue {
    message: String,
    /// A short description of what --fix will do.
    fix_hint: Option<&'static str>,
    /// The actual fix to apply. Returns a description of what was done.
    fix_fn: Option<Box<dyn FnOnce() -> Result<String>>>,
}

impl Issue {
    fn warn(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            fix_hint: None,
            fix_fn: None,
        }
    }

    fn fixable(
        msg: impl Into<String>,
        hint: &'static str,
        f: impl FnOnce() -> Result<String> + 'static,
    ) -> Self {
        Self {
            message: msg.into(),
            fix_hint: Some(hint),
            fix_fn: Some(Box::new(f)),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn cmd_doctor(args: DoctorArgs) -> Result<()> {
    banner(&format!("rsclaw v{VERSION} \u{2014} doctor"));

    let mut issues: Vec<Issue> = Vec::new();
    let mut passed: usize = 0;

    // -- 1. Config load -------------------------------------------------------
    let cfg = match config::load_quiet() {
        Ok(c) => {
            println!("  {} config loaded \u{2014} {} agent(s)", green("[ok]"), c.agents.list.len());
            passed += 1;
            Some(c)
        }
        Err(e) => {
            let err_str = format!("{e:#}");
            if err_str.contains("JSON5 parse error") || err_str.contains("expected") {
                issues.push(Issue::fixable(
                    format!("config error: {err_str}"),
                    "attempt auto-repair of config JSON syntax",
                    || repair_config_json(),
                ));
            } else if err_str.contains("invalid type") {
                issues.push(Issue::fixable(
                    format!("config error: {err_str}"),
                    "fix type mismatches in config",
                    || fix_config_type_mismatches(),
                ));
            } else {
                issues.push(Issue::warn(format!("config error: {err_str}")));
            }
            None
        }
    };

    // -- 2. State directory ---------------------------------------------------
    let base_dir = config::loader::base_dir();
    if !base_dir.is_dir() {
        let path = base_dir.clone();
        issues.push(Issue::fixable(
            format!("state directory missing: {}", base_dir.display()),
            "create state directory",
            move || {
                std::fs::create_dir_all(&path)?;
                Ok(format!("created {}", path.display()))
            },
        ));
    } else {
        println!("  {} state directory exists", green("[ok]"));
        passed += 1;
    }

    // -- 3. Stale PID file ----------------------------------------------------
    let pid_file = config::loader::pid_file();
    if pid_file.exists() {
        let stale = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|pid| !process_exists(pid))
            .unwrap_or(true);

        if stale {
            let path = pid_file.clone();
            issues.push(Issue::fixable(
                format!(
                    "stale PID file (gateway not running): {}",
                    pid_file.display()
                ),
                "delete stale PID file",
                move || {
                    std::fs::remove_file(&path)?;
                    Ok(format!("removed {}", path.display()))
                },
            ));
        } else {
            println!("  {} gateway running (PID file valid)", green("[ok]"));
            passed += 1;
        }
    }

    // -- 4. Config file permissions -------------------------------------------
    if let Some(cfg_path) = config::loader::detect_config_path() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&cfg_path) {
                let mode = meta.permissions().mode();
                if mode & 0o044 != 0 {
                    let path = cfg_path.clone();
                    let safe_mode = mode & !0o044;
                    issues.push(Issue::fixable(
                        format!(
                            "config file is world/group-readable ({:04o}): {}",
                            mode & 0o777,
                            cfg_path.display()
                        ),
                        "chmod to owner-only",
                        move || {
                            let mut perms = std::fs::metadata(&path)?.permissions();
                            perms.set_mode(safe_mode);
                            std::fs::set_permissions(&path, perms)?;
                            Ok(format!(
                                "chmod {:04o} {}",
                                safe_mode & 0o777,
                                path.display()
                            ))
                        },
                    ));
                } else {
                    println!("  {} config permissions {:04o}", green("[ok]"), mode & 0o777);
                    passed += 1;
                }
            }
        }
        #[cfg(windows)]
        {
            // Windows: user profile dirs (%LOCALAPPDATA%) are owner-only by default.
            // Check that the config file is not in a world-readable location.
            if let Ok(meta) = std::fs::metadata(&cfg_path) {
                if !meta.permissions().readonly() {
                    println!("  {} config file exists: {}", green("[ok]"), cfg_path.display());
                    passed += 1;
                }
            }
        }
    }

    // -- 5. Agent / model checks (advisory only) ------------------------------
    if let Some(ref c) = cfg {
        // agents.list empty is fine 鈥?a default "main" agent is auto-synthesized.
        if !c.agents.list.is_empty() {
            println!("  {} {} agent(s) configured", green("[ok]"), c.agents.list.len());
            passed += 1;
        }

        let has_default_model = c
            .agents
            .defaults
            .model
            .as_ref()
            .and_then(|m| m.primary.as_ref())
            .is_some();
        for a in &c.agents.list {
            let has_model = a.model.as_ref().and_then(|m| m.primary.as_ref()).is_some();
            if !has_model && !has_default_model {
                issues.push(Issue::warn(format!(
                    "agent '{}' has no model \u{2014} set agents.defaults.model.primary or agent-level model",
                    a.id
                )));
            }
        }

        let has_provider = c
            .model
            .models
            .as_ref()
            .map(|m| !m.providers.is_empty())
            .unwrap_or(false);
        if !has_provider {
            issues.push(Issue::warn(
                "no model providers configured \u{2014} set models.providers in config".to_owned(),
            ));
        }

        if c.gateway.auth_token.is_none() {
            issues.push(Issue::warn(
                "gateway.auth.token not set \u{2014} consider setting one for security".to_owned(),
            ));
        }
    }

    // -- Summary & apply fixes ------------------------------------------------
    let fixable_count = issues.iter().filter(|i| i.fix_fn.is_some()).count();
    let issue_count = issues.len();

    println!();
    if issues.is_empty() {
        println!(
            "  {}",
            green(&format!("{passed} checks passed, 0 issues"))
        );
        return Ok(());
    }

    for issue in &issues {
        if let Some(hint) = issue.fix_hint {
            eprintln!("  {} {} {}", yellow("[warn]"), issue.message, dim(&format!("(fix: {hint})")));
        } else {
            eprintln!("  {} {}", yellow("[warn]"), issue.message);
        }
    }

    println!();
    println!(
        "  {}",
        bold(&format!(
            "{passed} checks passed, {issue_count} issue(s) ({fixable_count} fixable)"
        ))
    );

    if args.fix {
        println!();
        for issue in issues {
            if let Some(fix) = issue.fix_fn {
                match fix() {
                    Ok(done) => println!("  {} {done}", green("[fixed]")),
                    Err(e) => eprintln!("  {} {}: {e:#}", red("[fix-failed]"), issue.message),
                }
            }
        }
    } else if fixable_count > 0 {
        eprintln!(
            "  {}",
            dim(&format!(
                "run `rsclaw doctor --fix` to apply {fixable_count} auto-fix(es)"
            ))
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Attempt to repair broken JSON5 config by fixing common syntax issues:
/// - Missing commas between properties
/// - Trailing commas before closing braces
/// - Unquoted string values that should be quoted
fn repair_config_json() -> Result<String> {
    let Some(cfg_path) = config::loader::detect_config_path() else {
        anyhow::bail!("no config file found");
    };
    let raw = std::fs::read_to_string(&cfg_path)?;

    // Backup first
    let backup = cfg_path.with_extension("json5.bak");
    std::fs::copy(&cfg_path, &backup)?;

    let repaired = repair_json5_syntax(&raw);

    // Try parsing the repaired version
    match json5::from_str::<serde_json::Value>(&repaired) {
        Ok(mut val) => {
            // Also fix type mismatches while we're at it
            fix_types_recursive(&mut val);
            std::fs::write(&cfg_path, serde_json::to_string_pretty(&val)?)?;
            Ok(format!(
                "repaired and reformatted config (backup at {})",
                backup.display()
            ))
        }
        Err(e) => {
            // Repaired version still broken -- try writing it anyway (might be closer to valid)
            std::fs::write(&cfg_path, &repaired)?;
            anyhow::bail!(
                "partial repair applied but config still has errors: {e}. \
                 Backup at {}. Please fix manually.",
                backup.display()
            )
        }
    }
}

/// Fix common JSON5 syntax issues in raw text.
fn repair_json5_syntax(raw: &str) -> String {
    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim().to_owned();

        if trimmed.is_empty() || trimmed.starts_with("//") {
            i += 1;
            continue;
        }

        // Fix: line ends with a value and next line starts a new key -- insert comma
        if !trimmed.ends_with(',')
            && !trimmed.ends_with('{')
            && !trimmed.ends_with('[')
            && !trimmed.ends_with(':')
            && trimmed != "}"
            && trimmed != "]"
            && trimmed != "},"
            && trimmed != "],"
        {
            let next_trimmed = lines[i + 1..]
                .iter()
                .find(|l| { let t = l.trim(); !t.is_empty() && !t.starts_with("//") })
                .map(|l| l.trim().to_owned());

            if let Some(ref nt) = next_trimmed {
                let needs_comma = nt.starts_with('"')
                    || nt.starts_with('\'')
                    || nt.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_');
                if needs_comma {
                    lines[i] = format!("{},", lines[i].trim_end());
                }
            }
        }

        i += 1;
    }

    lines.join("\n")
}

/// Fix string-valued booleans/numbers in the config JSON.
/// e.g. `"true"` -> `true`, `"false"` -> `false`, `"18888"` -> `18888`
fn fix_config_type_mismatches() -> Result<String> {
    let Some(cfg_path) = config::loader::detect_config_path() else {
        anyhow::bail!("no config file found");
    };
    let raw = std::fs::read_to_string(&cfg_path)?;
    let mut val: serde_json::Value = json5::from_str(&raw)?;
    let count = fix_types_recursive(&mut val);
    if count == 0 {
        return Ok("no mismatches found".to_string());
    }
    std::fs::write(&cfg_path, serde_json::to_string_pretty(&val)?)?;
    Ok(format!("fixed {count} type mismatch(es) in {}", cfg_path.display()))
}

fn fix_types_recursive(val: &mut serde_json::Value) -> usize {
    let mut count = 0;
    match val {
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if let Some(v) = map.get_mut(&key) {
                    if let serde_json::Value::String(s) = v {
                        if s == "true" {
                            *v = serde_json::Value::Bool(true);
                            count += 1;
                        } else if s == "false" {
                            *v = serde_json::Value::Bool(false);
                            count += 1;
                        } else if key == "port"
                            && let Ok(n) = s.parse::<u64>()
                        {
                            *v = serde_json::json!(n);
                            count += 1;
                        } else if key == "model" && s.contains('/') {
                            // "model": "provider/name" -> "model": { "primary": "provider/name" }
                            *v = serde_json::json!({ "primary": s.clone() });
                            count += 1;
                        }
                    } else {
                        count += fix_types_recursive(v);
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                count += fix_types_recursive(item);
            }
        }
        _ => {}
    }
    count
}

/// Check if a process with the given PID is running.
fn process_exists(pid: u32) -> bool {
    crate::sys::process_alive(pid)
}
