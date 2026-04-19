//! `rsclaw migrate` command handler.
//!
//! Reads OpenClaw JSONL session data and imports it into rsclaw's redb store.

use std::path::PathBuf;

use anyhow::{Result, bail};

use super::style::{banner, bold, dim, err_msg, green, item, kv, ok, warn_msg};
use crate::cli::MigrateArgs;
use crate::migrate::openclaw::{self, ImportStats};
use crate::migrate::{MigrateMode, detect_openclaw_dir};

const VERSION: &str = env!("RSCLAW_BUILD_VERSION");

pub async fn cmd_migrate(args: MigrateArgs) -> Result<()> {
    banner(&format!("rsclaw migrate v{VERSION}"));

    // Parse mode.
    let mode = MigrateMode::from_str_loose(&args.mode).unwrap_or_else(|| {
        warn_msg(&format!(
            "unknown mode '{}', defaulting to 'import'",
            args.mode
        ));
        MigrateMode::Import
    });

    kv("Mode:", mode.label());
    kv("Dry run:", if args.dry_run { "yes" } else { "no" });

    // Resolve OpenClaw directory.
    let oc_dir = if let Some(ref p) = args.openclaw_dir {
        let path = crate::config::loader::expand_tilde_path_pub(p);
        if !path.is_dir() {
            err_msg(&format!(
                "specified directory not found: {}",
                path.display()
            ));
            bail!("directory not found: {}", path.display());
        }
        Some(path)
    } else {
        detect_openclaw_dir()
    };

    match oc_dir {
        Some(ref p) => kv("OpenClaw dir:", &p.display().to_string()),
        None => kv("OpenClaw dir:", &dim("(not found)")),
    }

    let home = dirs_next::home_dir().unwrap_or_default();
    let openclaw_dir = oc_dir.unwrap_or_else(|| home.join(".openclaw"));
    let rsclaw_dir = std::env::var("RSCLAW_BASE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".rsclaw"));

    println!();

    // Execute based on mode.
    match mode {
        MigrateMode::Seamless => {
            println!("Seamless mode is only available via interactive setup (rsclaw setup)");
            println!("Use 'rsclaw setup' for seamless OpenClaw takeover");
        }
        MigrateMode::New => {
            handle_fresh(&rsclaw_dir, args.dry_run)?;
        }
        MigrateMode::Import => {
            handle_import(&openclaw_dir, &rsclaw_dir, args.dry_run)?;
        }
    }

    println!();
    ok(&format!("migration ({}) complete", green(mode.label())));
    Ok(())
}

// ---------------------------------------------------------------------------
// Mode handlers
// ---------------------------------------------------------------------------

fn handle_fresh(rsclaw_dir: &PathBuf, dry_run: bool) -> Result<()> {
    item("*", &format!("rsclaw will use {}", rsclaw_dir.display()));
    item("*", "OpenClaw data will be ignored");

    if !dry_run {
        std::fs::create_dir_all(rsclaw_dir)?;
        ok("created rsclaw data directory");
    } else {
        item("-", &format!("would create {}", rsclaw_dir.display()));
    }
    ok("fresh mode -- no data to migrate");
    Ok(())
}

fn handle_import(openclaw_dir: &PathBuf, rsclaw_dir: &PathBuf, dry_run: bool) -> Result<()> {
    if !openclaw_dir.is_dir() {
        err_msg("no OpenClaw data directory found to import from");
        err_msg("specify --openclaw-dir or ensure OpenClaw is installed");
        bail!("no OpenClaw directory found");
    }

    item(
        "*",
        &format!("will import data from {}", openclaw_dir.display()),
    );
    item(
        "*",
        &format!("into rsclaw stores at {}", rsclaw_dir.display()),
    );

    // Scan and show what we found.
    let scan = show_scan_results(openclaw_dir)?;

    let has_data = scan.total_sessions > 0
        || scan.total_memories > 0
        || scan.total_memory_md_files > 0
        || scan.total_memory_dbs > 0
        || scan.total_workspaces > 0
        || scan.total_skills > 0
        || scan.total_cron_jobs > 0;

    if !has_data {
        warn_msg("no data found to import");
        return Ok(());
    }

    if dry_run {
        println!();
        item("*", "Dry run -- no data was modified");
        return Ok(());
    }

    // Perform the actual import.
    import_data(openclaw_dir, rsclaw_dir)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn show_scan_results(openclaw_dir: &PathBuf) -> Result<openclaw::OpenClawScanResult> {
    let scan = openclaw::scan_openclaw(openclaw_dir)?;

    println!();
    item("*", "OpenClaw data detected:");
    kv("  Config:", if scan.has_config { "yes" } else { "no" });
    kv("  Agents:", &bold(&scan.agent_ids.len().to_string()));

    for agent_id in &scan.agent_ids {
        let count = scan.sessions_per_agent.get(agent_id).copied().unwrap_or(0);
        item(" ", &format!("agent '{}': {} session(s)", agent_id, count));
    }

    kv("  Sessions:", &bold(&scan.total_sessions.to_string()));
    kv("  JSONL files:", &scan.total_jsonl_files.to_string());
    kv("  Memory (JSONL):", &scan.total_memories.to_string());
    kv("  Memory (MEMORY.md):", &scan.total_memory_md_files.to_string());
    kv("  Memory (SQLite):", &scan.total_memory_dbs.to_string());
    kv("  Workspaces:", &scan.total_workspaces.to_string());
    kv("  Skills:", &scan.total_skills.to_string());
    kv("  Cron jobs:", &scan.total_cron_jobs.to_string());
    println!();

    Ok(scan)
}

/// Public entry point for setup.rs to call the unified import logic.
pub fn import_data_from(openclaw_dir: &std::path::Path, rsclaw_dir: &std::path::Path) -> Result<()> {
    import_data(&openclaw_dir.to_path_buf(), &rsclaw_dir.to_path_buf())
}

fn import_data(openclaw_dir: &PathBuf, rsclaw_dir: &PathBuf) -> Result<()> {
    use crate::store::redb_store::RedbStore;

    std::fs::create_dir_all(rsclaw_dir)?;
    let redb_dir = rsclaw_dir.join("var/data/redb");
    std::fs::create_dir_all(&redb_dir)?;

    let store = RedbStore::open(&redb_dir.join("data.redb"), crate::MemoryTier::Standard)?;

    let mut stats = openclaw::import_sessions_to_redb(openclaw_dir, &store)?;

    // --- Import workspace files and skills ---
    let config_path = openclaw_dir.join("openclaw.json");
    let config_raw = std::fs::read_to_string(&config_path).unwrap_or_default();
    let config: serde_json::Value = json5::from_str(&config_raw)
        .or_else(|_| serde_json::from_str(&config_raw))
        .unwrap_or_default();

    // Migrate config: extract compatible parts from openclaw.json -> rsclaw.json5
    let rsclaw_config = rsclaw_dir.join("rsclaw.json5");
    if config_path.is_file() && !rsclaw_config.exists() {
        let mut rsclaw_cfg = serde_json::Map::new();

        // Gateway: migrate compatible fields, set port to 18888.
        let mut gw = serde_json::Map::new();
        gw.insert("port".to_owned(), serde_json::json!(18888));
        if let Some(oc_gw) = config.get("gateway").and_then(|v| v.as_object()) {
            // Supported gateway fields to migrate as-is.
            let supported = ["bind", "auth", "language", "mode", "controlUi"];
            for key in &supported {
                if let Some(val) = oc_gw.get(*key) {
                    gw.insert(key.to_string(), val.clone());
                }
            }
        }
        rsclaw_cfg.insert("gateway".to_owned(), serde_json::Value::Object(gw));

        // Models: providers (fully compatible).
        if let Some(models) = config.get("models") {
            rsclaw_cfg.insert("models".to_owned(), models.clone());
        }

        // Agents: defaults + list (rewrite workspace paths, strip agentDir).
        if let Some(agents) = config.get("agents") {
            let mut agents_cfg = agents.clone();
            // Clean up defaults: keep only supported fields, set rsclaw defaults.
            if let Some(defaults) = agents_cfg.pointer_mut("/defaults") {
                if let Some(obj) = defaults.as_object_mut() {
                    // Remove openclaw-specific fields.
                    obj.remove("memorySearch");
                    // Set rsclaw defaults.
                    obj.insert("workspace".to_owned(),
                        serde_json::Value::String("~/.rsclaw/workspace".to_owned()));
                    obj.insert("compaction".to_owned(),
                        serde_json::json!({"mode": "layered"}));
                }
            }
            // Build agent->channels map from OpenClaw bindings.
            let mut agent_channels: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            if let Some(bindings) = config.get("bindings").and_then(|v| v.as_array()) {
                for binding in bindings {
                    let agent_id = binding.get("agentId").and_then(|v| v.as_str()).unwrap_or("");
                    let channel = binding.pointer("/match/channel").and_then(|v| v.as_str()).unwrap_or("");
                    let account = binding.pointer("/match/accountId").and_then(|v| v.as_str()).unwrap_or("");
                    if !agent_id.is_empty() && !channel.is_empty() {
                        let ch_key = if account.is_empty() {
                            channel.to_owned()
                        } else {
                            format!("{channel}:{account}")
                        };
                        agent_channels.entry(agent_id.to_owned()).or_default().push(ch_key);
                    }
                }
            }

            // Rewrite per-agent workspaces, strip agentDir, add channels from bindings.
            if let Some(list) = agents_cfg.pointer_mut("/list").and_then(|v| v.as_array_mut()) {
                for agent in list.iter_mut() {
                    if let Some(obj) = agent.as_object_mut() {
                        let agent_id = obj.get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("main")
                            .to_owned();
                        obj.remove("agentDir");
                        if obj.contains_key("workspace") {
                            obj.insert("workspace".to_owned(),
                                serde_json::Value::String(format!("~/.rsclaw/workspace-{agent_id}")));
                        }
                        // Inject channels from bindings.
                        if let Some(chs) = agent_channels.get(&agent_id) {
                            let ch_values: Vec<serde_json::Value> = chs.iter()
                                .map(|c| serde_json::Value::String(c.clone()))
                                .collect();
                            obj.insert("channels".to_owned(), serde_json::Value::Array(ch_values));
                        }
                    }
                }
            }
            rsclaw_cfg.insert("agents".to_owned(), agents_cfg);
        }

        // Channels: only migrate supported channel configs, skip installs/plugins.
        if let Some(channels) = config.get("channels").and_then(|v| v.as_object()) {
            let supported = [
                "telegram", "feishu", "dingtalk", "wechat", "wecom",
                "discord", "slack", "whatsapp", "signal", "qq",
                "line", "zalo", "matrix",
            ];
            let mut ch_cfg = serde_json::Map::new();
            for (name, val) in channels {
                if supported.iter().any(|s| name.starts_with(s)) {
                    ch_cfg.insert(name.clone(), val.clone());
                }
            }
            if !ch_cfg.is_empty() {
                rsclaw_cfg.insert("channels".to_owned(), serde_json::Value::Object(ch_cfg));
            }
        }

        // Session: only migrate dmScope (skip reset, installs, and other
        // unsupported options — rsclaw uses layered compaction instead).
        // Note: dmScope is not migrated either since rsclaw defaults to
        // per-channel-peer and uses session aliases for compatibility.

        let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(rsclaw_cfg))?;
        std::fs::write(&rsclaw_config, &pretty)?;
        item("*", "config migrated: openclaw.json -> rsclaw.json5");
        if config.get("bindings").is_some() {
            item(" ", "bindings converted to per-agent channels");
        }
    }

    let default_workspace = openclaw_dir.join("workspace");
    let rsclaw_default_workspace = rsclaw_dir.join("workspace");

    // Copy default workspace files + skills.
    match openclaw::copy_workspace_files(&default_workspace, &rsclaw_default_workspace) {
        Ok(n) => stats.workspace_files += n,
        Err(e) => {
            warn_msg(&format!("failed to copy default workspace: {e}"));
            stats.errors += 1;
        }
    }
    match openclaw::convert_heartbeat(&default_workspace, &rsclaw_default_workspace) {
        Ok(true) => stats.workspace_files += 1,
        Err(e) => {
            warn_msg(&format!("failed to convert HEARTBEAT.md: {e}"));
            stats.errors += 1;
        }
        _ => {}
    }
    match openclaw::copy_skills(&default_workspace, &rsclaw_default_workspace) {
        Ok(n) => stats.skills += n,
        Err(e) => {
            warn_msg(&format!("failed to copy skills: {e}"));
            stats.errors += 1;
        }
    }
    // Also copy skills to base_dir/skills/ (where gateway loads them from).
    match openclaw::copy_skills(&default_workspace, rsclaw_dir) {
        Ok(_) => {}
        Err(e) => {
            warn_msg(&format!("failed to copy skills to gateway dir: {e}"));
        }
    }

    // Copy per-agent workspaces.
    if let Some(agents) = config.pointer("/agents/list").and_then(|v| v.as_array()) {
        for agent in agents {
            let agent_id = agent.get("id").and_then(|v| v.as_str()).unwrap_or("main");
            let src_workspace = agent
                .get("workspace")
                .and_then(|v| v.as_str())
                .map(|p| {
                    let expanded = if let Some(rest) = p.strip_prefix("~/") {
                        dirs_next::home_dir().unwrap_or_default().join(rest)
                    } else {
                        std::path::PathBuf::from(p)
                    };
                    // Remap absolute paths from another machine to current openclaw dir.
                    if expanded.is_dir() {
                        expanded
                    } else if let Some(dirname) = expanded.file_name() {
                        let remapped = openclaw_dir.join(dirname);
                        if remapped.is_dir() { remapped } else { expanded }
                    } else {
                        expanded
                    }
                })
                .unwrap_or_else(|| default_workspace.clone());

            if src_workspace == default_workspace {
                continue; // Already handled above.
            }

            let dst_workspace = rsclaw_dir.join(format!("workspace-{agent_id}"));
            match openclaw::copy_workspace_files(&src_workspace, &dst_workspace) {
                Ok(n) => stats.workspace_files += n,
                Err(e) => {
                    warn_msg(&format!("failed to copy workspace for {agent_id}: {e}"));
                    stats.errors += 1;
                }
            }
            match openclaw::convert_heartbeat(&src_workspace, &dst_workspace) {
                Ok(true) => stats.workspace_files += 1,
                Err(e) => {
                    warn_msg(&format!("failed to convert HEARTBEAT.md for {agent_id}: {e}"));
                    stats.errors += 1;
                }
                _ => {}
            }
        }
    }

    // Fallback: scan all workspace-* directories in openclaw dir.
    // Catches workspaces not listed in config (e.g. commander with agentDir only).
    if let Ok(entries) = std::fs::read_dir(openclaw_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if path.is_dir() && name.starts_with("workspace-") {
                let dst = rsclaw_dir.join(&name);
                if !dst.exists() {
                    match openclaw::copy_workspace_files(&path, &dst) {
                        Ok(n) => stats.workspace_files += n,
                        Err(e) => {
                            warn_msg(&format!("failed to copy {name}: {e}"));
                            stats.errors += 1;
                        }
                    }
                }
                match openclaw::convert_heartbeat(&path, &dst) {
                    Ok(true) => stats.workspace_files += 1,
                    Err(e) => {
                        warn_msg(&format!("failed to convert HEARTBEAT.md for {name}: {e}"));
                        stats.errors += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    // --- Import memories into memory.redb ---
    let memory_entries = openclaw::collect_all_memories(openclaw_dir, &config_raw)?;
    if !memory_entries.is_empty() {
        import_memories_to_redb(&memory_entries, rsclaw_dir, &mut stats)?;
    }

    // --- Copy cron jobs (openclaw cron/jobs.json -> rsclaw cron.json5) ---
    let cron_src = openclaw_dir.join("cron/jobs.json");
    if cron_src.is_file() {
        let cron_dst = rsclaw_dir.join("cron.json5");
        if !cron_dst.exists() {
            std::fs::copy(&cron_src, &cron_dst)?;
            // Count jobs for stats.
            if let Ok(data) = std::fs::read_to_string(&cron_dst) {
                let count = serde_json::from_str::<serde_json::Value>(&data)
                    .ok()
                    .and_then(|v| v["jobs"].as_array().map(|a| a.len()))
                    .unwrap_or(0);
                item("*", &format!("{count} cron job(s) migrated"));
            }
        } else {
            item(" ", "cron.json5 already exists, skipping");
        }
    }

    // --- Copy cron run history ---
    let cron_runs_src = openclaw_dir.join("cron/runs");
    if cron_runs_src.is_dir() {
        let cron_runs_dst = rsclaw_dir.join("var/data/cron");
        std::fs::create_dir_all(&cron_runs_dst)?;
        if let Ok(entries) = std::fs::read_dir(&cron_runs_src) {
            let mut count = 0;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    let dst = cron_runs_dst.join(path.file_name().unwrap());
                    if !dst.exists() {
                        let _ = std::fs::copy(&path, &dst);
                        count += 1;
                    }
                }
            }
            if count > 0 {
                item("*", &format!("{count} cron run log(s) migrated"));
            }
        }
    }

    // --- Migrate feishu allowFrom to channel config ---
    let allow_from_path = openclaw_dir.join("credentials/feishu-main-bot-allowFrom.json");
    if allow_from_path.is_file() {
        if let Ok(data) = std::fs::read_to_string(&allow_from_path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
                if let Some(allow_from) = val.get("allowFrom").and_then(|v| v.as_array()) {
                    if !allow_from.is_empty() {
                        item("*", &format!("{} feishu allowFrom entries found (add to channels.feishu.allowFrom in config)", allow_from.len()));
                    }
                }
            }
        }
    }

    println!();
    print_import_stats(&stats);

    ok("data imported into rsclaw stores");
    Ok(())
}

fn import_memories_to_redb(
    entries: &[openclaw::MemoryEntry],
    rsclaw_dir: &std::path::Path,
    stats: &mut openclaw::ImportStats,
) -> Result<()> {
    use crate::agent::memory::MemoryDoc;

    let mem_dir = rsclaw_dir.join("var/data/memory");
    std::fs::create_dir_all(&mem_dir)?;

    let db_path = mem_dir.join("memory.redb");
    let db = redb::Database::create(&db_path)?;

    let table_def: redb::TableDefinition<&str, &[u8]> =
        redb::TableDefinition::new("memory_docs");

    // Ensure table exists.
    {
        let write = db.begin_write()?;
        let _ = write.open_table(table_def)?;
        write.commit()?;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    for entry in entries {
        let doc = MemoryDoc {
            id: format!("oc:{}:{}", entry.agent_id, md5_short(&entry.content)),
            scope: entry.agent_id.clone(),
            kind: "note".to_owned(),
            text: format!("## {}\n\n{}", entry.title, entry.content),
            vector: Vec::new(), // Will be embedded on next startup.
            created_at: now,
            accessed_at: now,
            access_count: 0,
            importance: 0.6, // Slightly above default for imported memories.
            tier: Default::default(),
            abstract_text: None,
            overview_text: None,
            tags: vec![],
                pinned: false,
        };

        let json_bytes = serde_json::to_vec(&doc)?;
        // Storage format: 4 bytes vec_len (0, no vector yet) + json bytes.
        let mut data = Vec::with_capacity(4 + json_bytes.len());
        data.extend_from_slice(&0u32.to_le_bytes()); // vec_len = 0
        data.extend_from_slice(&json_bytes);

        let write = db.begin_write()?;
        {
            let mut table = write.open_table(table_def)?;
            table.insert(doc.id.as_str(), data.as_slice())?;
        }
        write.commit()?;
        stats.memories += 1;
    }

    item("*", &format!("{} memory entries imported (embedding on next startup)", stats.memories));
    Ok(())
}

fn md5_short(text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn print_import_stats(stats: &ImportStats) {
    item("*", "Import results:");
    kv("  Sessions:", &bold(&stats.sessions.to_string()));
    kv("  Messages:", &bold(&stats.messages.to_string()));
    kv(
        "  Memories:",
        &format!(
            "{} (embedding on next startup)",
            bold(&stats.memories.to_string())
        ),
    );
    kv("  Workspace files:", &bold(&stats.workspace_files.to_string()));
    kv("  Skills:", &bold(&stats.skills.to_string()));
    if stats.aliases > 0 {
        kv("  Session aliases:", &bold(&stats.aliases.to_string()));
    }
    if stats.errors > 0 {
        warn_msg(&format!("{} errors during import", stats.errors));
    }
    println!();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// Extracts the binding->channels conversion logic for testing.
    fn convert_bindings_to_agent_channels(config: &serde_json::Value) -> serde_json::Value {
        let mut agents_cfg = config.get("agents").cloned().unwrap_or_default();

        // Build agent->channels map from OpenClaw bindings.
        let mut agent_channels: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        if let Some(bindings) = config.get("bindings").and_then(|v| v.as_array()) {
            for binding in bindings {
                let agent_id = binding.get("agentId").and_then(|v| v.as_str()).unwrap_or("");
                let channel = binding.pointer("/match/channel").and_then(|v| v.as_str()).unwrap_or("");
                let account = binding.pointer("/match/accountId").and_then(|v| v.as_str()).unwrap_or("");
                if !agent_id.is_empty() && !channel.is_empty() {
                    let ch_key = if account.is_empty() {
                        channel.to_owned()
                    } else {
                        format!("{channel}:{account}")
                    };
                    agent_channels.entry(agent_id.to_owned()).or_default().push(ch_key);
                }
            }
        }

        // Inject channels from bindings into agents list.
        if let Some(list) = agents_cfg.pointer_mut("/list").and_then(|v| v.as_array_mut()) {
            for agent in list.iter_mut() {
                if let Some(obj) = agent.as_object_mut() {
                    let agent_id = obj.get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("main")
                        .to_owned();
                    if let Some(chs) = agent_channels.get(&agent_id) {
                        let ch_values: Vec<serde_json::Value> = chs.iter()
                            .map(|c| serde_json::Value::String(c.clone()))
                            .collect();
                        obj.insert("channels".to_owned(), serde_json::Value::Array(ch_values));
                    }
                }
            }
        }

        agents_cfg
    }

    #[test]
    fn bindings_convert_bare_channel() {
        let config = json!({
            "agents": {
                "list": [
                    { "id": "main", "default": true },
                    { "id": "tg_bot" }
                ]
            },
            "bindings": [
                { "agentId": "tg_bot", "match": { "channel": "telegram" } }
            ]
        });

        let result = convert_bindings_to_agent_channels(&config);
        let tg_bot = &result["list"][1];
        assert_eq!(tg_bot["channels"], json!(["telegram"]));
        // main has no bindings, so no channels field.
        assert!(result["list"][0].get("channels").is_none());
    }

    #[test]
    fn bindings_convert_channel_with_account() {
        let config = json!({
            "agents": {
                "list": [
                    { "id": "main", "default": true },
                    { "id": "sales" },
                    { "id": "support" }
                ]
            },
            "bindings": [
                {
                    "agentId": "sales",
                    "match": { "channel": "feishu", "accountId": "sales-bot" }
                },
                {
                    "agentId": "support",
                    "match": { "channel": "feishu", "accountId": "support-bot" }
                },
                {
                    "agentId": "support",
                    "match": { "channel": "dingtalk" }
                }
            ]
        });

        let result = convert_bindings_to_agent_channels(&config);
        let sales = &result["list"][1];
        let support = &result["list"][2];
        assert_eq!(sales["channels"], json!(["feishu:sales-bot"]));
        assert_eq!(support["channels"], json!(["feishu:support-bot", "dingtalk"]));
    }

    #[test]
    fn bindings_skip_invalid_entries() {
        let config = json!({
            "agents": {
                "list": [
                    { "id": "main", "default": true }
                ]
            },
            "bindings": [
                { "agentId": "", "match": { "channel": "telegram" } },
                { "agentId": "main", "match": { "channel": "" } },
                { "agentId": "main", "match": {} }
            ]
        });

        let result = convert_bindings_to_agent_channels(&config);
        // All invalid, so main should have no channels.
        assert!(result["list"][0].get("channels").is_none());
    }
}
