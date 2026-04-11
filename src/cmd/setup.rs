use anyhow::Result;
use dialoguer::{Input, Password, Select};
use serde_json::json;

use super::config_json::{get_nested_value, load_config_json, set_nested_value};
use crate::{
    agent,
    cli::{ConfigureArgs, OnboardArgs, SetupArgs},
};

// ---------------------------------------------------------------------------
// Wizard step helpers (ESC-to-go-back support)
// ---------------------------------------------------------------------------

enum StepResult<T> {
    Next(T),
    Back,
    Cancel,
}

fn select_step(prompt: &str, items: &[&str], default: usize) -> StepResult<usize> {
    match Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(prompt)
        .items(items)
        .default(default)
        .interact_opt()
    {
        Ok(Some(idx)) => StepResult::Next(idx),
        Ok(None) => StepResult::Back,
        Err(_) => StepResult::Cancel,
    }
}

fn input_step<T>(prompt: &str, default: T) -> StepResult<T>
where
    T: Clone + ToString + std::str::FromStr,
    <T as std::str::FromStr>::Err: std::fmt::Debug + std::fmt::Display,
{
    // Show current value, let user choose: edit or keep (ESC = back)
    let current = default.to_string();
    if !current.is_empty() {
        let display_val = if current.len() > 50 {
            format!("{}...", &current[..47])
        } else {
            current.clone()
        };
        let lang = crate::i18n::default_lang();
        let keep_label = crate::i18n::t_fmt("cli_keep", lang, &[("value", &display_val)]);
        let edit_label = crate::i18n::t("cli_edit", lang);
        let back_label = crate::i18n::t("cli_back", lang);
        let items = &[
            keep_label.as_str(),
            edit_label.as_str(),
            back_label.as_str(),
        ];
        match Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt(prompt)
            .items(items)
            .default(0)
            .interact_opt()
        {
            Ok(Some(0)) => return StepResult::Next(default), // Keep current
            Ok(Some(2)) | Ok(None) => return StepResult::Back,
            Ok(Some(1)) => {} // Fall through to input
            _ => return StepResult::Cancel,
        }
    }

    match Input::<T>::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(prompt)
        .default(default)
        .interact_text()
    {
        Ok(val) => StepResult::Next(val),
        Err(_) => StepResult::Back,
    }
}

fn password_step(prompt: &str) -> StepResult<String> {
    match Password::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(prompt)
        .allow_empty_password(true)
        .interact()
    {
        Ok(val) => StepResult::Next(val),
        Err(_) => StepResult::Back,
    }
}


fn confirm_step(prompt: &str, default: bool) -> StepResult<bool> {
    match dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(prompt)
        .default(default)
        .interact_opt()
    {
        Ok(Some(val)) => StepResult::Next(val),
        Ok(None) => StepResult::Back,
        Err(_) => StepResult::Cancel,
    }
}

fn default_config(lang: &str) -> String {
    let lang_name = lang_code_to_name(lang);
    format!(
        r#"// rsclaw configuration (JSON5)
// Docs: https://github.com/rsclaw-ai/rsclaw
{{
  gateway: {{
    port: 18888,
    bind: "loopback",
    language: "{lang_name}",
  }},
  models: {{
    providers: {{
      anthropic: {{ apiKey: "${{ANTHROPIC_API_KEY}}" }},
      // openai: {{ apiKey: "${{OPENAI_API_KEY}}" }},
    }},
  }},
  agents: {{
    list: [
      {{
        id: "main",
        default: true,
        // workspace defaults to $base_dir/workspace
        model: {{ primary: "anthropic/claude-sonnet-4-6" }},
      }},
    ],
  }},
  // channels: {{
  //   telegram: {{ botToken: "${{TELEGRAM_BOT_TOKEN}}" }},
  //   discord: {{ token: "${{DISCORD_BOT_TOKEN}}" }},
  // }},
}}
"#
    )
}

// ---------------------------------------------------------------------------
// Styled output helpers
// ---------------------------------------------------------------------------

/// Recursively copy a directory tree (files only, skips symlinks).
#[allow(dead_code)]
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let dest = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else if ft.is_file() {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn header(title: &str) {
    println!();
    println!("  {title}");
    println!("  {}", "-".repeat(title.len()));
}

fn step(icon: &str, msg: &str) {
    println!("  {icon} {msg}");
}

fn done(msg: &str) {
    println!();
    println!("  [ok] {msg}");
}

fn hint(msg: &str) {
    println!("       {msg}");
}

/// Language selection prompt -- shown as the very first step in setup/onboard.
/// Returns the resolved language code and sets it as the i18n default.
fn select_language() -> Result<&'static str> {
    let labels = [
        "中文 (Chinese)",
        "English",
        "Francais (French)",
        "Deutsch (German)",
        "日本語 (Japanese)",
        "한국어 (Korean)",
        "Espanol (Spanish)",
        "Русский (Russian)",
        "ไทย (Thai)",
        "Tieng Viet (Vietnamese)",
    ];
    let codes: [&str; 10] = ["zh", "en", "fr", "de", "ja", "ko", "es", "ru", "th", "vi"];

    println!();
    println!("  Language / 语言");
    println!("  ----------------");

    let selection = Select::new()
        .items(&labels)
        .default(0) // Chinese as default
        .interact_opt()?;

    let idx = selection.unwrap_or(0);
    let lang = codes[idx];
    crate::i18n::set_default_lang(lang);
    Ok(lang)
}

/// Detect LAN IP addresses (non-loopback, non-link-local IPv4).
fn detect_lan_ips() -> Vec<String> {
    let mut ips = Vec::new();

    if cfg!(windows) {
        // Windows: parse ipconfig output
        if let Ok(output) = std::process::Command::new("ipconfig").output() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                let trimmed = line.trim();
                // Match "IPv4 Address. . . . . . . . . . . : 192.168.x.x"
                if let Some(pos) = trimmed.find(": ") {
                    let ip = trimmed[pos + 2..].trim();
                    if ip.contains('.') && !ip.starts_with("127.") && !ip.starts_with("169.254.") {
                        ips.push(ip.to_owned());
                    }
                }
            }
        }
    } else {
        // macOS: ifconfig
        if let Ok(output) = std::process::Command::new("ifconfig").output() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("inet ") {
                    let ip = rest.split_whitespace().next().unwrap_or("");
                    if !ip.starts_with("127.") && !ip.starts_with("169.254.") && !ip.is_empty() {
                        ips.push(ip.to_owned());
                    }
                }
            }
        }
        // Fallback for Linux: ip addr
        if ips.is_empty() {
            if let Ok(output) = std::process::Command::new("ip").args(["addr", "show"]).output() {
                let text = String::from_utf8_lossy(&output.stdout);
                for line in text.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("inet ") {
                        let ip = rest.split('/').next().unwrap_or("");
                        if !ip.starts_with("127.") && !ip.starts_with("169.254.") && !ip.is_empty() {
                            ips.push(ip.to_owned());
                        }
                    }
                }
            }
        }
    }
    ips
}

/// Build bind mode labels with auto-detected LAN IPs.
/// Returns (labels, bind_values) where bind_values[i] is the config value for selection i.
/// LAN IPs bind to the specific IP address, not 0.0.0.0.
fn build_bind_options() -> (Vec<String>, Vec<String>) {
    let mut labels = vec![
        "loopback (127.0.0.1 only)".to_string(),
        "all (0.0.0.0, public)".to_string(),
    ];
    let mut values = vec![
        "loopback".to_string(),
        "all".to_string(),
    ];

    let lan_ips = detect_lan_ips();
    for ip in &lan_ips {
        labels.push(format!("LAN: {ip}"));
        values.push(ip.clone()); // Bind to specific LAN IP
    }

    (labels, values)
}

/// Map a language code to a human-readable name for config storage.
fn lang_code_to_name(code: &str) -> &'static str {
    match code {
        "zh" => "Chinese",
        "fr" => "French",
        "de" => "German",
        "ja" => "Japanese",
        "ko" => "Korean",
        "es" => "Spanish",
        "ru" => "Russian",
        "th" => "Thai",
        "vi" => "Vietnamese",
        _ => "English",
    }
}

// ---------------------------------------------------------------------------
// Provider / channel definitions 鈥?loaded from defaults.toml
// ---------------------------------------------------------------------------

/// Compiled-in fallback (shipped in the binary).
fn builtin_defaults() -> String {
    crate::config::loader::load_defaults_toml()
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ProviderDef {
    name: String,
    label: String,
    #[serde(default)]
    env_var: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    user_agent: String,
    #[serde(default)]
    needs_key: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ChannelFieldDef {
    key: String,
    prompt: String,
    #[serde(default)]
    secret: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ChannelDef {
    name: String,
    label: String,
    #[serde(default)]
    fields: Vec<ChannelFieldDef>,
    /// If true, run `channels login` flow (QR/OAuth) instead of prompting fields.
    #[serde(default)]
    login: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct SearchEngineDef {
    name: String,
    label: String,
    url: String,
    #[serde(default)]
    env_var: String,
}

#[derive(Debug, serde::Deserialize)]
struct Defaults {
    #[serde(default)]
    providers: Vec<ProviderDef>,
    #[serde(default)]
    channels: Vec<ChannelDef>,
    #[serde(default)]
    search_engines: Vec<SearchEngineDef>,
}

/// Load defaults: user's `~/.rsclaw/defaults.toml` with built-in fallback.
///
/// Users can edit `defaults.toml` directly to customize providers, channels,
/// and search engines. If the file doesn't exist, built-in defaults are used.
fn load_defaults() -> Defaults {
    let builtin: Defaults =
        toml::from_str(&builtin_defaults()).expect("built-in defaults.toml is invalid");

    let user_path = crate::config::loader::base_dir().join("defaults.toml");
    if let Ok(content) = std::fs::read_to_string(&user_path)
        && let Ok(user) = toml::from_str::<Defaults>(&content)
    {
        Defaults {
            providers: if user.providers.is_empty() {
                builtin.providers
            } else {
                user.providers
            },
            channels: if user.channels.is_empty() {
                builtin.channels
            } else {
                user.channels
            },
            search_engines: if user.search_engines.is_empty() {
                builtin.search_engines
            } else {
                user.search_engines
            },
        }
    } else {
        builtin
    }
}

// ---------------------------------------------------------------------------
// Helpers: load existing config defaults
// ---------------------------------------------------------------------------

struct ExistingConfig {
    agent_name: String,
    provider_idx: usize,
    api_key_display: String,
    base_url: String,
    model: String,
    port: u16,
    bind_idx: usize,
    enabled_channels: Vec<String>,
    /// Per-provider model map: provider_name -> last used model string
    provider_models: std::collections::HashMap<String, String>,
}

fn load_existing_defaults(defs: &Defaults) -> ExistingConfig {
    let mut ec = ExistingConfig {
        agent_name: "main".into(),
        provider_idx: 0,
        api_key_display: String::new(),
        base_url: String::new(),
        model: String::new(),
        port: 18888,
        bind_idx: 0,
        enabled_channels: vec![],
        provider_models: std::collections::HashMap::new(),
    };

    let Ok((_, val)) = load_config_json() else {
        return ec;
    };

    // Port
    if let Some(p) = get_nested_value(&val, "gateway.port").and_then(|v| v.as_u64()) {
        ec.port = p as u16;
    }

    // Bind
    let bind_options = ["loopback", "all"];
    if let Some(b) = get_nested_value(&val, "gateway.bind").and_then(|v| v.as_str().map(|s| s.to_owned())) {
        ec.bind_idx = bind_options.iter().position(|&x| x == b).unwrap_or(0);
    }

    // Load model early so we can use its prefix to detect the active provider
    if let Some(arr) = val.get("agents").and_then(|a| a.get("list")).and_then(|l| l.as_array()) {
        if let Some(first) = arr.first() {
            if let Some(id) = first.get("id").and_then(|v| v.as_str()) {
                ec.agent_name = id.to_owned();
            }
            if let Some(m) = first.get("model").and_then(|m| m.get("primary")).and_then(|p| p.as_str()) {
                ec.model = m.to_owned();
            }
        }
    }
    if ec.model.is_empty() {
        if let Some(m) = get_nested_value(&val, "agents.defaults.model.primary").and_then(|v| v.as_str().map(|s| s.to_owned())) {
            ec.model = m;
        }
    }

    // Provider: determine from model prefix first, then from config providers
    let model_provider_prefix = ec.model.split('/').next().unwrap_or("").to_owned();
    if let Some(obj) = get_nested_value(&val, "models.providers").and_then(|v| v.as_object().cloned()) {
        // First try: match by model's provider/ prefix
        let pos = if !model_provider_prefix.is_empty() {
            defs.providers.iter().position(|p| p.name == model_provider_prefix && obj.contains_key(&p.name))
        } else {
            None
        };
        // Fallback: first provider in defs that exists in config
        let pos = pos.or_else(|| defs.providers.iter().position(|p| obj.contains_key(&p.name)));
        if let Some(pos) = pos {
            ec.provider_idx = pos;
            let prov = &defs.providers[pos];
            // Try to read the API key display
            let key_path = format!("models.providers.{}.apiKey", prov.name);
            if let Some(k) = get_nested_value(&val, &key_path).and_then(|v| v.as_str().map(|s| s.to_owned())) {
                if k.starts_with("${") {
                    ec.api_key_display = k;
                } else if k.len() > 8 {
                    ec.api_key_display = format!("{}...{}", &k[..4], &k[k.len() - 4..]);
                } else if !k.is_empty() {
                    ec.api_key_display = "*".repeat(k.len().min(20));
                }
            }
            // Try to read the base URL
            let url_path = format!("models.providers.{}.baseUrl", prov.name);
            if let Some(u) = get_nested_value(&val, &url_path).and_then(|v| v.as_str().map(|s| s.to_owned())) {
                ec.base_url = u;
            }
        }
    }

    // Build per-provider model map from agents.defaults.models (openclaw compat)
    // and from the current model prefix
    if let Some(models_obj) = val.pointer("/agents/defaults/models").and_then(|v| v.as_object()) {
        for (model_key, _) in models_obj {
            if let Some((prov, _)) = model_key.split_once('/') {
                ec.provider_models.insert(prov.to_owned(), model_key.clone());
            }
        }
    }
    // Also index from models.providers keys + current model
    if !ec.model.is_empty() {
        if let Some((prov, _)) = ec.model.split_once('/') {
            ec.provider_models.insert(prov.to_owned(), ec.model.clone());
        }
    }

    // Enabled channels
    if let Some(ch_obj) = val.get("channels").and_then(|v| v.as_object()) {
        for (name, _) in ch_obj {
            ec.enabled_channels.push(name.clone());
        }
    }

    ec
}

// ---------------------------------------------------------------------------
// rsclaw setup
// ---------------------------------------------------------------------------

pub async fn cmd_setup(args: SetupArgs) -> Result<()> {
    if args.wizard {
        return cmd_onboard(OnboardArgs::default()).await;
    }

    // Non-interactive: create directory structure and empty config, then exit.
    if args.non_interactive {
        let base = crate::config::loader::base_dir();
        std::fs::create_dir_all(&base)?;
        let config_path = base.join("rsclaw.json5");
        if !config_path.exists() {
            std::fs::write(&config_path, "{}\n")?;
        }
        for dir in &[
            "var/data/redb", "var/data/search", "var/data/memory", "var/data/cron",
            "var/run", "var/logs", "var/cache",
            "skills", "models", "plugins", "workspace",
        ] {
            let _ = std::fs::create_dir_all(base.join(dir));
        }
        let defaults_path = base.join("defaults.toml");
        if !defaults_path.exists() {
            let _ = std::fs::write(&defaults_path, &builtin_defaults());
        }
        // Seed workspace with default SOUL.md, AGENTS.md, USER.md
        let workspace = base.join("workspace");
        let _ = crate::agent::bootstrap::seed_workspace(&workspace);
        return Ok(());
    }

    // Language selection as the very first step.
    let lang = select_language()?;
    crate::i18n::set_default_lang(lang);

    header(&crate::i18n::t("cli_setup_title", lang));

    // Check for existing OpenClaw installation
    let home = dirs_next::home_dir().unwrap_or_default();
    let openclaw_dir = home.join(".openclaw");
    let openclaw_config = openclaw_dir.join("openclaw.json");

    // Detect OpenClaw installation and offer migration options.
    let mut session_count = 0usize;
    let migrate_mode = if openclaw_config.exists()
        && std::env::var("RSCLAW_BASE_DIR").is_err()
    {
        // Scan for data summary.
        let scan = crate::migrate::openclaw::scan_openclaw(&openclaw_dir).ok();
        session_count = scan.as_ref().map(|s| s.total_sessions).unwrap_or(0);
        let jsonl_count = scan.as_ref().map(|s| s.total_jsonl_files).unwrap_or(0);
        let agent_count = scan.as_ref().map(|s| s.agent_ids.len()).unwrap_or(0);

        step("*", &crate::i18n::t_fmt("cli_detected_openclaw", crate::i18n::default_lang(), &[("path", &openclaw_dir.display().to_string())]));
        if session_count > 0 {
            let lang = crate::i18n::default_lang();
            step(" ", &format!("  {}", crate::i18n::t_fmt("cli_data_summary", lang, &[
                ("agents", &agent_count.to_string()),
                ("sessions", &session_count.to_string()),
                ("jsonl", &jsonl_count.to_string()),
            ])));
        }
        println!();

        let lang = crate::i18n::default_lang();
        let import_desc = crate::i18n::t("cli_import_desc", lang);
        let fresh_desc = crate::i18n::t("cli_fresh_desc", lang);
        let options: Vec<&str> = vec![import_desc.as_str(), fresh_desc.as_str()];
        let migration_prompt = crate::i18n::t("cli_migration_mode", lang);
        match select_step(&migration_prompt, &options, 0) {
            StepResult::Next(0) => Some(crate::migrate::MigrateMode::Import),
            _ => Some(crate::migrate::MigrateMode::New),
        }
    } else {
        None
    };

    let base = {
        let b = crate::config::loader::base_dir();
        let lang = crate::i18n::default_lang();
        if migrate_mode == Some(crate::migrate::MigrateMode::Import) {
            step("+", &crate::i18n::t_fmt("cli_import_data_to", lang, &[("path", &b.display().to_string())]));
        } else {
            step("*", &crate::i18n::t_fmt("cli_using_dir", lang, &[("path", &b.display().to_string())]));
        }
        b
    };

    for dir in &[
        "var/data/redb",
        "var/data/search",
        "var/data/memory",
        "var/data/cron",
        "var/run",
        "var/logs",
        "var/cache",
        "skills",
        "models",
        "plugins",
    ] {
        let path = base.join(dir);
        std::fs::create_dir_all(&path)?;
        step("+", &format!("{}", path.display()));
    }

    // Import data from OpenClaw when user chose Import mode.
    // Delegates to the unified import_data() in cmd/migrate.rs.
    if migrate_mode == Some(crate::migrate::MigrateMode::Import) {
        step("*", &crate::i18n::t_fmt("cli_importing_sessions", crate::i18n::default_lang(), &[("count", &session_count.to_string())]));
        match super::migrate::import_data_from(&openclaw_dir, &base) {
            Ok(()) => {
                step("+", &crate::i18n::t("cli_converted_config", crate::i18n::default_lang()));
            }
            Err(e) => {
                step("!", &crate::i18n::t_fmt("cli_import_failed", crate::i18n::default_lang(), &[("err", &e.to_string())]));
            }
        }
    }

    let config_path = {
        let p = base.join("rsclaw.json5");
        if !p.exists() {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, default_config(lang))?;
            step("+", &format!("{}", p.display()));
        } else {
            step("=", &format!("{} (exists)", p.display()));
        }
        p
    };
    if config_path.exists() {
        step("=", &format!("{} (config)", config_path.display()));
    }

    let defaults_path = base.join("defaults.toml");
    if defaults_path.exists() {
        step("=", &format!("{} (exists)", defaults_path.display()));
    } else {
        std::fs::write(&defaults_path, &builtin_defaults())?;
        step("+", &format!("{}", defaults_path.display()));
    }

    // Seed workspace templates using the language selected at the start.
    let ws_lang = if lang == "zh" { Some("Chinese") } else { None };
    let workspace = base.join("workspace");
    let seeded = agent::seed_workspace_with_lang(&workspace, ws_lang)?;
    if seeded > 0 {
        step(
            "+",
            &crate::i18n::t_fmt("cli_workspace_seeded", lang, &[
                ("count", &seeded.to_string()),
                ("path", &workspace.display().to_string()),
            ]),
        );
    }

    // Language is already written into the config template via default_config(lang).
    step("+", &crate::i18n::t_fmt("cli_gateway_language_set", lang, &[("lang", lang_code_to_name(lang))]));

    let lang_final = crate::i18n::default_lang();
    done(&crate::i18n::t("cli_setup_complete", lang_final));
    println!();
    if migrate_mode == Some(crate::migrate::MigrateMode::Import) {
        // Migration done: config already exists, just start gateway
        hint(&crate::i18n::t("cli_then_start", lang_final));
    } else {
        // Fresh install: run onboard wizard to configure providers/channels
        hint(&crate::i18n::t_fmt("cli_edit_config", lang_final, &[("path", &config_path.display().to_string())]));
        hint(if lang_final == "zh" { "rsclaw onboard" } else { "rsclaw onboard" });
    }
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// rsclaw onboard
// ---------------------------------------------------------------------------

pub async fn cmd_onboard(_args: OnboardArgs) -> Result<()> {
    // Language selection as the very first step.
    // Skip if language was already configured (e.g. via `rsclaw setup`).
    let lang = {
        let configured_lang = crate::config::load()
            .ok()
            .and_then(|c| c.raw.gateway.as_ref().and_then(|g| g.language.clone()));
        if let Some(ref l) = configured_lang {
            let resolved = crate::i18n::resolve_lang(l);
            crate::i18n::set_default_lang(resolved);
            resolved
        } else {
            select_language()?
        }
    };

    println!();
    let wiz_title = crate::i18n::t("cli_setup_wizard_title", lang);
    println!("  {wiz_title}");
    println!("  {}", "=".repeat(wiz_title.len()));
    println!();
    hint(&crate::i18n::t("cli_press_esc_back", lang));

    let defs = load_defaults();
    let ec = load_existing_defaults(&defs);

    let provider_labels: Vec<&str> = defs.providers.iter().map(|p| p.label.as_str()).collect();

    let mut agent_name = ec.agent_name;
    let mut provider_idx = ec.provider_idx;
    let mut api_key = String::new();
    let mut base_url = ec.base_url;
    let mut default_model = ec.model;
    let mut port = ec.port;
    let mut bind_mode = ec.bind_idx;
    let mut channel_configs: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut custom_bind: Option<String> = None;
    let mut api_type = String::from("openai"); // API protocol type for custom/codingplan
    let mut user_agent = String::new();         // custom User-Agent header

    const STEP_AGENT: usize = 0;
    const STEP_PROVIDER: usize = 1;
    const STEP_API_TYPE: usize = 10;  // API protocol selection (custom/codingplan only)
    const STEP_BASE_URL: usize = 2;
    const STEP_USER_AGENT: usize = 11; // User-Agent header (custom/codingplan only)
    const STEP_API_KEY: usize = 3;
    const STEP_MODEL: usize = 4;
    const STEP_PORT: usize = 5;
    const STEP_BIND: usize = 6;
    const STEP_CHANNELS: usize = 7;
    const STEP_DONE: usize = 99;

    let mut wiz_step: usize = STEP_AGENT;

    'outer: loop {
        match wiz_step {
            STEP_AGENT => {
                header(&crate::i18n::t("cli_step_agent", lang));
                let agent_prompt = crate::i18n::t("cli_agent_name", lang);
                match input_step(&format!("  {agent_prompt}"), agent_name.clone()) {
                    StepResult::Next(val) => { agent_name = val; wiz_step = STEP_PROVIDER; }
                    StepResult::Back | StepResult::Cancel => {
                        println!("  {}", crate::i18n::t("cli_setup_cancelled", lang));
                        return Ok(());
                    }
                }
            }
            STEP_PROVIDER => {
                header(&crate::i18n::t("cli_step_model_provider", lang));
                let choose_prov = crate::i18n::t("cli_choose_provider", lang);
                match select_step(&format!("  {choose_prov}"), &provider_labels, provider_idx) {
                    StepResult::Next(idx) => {
                        provider_idx = idx;
                        let prov = &defs.providers[idx];
                        if prov.name == "custom" || prov.name == "codingplan" {
                            wiz_step = STEP_API_TYPE;
                        } else {
                            wiz_step = STEP_BASE_URL;
                        }
                    }
                    StepResult::Back => { wiz_step = STEP_AGENT; }
                    StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                }
            }
            STEP_API_TYPE => {
                // API protocol selection for custom/codingplan providers.
                let api_labels = &[
                    "OpenAI Chat (default)",
                    "OpenAI Responses",
                    "Anthropic",
                    "Google Gemini",
                    "Ollama",
                ];
                let api_values = &["openai", "openai-responses", "anthropic", "gemini", "ollama"];
                let current_idx = api_values.iter().position(|v| *v == api_type).unwrap_or(0);
                match select_step("  API Protocol", api_labels, current_idx) {
                    StepResult::Next(idx) => {
                        api_type = api_values[idx].to_string();
                        // Auto-fill base URL from API type default
                        let default_urls: &[(&str, &str)] = &[
                            ("openai", "https://api.openai.com/v1"),
                            ("openai-responses", "https://api.openai.com/v1"),
                            ("anthropic", "https://api.anthropic.com/v1"),
                            ("gemini", "https://generativelanguage.googleapis.com/v1beta"),
                            ("ollama", "http://localhost:11434"),
                        ];
                        if base_url.is_empty() {
                            if let Some((_, url)) = default_urls.iter().find(|(k, _)| *k == api_type) {
                                base_url = url.to_string();
                            }
                        }
                        wiz_step = STEP_BASE_URL;
                    }
                    StepResult::Back => { wiz_step = STEP_PROVIDER; }
                    StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                }
            }
            STEP_USER_AGENT => {
                // User-Agent header for custom/codingplan providers.
                let default_ua = if user_agent.is_empty() { "rsclaw/1.0".to_string() } else { user_agent.clone() };
                match input_step("  User-Agent header (blank for default)", default_ua) {
                    StepResult::Next(val) => { user_agent = val; wiz_step = STEP_API_KEY; }
                    StepResult::Back => { wiz_step = STEP_BASE_URL; }
                    StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                }
            }
            STEP_BASE_URL => {
                let provider = &defs.providers[provider_idx];
                if provider.name == "ollama" {
                    match input_step("  Ollama base URL", provider.base_url.to_string()) {
                        StepResult::Next(val) => { base_url = val; wiz_step = STEP_MODEL; }
                        StepResult::Back => { wiz_step = STEP_PROVIDER; }
                        StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                    }
                } else if provider.name == "custom" || provider.name == "codingplan" {
                    let default_url = if base_url.is_empty() { "https://api.example.com".to_string() } else { base_url.clone() };
                    match input_step("  API base URL", default_url) {
                        StepResult::Next(val) => { base_url = val; wiz_step = STEP_USER_AGENT; }
                        StepResult::Back => { wiz_step = STEP_API_TYPE; }
                        StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                    }
                } else if provider.name == "kimi" {
                    let default_url = if base_url.is_empty() { provider.base_url.to_string() } else { base_url.clone() };
                    match input_step("  Kimi API URL", default_url) {
                        StepResult::Next(val) => { base_url = val; wiz_step = STEP_API_KEY; }
                        StepResult::Back => { wiz_step = STEP_PROVIDER; }
                        StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                    }
                } else if provider.name == "doubao" {
                    let default_url = if base_url.is_empty() { provider.base_url.to_string() } else { base_url.clone() };
                    match input_step("  Doubao API URL", default_url) {
                        StepResult::Next(val) => { base_url = val; wiz_step = STEP_API_KEY; }
                        StepResult::Back => { wiz_step = STEP_PROVIDER; }
                        StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                    }
                } else {
                    base_url.clear();
                    wiz_step = STEP_API_KEY;
                }
            }
            STEP_API_KEY => {
                let provider = &defs.providers[provider_idx];
                if provider.needs_key || provider.name == "custom" {
                    let prompt = if provider.name == "custom" {
                        "  API key (blank if none required)".to_string()
                    } else {
                        let enter_key = crate::i18n::t("cli_enter_api_key", lang);
                        format!("  {} ({} - blank = env ${})", provider.label, enter_key, provider.env_var)
                    };
                    match password_step(&prompt) {
                        StepResult::Next(val) => { api_key = val; wiz_step = STEP_MODEL; }
                        StepResult::Back => {
                            if provider.name == "custom" { wiz_step = STEP_BASE_URL; }
                            else { wiz_step = STEP_PROVIDER; }
                        }
                        StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                    }
                } else {
                    api_key.clear();
                    wiz_step = STEP_MODEL;
                }
            }
            STEP_MODEL => {
                let provider = &defs.providers[provider_idx];
                let model_default = if default_model.is_empty() {
                    if provider.name == "custom" {
                        "custom/your-model-id".to_string()
                    } else {
                        provider.model.to_string()
                    }
                } else {
                    default_model.clone()
                };
                let model_prompt = crate::i18n::t("cli_default_model", lang);
                match input_step(&format!("  {model_prompt}"), model_default) {
                    StepResult::Next(val) => { default_model = val; wiz_step = STEP_PORT; }
                    StepResult::Back => {
                        let prov = &defs.providers[provider_idx];
                        if prov.name == "ollama" { wiz_step = STEP_BASE_URL; }
                        else if prov.name == "custom" || prov.name == "doubao" { wiz_step = STEP_API_KEY; }
                        else if !prov.needs_key { wiz_step = STEP_PROVIDER; }
                        else { wiz_step = STEP_API_KEY; }
                    }
                    StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                }
            }
            STEP_PORT => {
                header(&crate::i18n::t("cli_step_gateway", lang));
                let port_prompt = crate::i18n::t("cli_port", lang);
                match input_step(&format!("  {port_prompt}"), port) {
                    StepResult::Next(val) => { port = val; wiz_step = STEP_BIND; }
                    StepResult::Back => { wiz_step = STEP_MODEL; }
                    StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                }
            }
            STEP_BIND => {
                let (bind_labels, bind_values_vec) = build_bind_options();
                let bind_refs: Vec<&str> = bind_labels.iter().map(|s| s.as_str()).collect();
                let bind_prompt = crate::i18n::t("cli_bind_mode", lang);
                match select_step(&format!("  {bind_prompt}"), &bind_refs, bind_mode) {
                    StepResult::Next(idx) => {
                        custom_bind = Some(bind_values_vec[idx].clone());
                        bind_mode = idx;
                        wiz_step = STEP_CHANNELS;
                    }
                    StepResult::Back => { wiz_step = STEP_PORT; }
                    StepResult::Cancel => { println!("  {}", crate::i18n::t("cli_setup_cancelled", lang)); return Ok(()); }
                }
            }
            STEP_CHANNELS => {
                // One-at-a-time channel configuration loop
                let ch_header = crate::i18n::t("cli_choose_channels", lang);
                header(&crate::i18n::t_fmt("cli_step_channels", lang, &[("label", &ch_header)]));

                loop {
                    let available: Vec<(usize, &str)> = defs
                        .channels
                        .iter()
                        .enumerate()
                        .filter(|(_, ch)| !channel_configs.iter().any(|(n, _)| *n == ch.name))
                        .map(|(i, ch)| (i, ch.label.as_str()))
                        .collect();

                    if available.is_empty() {
                        println!("  {}", crate::i18n::t("cli_all_channels_configured", lang));
                        break;
                    }

                    // Add "Skip / Done" as the first option
                    let skip_done = crate::i18n::t("cli_skip_done", lang);
                    let mut labels: Vec<&str> = vec![&skip_done];
                    labels.extend(available.iter().map(|(_, l)| *l));

                    let add_prompt = if channel_configs.is_empty() {
                        crate::i18n::t("cli_add_channel", lang)
                    } else {
                        crate::i18n::t("cli_add_another_channel", lang)
                    };
                    match select_step(&format!("  {add_prompt}"), &labels, 0) {
                        StepResult::Next(0) => break, // Skip / Done
                        StepResult::Next(sel) => {
                            let (ch_idx, _) = available[sel - 1];
                            let ch = defs.channels[ch_idx].clone();
                            match configure_one_channel(&ch).await {
                                ChannelResult::Done(f) => {
                                    channel_configs.push((ch.name.clone(), f));
                                    // Loop back to offer next channel
                                }
                                ChannelResult::Back => {
                                    // Back from channel config -> show channel selection again
                                }
                                ChannelResult::Cancel => {
                                    println!("  {}", crate::i18n::t("cli_setup_cancelled", lang));
                                    return Ok(());
                                }
                            }
                        }
                        StepResult::Back => {
                            if channel_configs.is_empty() {
                                wiz_step = STEP_BIND;
                                continue 'outer;
                            } else {
                                channel_configs.pop();
                            }
                        }
                        StepResult::Cancel => {
                            println!("  {}", crate::i18n::t("cli_setup_cancelled", lang));
                            return Ok(());
                        }
                    }
                }
                wiz_step = STEP_DONE;
            }
            STEP_DONE => break,
            _ => break,
        }
    }

    // Build config
    let provider = &defs.providers[provider_idx];
    let api_key_entry = if api_key.is_empty() && provider.needs_key {
        format!("\"${{{}}}\"", provider.env_var)
    } else if api_key.is_empty() {
        "\"\"".to_string()
    } else {
        serde_json::to_string(&api_key)?
    };
    let (bind_str, bind_address) = if let Some(ref addr) = custom_bind {
        // Check if it's a named mode or an IP address
        match addr.as_str() {
            "loopback" | "all" => (addr.as_str(), None),
            ip => ("custom", Some(ip)),
        }
    } else {
        match bind_mode {
            0 => ("loopback", None),
            1 => ("all", None),
            _ => ("all", None),
        }
    };
    let effective_base_url = if !base_url.is_empty() {
        base_url.clone()
    } else if !provider.base_url.is_empty() {
        provider.base_url.to_string()
    } else {
        String::new()
    };
    // Write config — merge into existing config if present, otherwise create new.
    let base = crate::config::loader::base_dir();
    std::fs::create_dir_all(&base)?;
    let config_path = resolve_config_path_for_write();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let workspace_path = base.join("workspace").display().to_string().replace('\\', "/");
    let default_model_value = if default_model.contains('/') {
        default_model.clone()
    } else {
        format!("{}/{default_model}", provider.name)
    };

    // Load existing config or start fresh.
    let mut val: serde_json::Value = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|raw| json5::from_str(&raw).ok())
            .unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };

    // Ensure top-level is an object.
    if !val.is_object() {
        val = json!({});
    }

    // -- gateway --
    let gateway = val
        .as_object_mut().unwrap()
        .entry("gateway").or_insert_with(|| json!({}));
    if let Some(gw) = gateway.as_object_mut() {
        gw.insert("port".into(), json!(port));
        gw.insert("bind".into(), json!(bind_str));
        if let Some(ip) = bind_address {
            gw.insert("bindAddress".into(), json!(ip));
        }
    }

    // -- models.providers.<name> --
    if !val.get("models").is_some_and(|v| v.is_object()) {
        val.as_object_mut().unwrap().insert("models".into(), json!({}));
    }
    let models = val.as_object_mut().unwrap().get_mut("models").unwrap();
    let providers_obj = models
        .as_object_mut().unwrap()
        .entry("providers").or_insert_with(|| json!({}));
    if let Some(provs) = providers_obj.as_object_mut() {
        let prov_entry = provs
            .entry(provider.name.clone()).or_insert_with(|| json!({}));
        if let Some(prov_obj) = prov_entry.as_object_mut() {
            prov_obj.insert("apiKey".into(), serde_json::from_str(&api_key_entry).unwrap_or_else(|_| json!(api_key_entry)));
            if !effective_base_url.is_empty() {
                prov_obj.insert("baseUrl".into(), json!(effective_base_url));
            }
            // Write api type for custom/codingplan providers
            if (provider.name == "custom" || provider.name == "codingplan") && !api_type.is_empty() {
                prov_obj.insert("api".into(), json!(api_type));
            }
            // Write user_agent (from wizard input or provider default)
            if !user_agent.is_empty() {
                prov_obj.insert("userAgent".into(), json!(user_agent));
            } else if !provider.user_agent.is_empty() {
                prov_obj.insert("userAgent".into(), json!(provider.user_agent));
            }
        }
    }

    // -- agents: update or create the first agent entry --
    let agents = val
        .as_object_mut().unwrap()
        .entry("agents").or_insert_with(|| json!({}));
    if let Some(agents_obj) = agents.as_object_mut() {
        let list = agents_obj.entry("list").or_insert_with(|| json!([]));
        if let Some(arr) = list.as_array_mut() {
            if arr.is_empty() {
                arr.push(json!({
                    "id": agent_name,
                    "default": true,
                    "workspace": workspace_path,
                    "model": { "primary": default_model_value },
                }));
            } else {
                // Update the first agent in-place.
                let first = &mut arr[0];
                if let Some(obj) = first.as_object_mut() {
                    obj.insert("id".into(), json!(agent_name));
                    obj.insert("default".into(), json!(true));
                    obj.insert("workspace".into(), json!(workspace_path));
                    let model_obj = obj.entry("model").or_insert_with(|| json!({}));
                    if let Some(m) = model_obj.as_object_mut() {
                        m.insert("primary".into(), json!(default_model_value));
                    }
                }
            }
        }
    }

    // -- channels: only overwrite if the user configured channels in this run --
    if !channel_configs.is_empty() {
        let channels = val
            .as_object_mut().unwrap()
            .entry("channels").or_insert_with(|| json!({}));
        if let Some(ch_obj) = channels.as_object_mut() {
            for (name, fields) in &channel_configs {
                let mut entry = serde_json::Map::new();
                entry.insert("enabled".into(), json!(true));
                entry.insert("dmPolicy".into(), json!("pairing"));
                entry.insert("groupPolicy".into(), json!("allowlist"));
                for (k, v) in fields {
                    entry.insert(
                        k.clone(),
                        serde_json::from_str(v).unwrap_or_else(|_| json!(v)),
                    );
                }
                ch_obj.insert(name.clone(), serde_json::Value::Object(entry));
            }
        }
    }

    let content = serde_json::to_string_pretty(&val)?;

    // Backup existing config before overwriting.
    if config_path.exists() {
        rotate_backups(&config_path);
    }
    std::fs::write(&config_path, &content)?;

    // Create directory tree
    for dir in &[
        "var/data/redb", "var/data/search", "var/data/memory", "var/data/cron",
        "var/run", "var/logs", "var/cache",
        "skills", "models", "plugins",
    ] {
        std::fs::create_dir_all(base.join(dir))?;
    }

    // Write defaults.toml only if not present (user may have customized it)
    let defaults_path = base.join("defaults.toml");
    if !defaults_path.exists() {
        let _ = std::fs::write(&defaults_path, &builtin_defaults());
    }

    let workspace = base.join("workspace");
    let _ = agent::seed_workspace(&workspace);

    // Summary
    header(&crate::i18n::t("cli_onboard_complete", lang));
    step("*", &crate::i18n::t_fmt("cli_summary_config", lang, &[("path", &config_path.display().to_string())]));
    step("*", &crate::i18n::t_fmt("cli_summary_provider", lang, &[("label", &provider.label), ("name", &provider.name)]));
    step("*", &crate::i18n::t_fmt("cli_summary_model", lang, &[("model", &default_model)]));
    step("*", &crate::i18n::t_fmt("cli_summary_agent", lang, &[("name", &agent_name)]));
    step("*", &crate::i18n::t_fmt("cli_summary_port", lang, &[("port", &port.to_string())]));
    if !channel_configs.is_empty() {
        let names: Vec<&str> = channel_configs.iter().map(|(n, _)| n.as_str()).collect();
        step("*", &crate::i18n::t_fmt("cli_summary_channels", lang, &[("names", &names.join(", "))]));
    }
    println!();
    hint(&crate::i18n::t("cli_next_start", lang));
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Channel configuration helper (shared by onboard + configure)
// ---------------------------------------------------------------------------

enum ChannelResult {
    Done(Vec<(String, String)>),
    Back,
    Cancel,
}

/// Configure a single channel interactively.
async fn configure_one_channel(ch: &ChannelDef) -> ChannelResult {
    let lang = crate::i18n::default_lang();
    println!();
    println!("  -- {} --", ch.label);

    // Channels with login support
    if ch.login {
        if ch.fields.is_empty() {
            // Login-only channel (e.g. Weixin)
            println!("  {}", crate::i18n::t("cli_starting_login", lang));
            match run_channel_login(&ch.name).await {
                Ok(fields) => return ChannelResult::Done(fields),
                Err(e) => {
                    println!("  [!] {}", crate::i18n::t_fmt("cli_login_failed", lang, &[("err", &e.to_string())]));
                    println!("      {}", crate::i18n::t_fmt("cli_login_later", lang, &[("channel", &ch.name)]));
                    return ChannelResult::Done(vec![]);
                }
            }
        }

        // Channel supports both login and manual (e.g. Feishu)
        let opt_scan = crate::i18n::t("cli_scan_oauth", lang);
        let opt_manual = crate::i18n::t("cli_manual_input", lang);
        let options: Vec<&str> = vec![&opt_scan, &opt_manual];
        let auth_prompt = crate::i18n::t_fmt("cli_auth_method", lang, &[("label", &ch.label)]);
        match select_step(&format!("  {auth_prompt}"), &options, 0) {
            StepResult::Next(0) => {
                match run_channel_login(&ch.name).await {
                    Ok(fields) => return ChannelResult::Done(fields),
                    Err(e) => {
                        println!("  [!] {}", crate::i18n::t_fmt("cli_login_failed", lang, &[("err", &e.to_string())]));
                        println!("      {}", crate::i18n::t("cli_fallback_manual", lang));
                    }
                }
            }
            StepResult::Next(_) => { /* manual -- fall through */ }
            StepResult::Back => return ChannelResult::Back,
            StepResult::Cancel => return ChannelResult::Cancel,
        }
    }

    // No fields at all
    if ch.fields.is_empty() {
        return ChannelResult::Done(vec![]);
    }

    // Manual field prompts
    let mut fields = Vec::new();
    let mut field_idx = 0;
    while field_idx < ch.fields.len() {
        let f = &ch.fields[field_idx];
        let result = if f.secret {
            password_step(&format!("  {}", f.prompt))
        } else {
            input_step(&format!("  {}", f.prompt), String::new())
        };
        match result {
            StepResult::Next(val) => {
                if !val.is_empty() {
                    fields.push((f.key.clone(), val));
                }
                field_idx += 1;
            }
            StepResult::Back => {
                if field_idx == 0 {
                    return ChannelResult::Back;
                }
                if fields.last().is_some_and(|(k, _)| *k == ch.fields[field_idx - 1].key) {
                    fields.pop();
                }
                field_idx -= 1;
            }
            StepResult::Cancel => return ChannelResult::Cancel,
        }
    }
    ChannelResult::Done(fields)
}

// ---------------------------------------------------------------------------
// rsclaw configure
// ---------------------------------------------------------------------------

pub async fn cmd_configure(args: ConfigureArgs) -> Result<()> {
    let (path, mut val) = load_config_json().map_err(|e| {
        let err_str = format!("{e:#}");
        let lang0 = crate::i18n::default_lang();
        if err_str.contains("no config file found") {
            anyhow::anyhow!("{}", crate::i18n::t("cli_no_config_found", lang0))
        } else {
            anyhow::anyhow!("{}", crate::i18n::t_fmt("cli_config_parse_failed", lang0, &[("err", &err_str)]))
        }
    })?;

    // Load i18n language from config
    if let Ok(config) = crate::config::load() {
        if let Some(lang) = config.raw.gateway.as_ref().and_then(|g| g.language.as_deref()) {
            crate::i18n::set_default_lang(lang);
        }
    }
    let lang = crate::i18n::default_lang();

    header(&crate::i18n::t("cli_configure_title", lang));
    step("*", &crate::i18n::t_fmt("cli_editing", lang, &[("path", &path.display().to_string())]));
    hint(&crate::i18n::t("cli_press_esc", lang));

    let defs = load_defaults();
    let mut ec = load_existing_defaults(&defs);
    let original = val.clone();

    if !args.section.is_empty() {
        // Direct section mode: jump to requested sections
        for section in &args.section {
            match section.as_str() {
                "gateway" => configure_gateway(&mut val, &ec).await?,
                "model" | "provider" => configure_model(&mut val, &defs, &mut ec).await?,
                "channels" => configure_channels(&mut val, &defs).await?,
                "search" | "web" | "websearch" => configure_web_search(&mut val).await?,
                "upload" | "limits" => configure_upload_limits(&mut val).await?,
                "safety" | "exec" => configure_exec_safety(&mut val).await?,
                other => println!("  {}", crate::i18n::t_fmt("cli_unknown_section", lang, &[("name", other)])),
            }
        }
    } else {
        // Interactive section menu loop -- remember cursor position
        let mut last_idx: usize = 1; // Start at Gateway, not Save & Exit
        let mut at_save_exit = false; // true when cursor is on Save & Exit
        loop {
            let s_save = crate::i18n::t("cli_save_exit", lang);
            let s_gw = crate::i18n::t("cli_gateway", lang);
            let s_mp = crate::i18n::t("cli_model_provider", lang);
            let s_ch = crate::i18n::t("cli_channels", lang);
            let s_ws = crate::i18n::t("cli_web_search", lang);
            let s_ul = crate::i18n::t("cli_upload_limits", lang);
            let s_es = crate::i18n::t("cli_exec_safety", lang);
            let sections: Vec<&str> = vec![
                &s_save, &s_gw, &s_mp, &s_ch, &s_ws, &s_ul, &s_es,
            ];

            let section_prompt = crate::i18n::t("cli_configure_section", lang);
            match select_step(&format!("  {section_prompt}"), &sections, last_idx) {
                StepResult::Next(0) => break, // Save & Exit
                StepResult::Next(idx) => {
                    last_idx = idx;
                    at_save_exit = false;
                    match idx {
                        1 => configure_gateway(&mut val, &ec).await?,
                        2 => configure_model(&mut val, &defs, &mut ec).await?,
                        3 => configure_channels(&mut val, &defs).await?,
                        4 => configure_web_search(&mut val).await?,
                        5 => configure_upload_limits(&mut val).await?,
                        6 => configure_exec_safety(&mut val).await?,
                        _ => {}
                    }
                }
                StepResult::Back | StepResult::Cancel => {
                    if at_save_exit {
                        // Already at Save & Exit, ESC again -> discard and quit.
                        println!();
                        println!("  {}", crate::i18n::t("cli_cancelled", lang));
                        println!();
                        return Ok(());
                    }
                    // First ESC -> jump cursor to Save & Exit.
                    last_idx = 0;
                    at_save_exit = true;
                    continue;
                }
            }
        }
    }

    // Save only if changes were made
    if val == original {
        println!();
        println!("  {}", crate::i18n::t("cli_no_changes", lang));
        println!();
    } else {
        rotate_backups(&path);
        std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
        done(&crate::i18n::t_fmt("cli_saved_to", lang, &[("path", &path.display().to_string())]));

        // If gateway is running, restart it so changes take effect immediately.
        let pid_file = crate::cmd::gateway::gateway_pid_file();
        let gateway_running = pid_file.exists()
            && std::fs::read_to_string(&pid_file).ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .is_some_and(|pid| crate::sys::process_alive(pid));

        if gateway_running {
            hint(&crate::i18n::t("cli_restarting_gateway", lang));
            if let Ok(pid_str) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = pid_str.trim().parse::<u32>()
            {
                let _ = crate::sys::process_terminate(pid);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            match crate::cmd::gateway::spawn_gateway_bg_pub() {
                Ok(_) => done(&crate::i18n::t("cli_gateway_restarted", lang)),
                Err(e) => hint(&crate::i18n::t_fmt("cli_restart_failed", lang, &[("err", &e.to_string())])),
            }
        }
        println!();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Section: Gateway (port, bind)
// ---------------------------------------------------------------------------

async fn configure_gateway(val: &mut serde_json::Value, ec: &ExistingConfig) -> Result<()> {
    let lang = crate::i18n::default_lang();
    header(&crate::i18n::t("cli_section_gateway", lang));

    let current_port = get_nested_value(val, "gateway.port")
        .and_then(|v| v.as_u64())
        .unwrap_or(ec.port as u64) as u16;

    let bind_options = ["loopback", "all"];
    let current_bind = get_nested_value(val, "gateway.bind")
        .and_then(|v| v.as_str().map(|s| s.to_owned()))
        .unwrap_or_else(|| "loopback".into());
    let current_bind_idx = bind_options.iter().position(|&b| b == current_bind).unwrap_or(0);

    // Port
    let port_prompt = crate::i18n::t("cli_port", lang);
    let new_port = match input_step(&format!("  {port_prompt}"), current_port) {
        StepResult::Next(v) => v,
        StepResult::Back | StepResult::Cancel => return Ok(()),
    };

    // Bind mode with auto-detected LAN IPs
    let (bind_labels, bind_values) = build_bind_options();
    let bind_refs: Vec<&str> = bind_labels.iter().map(|s| s.as_str()).collect();
    let bind_prompt = crate::i18n::t("cli_bind_mode", lang);
    let new_bind_value = match select_step(&format!("  {bind_prompt}"), &bind_refs, current_bind_idx) {
        StepResult::Next(idx) => bind_values[idx].clone(),
        StepResult::Back | StepResult::Cancel => return Ok(()),
    };

    // Apply
    if new_port != current_port {
        ensure_json_path(val, &["gateway"]);
        set_nested_value(val, "gateway.port", serde_json::json!(new_port))?;
    }
    ensure_json_path(val, &["gateway"]);
    // Check if it's an IP address or a named mode
    let is_ip = new_bind_value.parse::<std::net::IpAddr>().is_ok();
    if is_ip {
        set_nested_value(val, "gateway.bind", serde_json::json!("custom"))?;
        set_nested_value(val, "gateway.bindAddress", serde_json::json!(new_bind_value))?;
    } else {
        set_nested_value(val, "gateway.bind", serde_json::json!(new_bind_value))?;
        // Remove bindAddress if switching back to named mode
        if let Some(obj) = val.pointer_mut("/gateway").and_then(|v| v.as_object_mut()) {
            obj.remove("bindAddress");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Section: Model Provider (provider, API key, model)
// ---------------------------------------------------------------------------

async fn configure_model(
    val: &mut serde_json::Value,
    defs: &Defaults,
    ec: &mut ExistingConfig,
) -> Result<()> {
    let lang = crate::i18n::default_lang();
    header(&crate::i18n::t("cli_section_model_provider", lang));

    let provider_labels: Vec<&str> = defs.providers.iter().map(|p| p.label.as_str()).collect();
    let mut provider_idx = ec.provider_idx;

    let current_model = ec.model.clone();
    let mut new_model = if current_model.is_empty() {
        defs.providers[provider_idx].model.clone()
    } else {
        current_model.clone()
    };

    // Provider select
    let prov_prompt = crate::i18n::t("cli_provider", lang);
    match select_step(&format!("  {prov_prompt}"), &provider_labels, provider_idx) {
        StepResult::Next(idx) => {
            if idx != provider_idx {
                // Save current provider's model before switching
                if !new_model.is_empty() {
                    let cur_prov = &defs.providers[provider_idx].name;
                    let save_model = if new_model.contains('/') {
                        new_model.clone()
                    } else {
                        format!("{cur_prov}/{new_model}")
                    };
                    ec.provider_models.insert(cur_prov.clone(), save_model);
                }
                // Look up previously saved model for the new provider
                let prov_name = &defs.providers[idx].name;
                new_model = ec
                    .provider_models
                    .get(prov_name.as_str())
                    .cloned()
                    .unwrap_or_default();
            }
            provider_idx = idx;
        }
        StepResult::Back | StepResult::Cancel => return Ok(()),
    }

    let provider = &defs.providers[provider_idx];
    let new_base_url;
    let mut change_key = false;
    let mut new_key = String::new();

    // Base URL (ollama / custom)
    if provider.name == "ollama" {
        let current = get_nested_value(val, &format!("models.providers.{}.baseUrl", provider.name))
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_else(|| provider.base_url.to_string());
        match input_step("  Ollama base URL", current) {
            StepResult::Next(u) => new_base_url = u,
            StepResult::Back | StepResult::Cancel => return Ok(()),
        }
    } else if provider.name == "custom" || provider.name == "codingplan" {
        let current = get_nested_value(val, &format!("models.providers.{}.baseUrl", provider.name))
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_else(|| "https://api.example.com".to_string());
        match input_step("  API base URL", current) {
            StepResult::Next(u) => new_base_url = u,
            StepResult::Back | StepResult::Cancel => return Ok(()),
        }
    } else if provider.name == "doubao" {
        let current = get_nested_value(val, "models.providers.doubao.baseUrl")
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_else(|| provider.base_url.to_string());
        match input_step("  Doubao API URL", current) {
            StepResult::Next(u) => new_base_url = u,
            StepResult::Back | StepResult::Cancel => return Ok(()),
        }
    } else {
        new_base_url = provider.base_url.to_string();
    }

    // API type + User-Agent for custom/codingplan providers
    let mut new_api_type = String::new();
    let mut new_user_agent = String::new();
    if provider.name == "custom" || provider.name == "codingplan" {
        // API protocol selection
        let api_labels = &[
            "OpenAI Chat (default)",
            "OpenAI Responses",
            "Anthropic",
            "Google Gemini",
            "Ollama",
        ];
        let api_values = &["openai", "openai-responses", "anthropic", "gemini", "ollama"];
        let current_api = get_nested_value(val, &format!("models.providers.{}.api", provider.name))
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_else(|| "openai".to_string());
        let current_idx = api_values.iter().position(|v| *v == current_api).unwrap_or(0);
        match select_step("  API Protocol", api_labels, current_idx) {
            StepResult::Next(idx) => { new_api_type = api_values[idx].to_string(); }
            StepResult::Back | StepResult::Cancel => return Ok(()),
        }

        // User-Agent header
        let current_ua = get_nested_value(val, &format!("models.providers.{}.userAgent", provider.name))
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_else(|| "rsclaw/1.0".to_string());
        match input_step("  User-Agent header", current_ua) {
            StepResult::Next(ua) => { new_user_agent = ua; }
            StepResult::Back | StepResult::Cancel => return Ok(()),
        }
    }

    // API key confirm + input
    if provider.needs_key || provider.name == "custom" || provider.name == "codingplan" {
        let api_key_path = format!("models.providers.{}.apiKey", provider.name);
        let current_key_display = get_nested_value(val, &api_key_path)
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .map(|s| {
                if s.starts_with("${") {
                    s
                } else if s.len() > 8 {
                    format!("{}...{}", &s[..4], &s[s.len() - 4..])
                } else {
                    "*".repeat(s.len().min(20))
                }
            })
            .unwrap_or_else(|| crate::i18n::t("cli_not_set", lang));
        step("*", &crate::i18n::t_fmt("cli_current_key", lang, &[("key", &current_key_display)]));

        let change_prompt = crate::i18n::t("cli_change_api_key", lang);
        match confirm_step(&format!("  {change_prompt}"), false) {
            StepResult::Next(true) => {
                change_key = true;
                match password_step(&format!(
                    "  {} API key (blank = env ${})",
                    provider.label, provider.env_var
                )) {
                    StepResult::Next(k) => new_key = k,
                    StepResult::Back | StepResult::Cancel => return Ok(()),
                }
            }
            StepResult::Next(false) => {}
            StepResult::Back | StepResult::Cancel => return Ok(()),
        }
    }

    // Model
    let model_default = if !new_model.is_empty() {
        new_model.clone()
    } else if !provider.model.is_empty() {
        provider.model.clone()
    } else {
        format!("{}/your-model-id", provider.name)
    };
    let model_prompt = crate::i18n::t("cli_default_model", lang);
    match input_step(&format!("  {model_prompt}"), model_default) {
        StepResult::Next(m) => new_model = m,
        StepResult::Back | StepResult::Cancel => return Ok(()),
    }

    // Connectivity test
    let test_url = if !new_base_url.is_empty() {
        new_base_url.clone()
    } else {
        provider.base_url.clone()
    };
    let test_key = if change_key && !new_key.is_empty() {
        Some(new_key.clone())
    } else {
        get_nested_value(val, &format!("models.providers.{}.apiKey", provider.name))
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .filter(|k| !k.starts_with("${") && !k.is_empty())
            .or_else(|| {
                std::env::var(if provider.env_var.is_empty() {
                    "_NONE_"
                } else {
                    &provider.env_var
                })
                .ok()
            })
    };
    if !test_url.is_empty()
        || provider.name == "anthropic"
        || provider.name == "openai"
        || provider.name == "gemini"
    {
        step("*", &crate::i18n::t("cli_testing_connectivity", lang));
        match test_provider_connectivity(&test_url, test_key.as_deref(), &provider.name).await {
            Ok(()) => step("*", &crate::i18n::t("cli_connection_ok", lang)),
            Err(e) => {
                println!("  [!] {}", crate::i18n::t_fmt("cli_connection_failed", lang, &[("err", &e.to_string())]));
                println!("      {}", crate::i18n::t("cli_fix_later", lang));
            }
        }
    }

    // Apply changes
    if change_key && (provider.needs_key || provider.name == "custom") {
        let api_key_path = format!("models.providers.{}.apiKey", provider.name);
        let key_val = if new_key.is_empty() && !provider.env_var.is_empty() {
            format!("${{{}}}", provider.env_var)
        } else {
            new_key
        };
        ensure_json_path(val, &["models"]);
        ensure_json_path(val, &["models", "providers"]);
        ensure_json_path(val, &["models", "providers", &provider.name]);
        set_nested_value(val, &api_key_path, serde_json::json!(key_val))?;
    }

    if !new_base_url.is_empty() {
        let url_path = format!("models.providers.{}.baseUrl", provider.name);
        ensure_json_path(val, &["models"]);
        ensure_json_path(val, &["models", "providers"]);
        ensure_json_path(val, &["models", "providers", &provider.name]);
        set_nested_value(val, &url_path, serde_json::json!(new_base_url))?;
    }

    // Write api type and user_agent for custom/codingplan
    if !new_api_type.is_empty() {
        let api_path = format!("models.providers.{}.api", provider.name);
        ensure_json_path(val, &["models", "providers", &provider.name]);
        set_nested_value(val, &api_path, serde_json::json!(new_api_type))?;
    }
    if !new_user_agent.is_empty() {
        let ua_path = format!("models.providers.{}.userAgent", provider.name);
        ensure_json_path(val, &["models", "providers", &provider.name]);
        set_nested_value(val, &ua_path, serde_json::json!(new_user_agent))?;
    }

    // Ensure model has provider/ prefix
    let final_model = if new_model.contains('/') {
        new_model.clone()
    } else {
        format!("{}/{new_model}", provider.name)
    };

    if final_model != current_model {
        // Write to agents.list[0].model.primary (rsclaw format)
        if let Some(arr) = val
            .get_mut("agents")
            .and_then(|a| a.get_mut("list"))
            .and_then(|l| l.as_array_mut())
            && let Some(agent) = arr.first_mut()
            && let Some(m) = agent.get_mut("model").and_then(|m| m.as_object_mut())
        {
            m.insert("primary".to_string(), serde_json::json!(final_model));
        }

        // Also write to agents.defaults.model.primary (openclaw compat)
        ensure_json_path(val, &["agents"]);
        ensure_json_path(val, &["agents", "defaults"]);
        ensure_json_path(val, &["agents", "defaults", "model"]);
        set_nested_value(
            val,
            "agents.defaults.model.primary",
            serde_json::json!(final_model),
        )?;

        // Save to per-provider models map (openclaw compat: agents.defaults.models)
        ensure_json_path(val, &["agents", "defaults", "models"]);
        if let Some(models_obj) = val
            .pointer_mut("/agents/defaults/models")
            .and_then(|v| v.as_object_mut())
        {
            models_obj.insert(
                final_model.clone(),
                serde_json::json!({ "alias": provider.name }),
            );
        }
    }

    // Update ec so subsequent sections see updated state
    ec.provider_idx = provider_idx;
    ec.model = final_model;

    Ok(())
}

// ---------------------------------------------------------------------------
// Section: Channels (add/remove)
// ---------------------------------------------------------------------------

fn get_channel_enabled(val: &serde_json::Value, ch_name: &str) -> bool {
    val.get("channels")
        .and_then(|c| c.get(ch_name))
        .map(|ch| ch.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true))
        .unwrap_or(false) // not in config = not enabled
}

fn toggle_channel_enabled(val: &mut serde_json::Value, ch_name: &str, enabled: bool) {
    ensure_json_path(val, &["channels"]);
    ensure_json_path(val, &["channels", ch_name]);
    if let Some(ch) = val.get_mut("channels").and_then(|c| c.get_mut(ch_name)) {
        if let Some(obj) = ch.as_object_mut() {
            obj.insert("enabled".to_string(), serde_json::json!(enabled));
            // Ensure dmPolicy and groupPolicy have explicit defaults
            obj.entry("dmPolicy").or_insert(serde_json::json!("pairing"));
            obj.entry("groupPolicy").or_insert(serde_json::json!("allowlist"));
        }
    }
}

fn channel_is_configured(val: &serde_json::Value, ch_name: &str) -> bool {
    val.get("channels")
        .and_then(|c| c.get(ch_name))
        .and_then(|ch| ch.as_object())
        .is_some_and(|obj| obj.keys().any(|k| k != "enabled"))
}

async fn edit_channel_config(
    val: &mut serde_json::Value,
    ch: &ChannelDef,
) -> bool {
    // Login-based channels: only show login option if NOT already configured
    let already_configured = ch.fields.iter().any(|f| {
        let path = format!("channels.{}.{}", ch.name, f.key);
        get_nested_value(val, &path)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    });

    let lang = crate::i18n::default_lang();

    if ch.login {
        if ch.fields.is_empty() && !already_configured {
            // Login-only channel with no manual fields
            println!("  {}", crate::i18n::t("cli_starting_login", lang));
            match run_channel_login(&ch.name).await {
                Ok(_fields) => {
                    toggle_channel_enabled(val, &ch.name, true);
                    return true;
                }
                Err(e) => {
                    println!("  [!] {}", crate::i18n::t_fmt("cli_login_failed", lang, &[("err", &e.to_string())]));
                    println!("      {}", crate::i18n::t_fmt("cli_login_later", lang, &[("channel", &ch.name)]));
                    return false;
                }
            }
        }

        // Show scan/manual/back -- default to manual if already configured
        let default_idx = if already_configured { 1 } else { 0 };
        let opt_scan = crate::i18n::t("cli_scan_rescan", lang);
        let opt_manual = crate::i18n::t("cli_manual_edit", lang);
        let opt_back = crate::i18n::t("cli_back", lang);
        let options_vec = [opt_scan.as_str(), opt_manual.as_str(), opt_back.as_str()];
        let auth_prompt = crate::i18n::t_fmt("cli_auth_method", lang, &[("label", &ch.label)]);
        match select_step(&format!("  {auth_prompt}"), &options_vec, default_idx) {
            StepResult::Next(0) => {
                match run_channel_login(&ch.name).await {
                    Ok(fields) => {
                        ensure_json_path(val, &["channels"]);
                        ensure_json_path(val, &["channels", &ch.name]);
                        for (k, v) in &fields {
                            let path = format!("channels.{}.{}", ch.name, k);
                            let _ = set_nested_value(val, &path, serde_json::json!(v));
                        }
                        toggle_channel_enabled(val, &ch.name, true);
                        return true;
                    }
                    Err(e) => {
                        println!("  [!] {}", crate::i18n::t_fmt("cli_login_failed", lang, &[("err", &e.to_string())]));
                        println!("      {}", crate::i18n::t("cli_fallback_manual", lang));
                    }
                }
            }
            StepResult::Next(1) => { /* manual -- fall through to field editor */ }
            _ => return false,
        }
    }

    if ch.fields.is_empty() {
        println!("  {}", crate::i18n::t_fmt("cli_no_fields", lang, &[("label", &ch.label)]));
        return false;
    }

    let mut changed = false;
    let is_configured = ch.fields.iter().any(|f| {
        let path = format!("channels.{}.{}", ch.name, f.key);
        get_nested_value(val, &path)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    });

    println!();
    if is_configured {
        println!("  {}", crate::i18n::t_fmt("cli_config_enter_keep", lang, &[("label", &ch.label)]));
    } else {
        println!("  {}", crate::i18n::t_fmt("cli_config_label", lang, &[("label", &ch.label)]));
    }

    for field in &ch.fields {
        let path = format!("channels.{}.{}", ch.name, field.key);
        let current = get_nested_value(val, &path)
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_default();

        let result = if field.secret && !current.is_empty() {
            // Show masked value for secrets, ask to change
            let masked = if current.starts_with("${") {
                current.clone()
            } else if current.len() > 8 {
                format!("{}...{}", &current[..4], &current[current.len() - 4..])
            } else {
                "*".repeat(current.len().min(8))
            };
            let keep_label = crate::i18n::t_fmt("cli_keep", lang, &[("value", &masked)]);
            let edit_label = crate::i18n::t("cli_edit", lang);
            let back_label = crate::i18n::t("cli_back", lang);
            let items = &[
                keep_label.as_str(),
                edit_label.as_str(),
                back_label.as_str(),
            ];
            match Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt(&format!("  {}", field.prompt))
                .items(items)
                .default(0)
                .interact_opt()
            {
                Ok(Some(0)) => continue,  // Keep
                Ok(Some(1)) => password_step(&format!("  {}", field.prompt)),
                _ => StepResult::Back,
            }
        } else {
            input_step(&format!("  {}", field.prompt), current.clone())
        };

        match result {
            StepResult::Next(new_val) => {
                if new_val != current && !new_val.is_empty() {
                    ensure_json_path(val, &["channels"]);
                    ensure_json_path(val, &["channels", &ch.name]);
                    let _ = set_nested_value(val, &path, serde_json::json!(new_val));
                    // Don't auto-enable; user controls enabled via Space toggle
                    changed = true;
                }
            }
            StepResult::Back | StepResult::Cancel => break,
        }
    }

    // DM Policy selector (common to all channels)
    let dm_path = format!("channels.{}.dmPolicy", ch.name);
    let current_dm = get_nested_value(val, &dm_path)
        .and_then(|v| v.as_str())
        .unwrap_or("pairing")
        .to_owned();
    let dm_policies = &["pairing", "open", "allowlist", "disabled"];
    let dm_idx = dm_policies.iter().position(|&p| p == current_dm).unwrap_or(0);
    let dm_prompt = crate::i18n::t_fmt("cli_dm_policy", lang, &[("policy", &current_dm)]);
    match select_step(
        &format!("  {dm_prompt}"),
        dm_policies,
        dm_idx,
    ) {
        StepResult::Next(idx) => {
            let new_policy = dm_policies[idx];
            if new_policy != current_dm {
                ensure_json_path(val, &["channels"]);
                ensure_json_path(val, &["channels", &ch.name]);
                let _ = set_nested_value(val, &dm_path, serde_json::json!(new_policy));
                changed = true;
            }
        }
        _ => {}
    }

    // Group Policy selector
    let gp_path = format!("channels.{}.groupPolicy", ch.name);
    let current_gp = get_nested_value(val, &gp_path)
        .and_then(|v| v.as_str())
        .unwrap_or("allowlist")
        .to_owned();
    let gp_policies = &["allowlist", "open", "disabled"];
    let gp_idx = gp_policies.iter().position(|&p| p == current_gp).unwrap_or(0);
    let gp_prompt = crate::i18n::t_fmt("cli_group_policy", lang, &[("policy", &current_gp)]);
    match select_step(
        &format!("  {gp_prompt}"),
        gp_policies,
        gp_idx,
    ) {
        StepResult::Next(idx) => {
            let new_policy = gp_policies[idx];
            if new_policy != current_gp {
                ensure_json_path(val, &["channels"]);
                ensure_json_path(val, &["channels", &ch.name]);
                let _ = set_nested_value(val, &gp_path, serde_json::json!(new_policy));
                changed = true;
            }
        }
        _ => {}
    }

    changed
}

async fn configure_channels(val: &mut serde_json::Value, defs: &Defaults) -> Result<()> {
    let lang = crate::i18n::default_lang();
    header(&crate::i18n::t("cli_section_channels", lang));
    hint(&crate::i18n::t("cli_channels_hint", lang));

    let term = console::Term::stderr();
    let mut cursor: usize = 0;

    loop {
        // Build items -- add [Finished] at top
        let finished_label = crate::i18n::t("cli_finished", lang);
        let configured_label = crate::i18n::t("cli_configured", lang);
        let mut items: Vec<String> = vec![finished_label];
        items.extend(defs
            .channels
            .iter()
            .map(|ch| {
                let enabled = get_channel_enabled(val, &ch.name);
                let configured = channel_is_configured(val, &ch.name);
                let check = if enabled { "\x1b[32m\u{25c9}\x1b[0m" } else { "\u{25cb}" };
                let tag = if configured { &configured_label } else { "" };
                format!("{} {}{}", check, ch.label, if tag.is_empty() { String::new() } else { format!(" ({})", tag.trim()) })
            }));

        // Render list
        let _ = term.clear_screen();
        println!("  {}", crate::i18n::t("cli_section_channels", lang));
        println!("  {}", "\u{2500}".repeat(20));
        println!("  {}", crate::i18n::t("cli_channels_hint_short", lang));
        println!();
        for (i, item) in items.iter().enumerate() {
            if i == cursor {
                println!("  \x1b[36m> {item}\x1b[0m");
            } else {
                println!("    {item}");
            }
        }
        println!();

        // Read key
        match term.read_key() {
            Ok(console::Key::ArrowUp) => {
                if cursor > 0 { cursor -= 1; }
            }
            Ok(console::Key::ArrowDown) => {
                if cursor < items.len() - 1 { cursor += 1; }
            }
            Ok(console::Key::Char(' ')) => {
                if cursor == 0 { continue; } // [Finished] can't toggle
                let ch = &defs.channels[cursor - 1];
                let is_enabled = get_channel_enabled(val, &ch.name);
                toggle_channel_enabled(val, &ch.name, !is_enabled);
            }
            Ok(console::Key::Enter) => {
                if cursor == 0 { break; } // [Finished] = save & exit
                let _ = term.clear_screen();
                let ch_clone = defs.channels[cursor - 1].clone();
                edit_channel_config(val, &ch_clone).await;
            }
            Ok(console::Key::Escape) => break,
            Ok(console::Key::Char('q')) => break,
            _ => {}
        }
    }

    // Clear and return
    let _ = term.clear_screen();
    Ok(())
}

// ---------------------------------------------------------------------------
// Section: Web Search (provider, API keys)
// ---------------------------------------------------------------------------

async fn configure_web_search(val: &mut serde_json::Value) -> Result<()> {
    let lang = crate::i18n::default_lang();
    header(&crate::i18n::t("cli_section_web_search", lang));

    let lang = crate::i18n::default_lang();
    let providers: Vec<String> = if lang == "zh" {
        vec![
            "Bing (免费)".into(),
            "Baidu/百度 (免费)".into(),
            "Sogou/搜狗 (免费)".into(),
            "360搜索 (免费)".into(),
            "DuckDuckGo (免费)".into(),
            "Google (需要接口密钥)".into(),
            "Bing (需要接口密钥)".into(),
            "Brave (需要接口密钥)".into(),
            "Baidu/百度 (需要接口密钥)".into(),
        ]
    } else {
        vec![
            "Bing (free, no key)".into(),
            "Baidu (free, no key)".into(),
            "Sogou (free, no key)".into(),
            "360 Search (free, no key)".into(),
            "DuckDuckGo (free, no key)".into(),
            "Google (API key)".into(),
            "Bing (API key)".into(),
            "Brave (API key)".into(),
            "Baidu (API key)".into(),
        ]
    };
    let provider_refs: Vec<&str> = providers.iter().map(|s| s.as_str()).collect();

    // Detect current provider
    let current = get_nested_value(val, "tools.webSearch.provider")
        .and_then(|v| v.as_str().map(|s| s.to_owned()))
        .unwrap_or_default();
    let default_idx = match current.as_str() {
        "bing-free" => 0,
        "baidu-free" => 1,
        "sogou" => 2,
        "360" => 3,
        "duckduckgo" => 4,
        "google" => 5,
        "bing" => 6,
        "brave" => 7,
        "baidu" => 8,
        _ => 0,
    };

    let search_prompt = crate::i18n::t("cli_search_provider", lang);
    match select_step(&format!("  {search_prompt}"), &provider_refs, default_idx) {
        StepResult::Next(0) => {
            // Bing Free
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("bing-free"))?;
        }
        StepResult::Next(1) => {
            // Baidu Free
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("baidu-free"))?;
        }
        StepResult::Next(2) => {
            // Sogou Free
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("sogou"))?;
        }
        StepResult::Next(3) => {
            // 360 Search Free
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("360"))?;
        }
        StepResult::Next(4) => {
            // DuckDuckGo
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("duckduckgo"))?;
        }
        StepResult::Next(5) => {
            // Google
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("google"))?;

            match password_step("  Google API Key") {
                StepResult::Next(key) if !key.is_empty() => {
                    set_nested_value(
                        val,
                        "tools.webSearch.googleApiKey",
                        serde_json::json!(key),
                    )?;
                }
                _ => {}
            }
            match input_step("  Google CX (Custom Search ID)", String::new()) {
                StepResult::Next(cx) if !cx.is_empty() => {
                    set_nested_value(val, "tools.webSearch.googleCx", serde_json::json!(cx))?;
                }
                _ => {}
            }
        }
        StepResult::Next(6) => {
            // Bing
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("bing"))?;
            match password_step("  Bing API Key") {
                StepResult::Next(key) if !key.is_empty() => {
                    set_nested_value(
                        val,
                        "tools.webSearch.bingApiKey",
                        serde_json::json!(key),
                    )?;
                }
                _ => {}
            }
        }
        StepResult::Next(7) => {
            // Brave
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("brave"))?;
            match password_step("  Brave API Key") {
                StepResult::Next(key) if !key.is_empty() => {
                    set_nested_value(
                        val,
                        "tools.webSearch.braveApiKey",
                        serde_json::json!(key),
                    )?;
                }
                _ => {}
            }
        }
        StepResult::Next(8) => {
            // Baidu API
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "webSearch"]);
            set_nested_value(val, "tools.webSearch.provider", serde_json::json!("baidu"))?;
            match password_step("  Baidu API Key") {
                StepResult::Next(key) if !key.is_empty() => {
                    set_nested_value(
                        val,
                        "tools.webSearch.baiduApiKey",
                        serde_json::json!(key),
                    )?;
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Section: Upload Limits (file size, text chars, vision)
// ---------------------------------------------------------------------------

async fn configure_upload_limits(val: &mut serde_json::Value) -> Result<()> {
    let lang = crate::i18n::default_lang();
    header(&crate::i18n::t("cli_section_upload_limits", lang));

    let current_size = get_nested_value(val, "tools.upload.maxFileSize")
        .and_then(|v| v.as_u64())
        .unwrap_or(50_000_000)
        / 1_000_000;
    let current_chars = get_nested_value(val, "tools.upload.maxTextChars")
        .and_then(|v| v.as_u64())
        .unwrap_or(50_000);

    let size_prompt = crate::i18n::t("cli_max_file_size", lang);
    match input_step(&format!("  {size_prompt}"), current_size as u32) {
        StepResult::Next(mb) => {
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "upload"]);
            set_nested_value(
                val,
                "tools.upload.maxFileSize",
                serde_json::json!(mb as u64 * 1_000_000),
            )?;
        }
        _ => return Ok(()),
    }

    let chars_prompt = crate::i18n::t("cli_max_text_chars", lang);
    match input_step(&format!("  {chars_prompt}"), current_chars as u32) {
        StepResult::Next(chars) => {
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "upload"]);
            set_nested_value(val, "tools.upload.maxTextChars", serde_json::json!(chars))?;
        }
        _ => return Ok(()),
    }

    // Vision support
    let current_vision = get_nested_value(val, "tools.upload.supportsVision")
        .and_then(|v| v.as_bool());
    let vision_options = &["Auto-detect", "Yes", "No"];
    let default_v = match current_vision {
        Some(true) => 1,
        Some(false) => 2,
        None => 0,
    };
    let vision_prompt = crate::i18n::t("cli_vision_support", lang);
    match select_step(&format!("  {vision_prompt}"), vision_options, default_v) {
        StepResult::Next(0) => {
            // Remove override, use auto-detect
            if let Some(obj) = val
                .pointer_mut("/tools/upload")
                .and_then(|v| v.as_object_mut())
            {
                obj.remove("supportsVision");
            }
        }
        StepResult::Next(1) => {
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "upload"]);
            set_nested_value(val, "tools.upload.supportsVision", serde_json::json!(true))?;
        }
        StepResult::Next(2) => {
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "upload"]);
            set_nested_value(val, "tools.upload.supportsVision", serde_json::json!(false))?;
        }
        _ => {}
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Section: Exec Safety (on/off)
// ---------------------------------------------------------------------------

async fn configure_exec_safety(val: &mut serde_json::Value) -> Result<()> {
    let lang = crate::i18n::default_lang();
    header(&crate::i18n::t("cli_section_exec_safety", lang));

    let current = get_nested_value(val, "tools.exec.safety")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let status = if current {
        crate::i18n::t("cli_exec_enabled", lang)
    } else {
        crate::i18n::t("cli_exec_disabled", lang)
    };
    step("*", &crate::i18n::t_fmt("cli_exec_current", lang, &[("status", &status)]));

    let enable_prompt = crate::i18n::t("cli_enable_exec_safety", lang);
    match confirm_step(&format!("  {enable_prompt}"), current) {
        StepResult::Next(enabled) => {
            ensure_json_path(val, &["tools"]);
            ensure_json_path(val, &["tools", "exec"]);
            set_nested_value(val, "tools.exec.safety", serde_json::json!(enabled))?;
            if enabled {
                step("*", &crate::i18n::t("cli_exec_safety_on", lang));
            } else {
                step("*", &crate::i18n::t("cli_exec_safety_off", lang));
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Channel login helper
// ---------------------------------------------------------------------------

/// Run the login flow for a channel, returning config key-value pairs on success.
async fn run_channel_login(channel: &str) -> anyhow::Result<Vec<(String, String)>> {
    let client = reqwest::Client::new();
    match channel {
        "wechat" | "weixin" => {
            let lang = crate::i18n::default_lang();
            println!("  {}", crate::i18n::t("cli_scanning_qr", lang));
            let (_url, qrcode) =
                crate::channel::wechat::WeChatPersonalChannel::start_qr_login(&client).await?;
            let (token, bot_id) =
                crate::channel::wechat::WeChatPersonalChannel::wait_qr_login(&client, &qrcode)
                    .await?;
            println!("  {}", crate::i18n::t_fmt("cli_login_success_bot", lang, &[("id", &bot_id)]));
            // token from ilink API is already in "botId:secret" format
            Ok(vec![
                ("botId".to_string(), bot_id),
                ("botToken".to_string(), token),
            ])
        }
        "feishu" | "lark" => {
            let brand = if channel == "lark" { "lark" } else { "feishu" };
            let (app_id, app_secret, actual_brand) =
                crate::channel::auth::feishu_auth::onboard(&client, brand).await?;
            println!("  {}", crate::i18n::t_fmt("cli_login_success_brand", crate::i18n::default_lang(), &[("brand", &actual_brand)]));
            Ok(vec![
                ("appId".to_string(), app_id),
                ("appSecret".to_string(), app_secret),
                ("brand".to_string(), actual_brand),
                ("connectionMode".to_string(), "websocket".to_string()),
            ])
        }
        _ => {
            anyhow::bail!("no login flow implemented for channel '{channel}'");
        }
    }
}

/// Test provider connectivity by hitting the models list endpoint.
async fn test_provider_connectivity(
    base_url: &str,
    api_key: Option<&str>,
    provider_name: &str,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let url = match provider_name {
        "anthropic" => "https://api.anthropic.com/v1/models".to_owned(),
        "gemini" => return Ok(()), // Gemini uses query param auth, skip
        _ => {
            let base = if base_url.is_empty() { "https://api.openai.com" } else { base_url };
            if base.ends_with("/v1") || base.contains("/v1/") {
                format!("{}/models", base.trim_end_matches('/'))
            } else {
                format!("{}/v1/models", base.trim_end_matches('/'))
            }
        }
    };

    let mut req = client.get(&url);
    if let Some(key) = api_key {
        if provider_name == "anthropic" {
            req = req.header("x-api-key", key).header("anthropic-version", "2023-06-01");
        } else {
            req = req.header("authorization", format!("Bearer {key}"));
        }
    }

    let resp = req.send().await.map_err(|e| anyhow::anyhow!("connection failed: {e}"))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 401 {
        // 401 = auth error but connection works; 200 = all good
        if status.as_u16() == 401 {
            anyhow::bail!("connected but API key is invalid (401)");
        }
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("{status}: {}", &body[..body.len().min(200)]);
    }
}

/// Rotate config backups: keep last 3 (.bak.1 = newest, .bak.3 = oldest).
/// Resolve config file path for writing:
/// 1. Existing config (detected by priority) -> use it
/// 2. Nothing exists -> create new in base_dir/rsclaw.json5
fn resolve_config_path_for_write() -> std::path::PathBuf {
    if let Some(existing) = crate::config::loader::detect_config_path() {
        return existing;
    }
    // Nothing exists -> new file in base_dir (defaults to ~/.rsclaw/)
    let base = dirs_next::home_dir().unwrap_or_default().join(".rsclaw");
    base.join("rsclaw.json5")
}

fn rotate_backups(path: &std::path::Path) {
    let ext = path.extension().unwrap_or_default().to_string_lossy();
    let bak3 = path.with_extension(format!("{ext}.bak.3"));
    let bak2 = path.with_extension(format!("{ext}.bak.2"));
    let bak1 = path.with_extension(format!("{ext}.bak.1"));
    let _ = std::fs::remove_file(&bak3);
    let _ = std::fs::rename(&bak2, &bak3);
    let _ = std::fs::rename(&bak1, &bak2);
    let _ = std::fs::copy(path, &bak1);
}

/// Ensure the nested JSON object path exists, creating empty objects as needed.
fn ensure_json_path(val: &mut serde_json::Value, keys: &[&str]) {
    let mut cur = val;
    for key in keys {
        if cur.as_object().is_none_or(|o| !o.contains_key(*key))
            && let Some(obj) = cur.as_object_mut()
        {
            obj.insert(
                (*key).to_owned(),
                serde_json::Value::Object(serde_json::Map::new()),
            );
        }
        cur = match cur.as_object_mut().and_then(|o| o.get_mut(*key)) {
            Some(v) => v,
            None => return,
        };
    }
}
