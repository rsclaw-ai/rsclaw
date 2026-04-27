use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::config_json::load_config_json;
use super::style::*;
use crate::{
    cli::PluginsCommand,
    config::loader::base_dir,
    plugin::manifest::{MANIFEST_FILE, LEGACY_MANIFEST_FILE, load_manifest, scan_plugins},
};

fn plugins_dir() -> PathBuf {
    base_dir().join("plugins")
}

pub async fn cmd_plugins(sub: PluginsCommand) -> Result<()> {
    match sub {
        PluginsCommand::List => plugins_list(),
        PluginsCommand::Info { plugin } => plugins_info(&plugin),
        PluginsCommand::Install { spec } => plugins_install(&spec).await,
        PluginsCommand::Enable { plugin } => plugins_set_enabled(&plugin, true),
        PluginsCommand::Disable { plugin } => plugins_set_enabled(&plugin, false),
        PluginsCommand::Doctor => plugins_doctor(),
        PluginsCommand::Inspect { plugin } => plugins_inspect(&plugin),
        PluginsCommand::Marketplace => {
            banner(&format!("rsclaw plugins marketplace v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
            let url = "https://clawhub.ai/plugins";
            kv("marketplace", &bold(url));
            println!("  {}", dim("Browse and install plugins with: rsclaw plugins install <spec>"));
            Ok(())
        }
        PluginsCommand::Uninstall { plugin } => plugins_uninstall(&plugin),
        PluginsCommand::Update { plugin } => plugins_update(plugin.as_deref()).await,
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn plugins_list() -> Result<()> {
    banner(&format!("rsclaw plugins v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
    let dir = plugins_dir();
    let plugins = scan_plugins(&dir)?;

    if plugins.is_empty() {
        warn_msg(&format!("no plugins installed in {}", dim(&dir.display().to_string())));
        return Ok(());
    }

    println!(
        "  {:<24} {:<10} {:<8} {}",
        bold("NAME"),
        bold("VERSION"),
        bold("RUNTIME"),
        bold("DESCRIPTION")
    );
    for p in &plugins {
        let version = p.version.as_deref().unwrap_or("-");
        let desc = p.description.as_deref().unwrap_or("-");
        println!(
            "  {:<24} {:<10} {:<8} {}",
            cyan(&p.name),
            dim(version),
            dim(&p.runtime),
            desc
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

fn plugins_info(name: &str) -> Result<()> {
    let plugin_dir = plugins_dir().join(name);
    if !plugin_dir.exists() {
        bail!("plugin `{name}` not found in {}", plugins_dir().display());
    }

    let m = load_manifest(&plugin_dir)
        .with_context(|| format!("failed to load manifest for `{name}`"))?;

    banner(&format!("rsclaw plugin: {name}"));
    kv("Name", &cyan(&m.name));
    kv("Version", m.version.as_deref().unwrap_or("-"));
    kv("Description", m.description.as_deref().unwrap_or("-"));
    kv("Runtime", &m.runtime);
    kv("Entry", &m.entry);
    if !m.slots.is_empty() {
        kv("Slots", &m.slots.join(", "));
    }
    if !m.hooks.is_empty() {
        kv("Hooks", &m.hooks.join(", "));
    }
    if !m.tools.is_empty() {
        println!();
        println!("  {} ({}):", bold("Tools"), m.tools.len());
        for t in &m.tools {
            println!("    - {} -- {}", cyan(&t.name), dim(&t.description));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

async fn plugins_install(spec: &str) -> Result<()> {
    std::fs::create_dir_all(plugins_dir())?;

    if spec.starts_with("http://") || spec.starts_with("https://") {
        install_from_url(spec).await
    } else if spec.ends_with(".wasm") {
        install_wasm_file(std::path::Path::new(spec)).await
    } else if spec.ends_with(".zip") {
        install_from_zip(std::path::Path::new(spec))
    } else if spec.ends_with(".tar.gz") || spec.ends_with(".tgz") {
        install_from_tarball(std::path::Path::new(spec))
    } else {
        install_from_path(std::path::Path::new(spec))
    }
}

fn install_from_path(src: &std::path::Path) -> Result<()> {
    if !src.exists() {
        bail!("path not found: {}", src.display());
    }
    if !src.is_dir() {
        bail!("expected a directory, got: {}", src.display());
    }

    let manifest = load_manifest(src)
        .with_context(|| format!("no valid {} or {} in {}", MANIFEST_FILE, LEGACY_MANIFEST_FILE, src.display()))?;

    let dest = plugins_dir().join(&manifest.name);
    if dest.exists() {
        bail!(
            "plugin `{}` already installed. Remove {} first.",
            manifest.name,
            dest.display()
        );
    }

    copy_dir_recursive(src, &dest)
        .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;

    ok(&format!("installed '{}' to {}", cyan(&manifest.name), dim(&dest.display().to_string())));
    Ok(())
}

async fn install_from_url(url: &str) -> Result<()> {
    println!("  {} {}...", dim("downloading"), cyan(url));
    let bytes = reqwest::get(url)
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error for {url}"))?
        .bytes()
        .await
        .context("reading response body")?;

    let tmp_dir = tempfile::tempdir().context("create temp dir")?;

    if url.ends_with(".zip") {
        let cursor = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).context("read zip")?;
        archive.extract(tmp_dir.path()).context("extract zip")?;
    } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        let cursor = std::io::Cursor::new(bytes);
        let gz = flate2::read::GzDecoder::new(cursor);
        let mut archive = tar::Archive::new(gz);
        archive.unpack(tmp_dir.path()).context("unpack tar.gz")?;
    } else {
        bail!("unsupported archive format (expected .zip, .tar.gz, or .tgz)");
    }

    let plugin_dir = find_plugin_dir(tmp_dir.path())?;
    install_from_path(&plugin_dir)
}

/// Install from a local `.zip` archive.
fn install_from_zip(src: &std::path::Path) -> Result<()> {
    if !src.exists() {
        bail!("file not found: {}", src.display());
    }
    let tmp_dir = tempfile::tempdir().context("create temp dir")?;
    let file = std::fs::File::open(src)
        .with_context(|| format!("open {}", src.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("read zip: {}", src.display()))?;
    archive.extract(tmp_dir.path())
        .with_context(|| format!("extract zip: {}", src.display()))?;

    let plugin_dir = find_plugin_dir(tmp_dir.path())?;
    install_from_path(&plugin_dir)
}

/// Install from a local `.tar.gz` archive.
fn install_from_tarball(src: &std::path::Path) -> Result<()> {
    if !src.exists() {
        bail!("file not found: {}", src.display());
    }
    let tmp_dir = tempfile::tempdir().context("create temp dir")?;
    let file = std::fs::File::open(src)
        .with_context(|| format!("open {}", src.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(tmp_dir.path())
        .with_context(|| format!("extract tar.gz: {}", src.display()))?;

    let plugin_dir = find_plugin_dir(tmp_dir.path())?;
    install_from_path(&plugin_dir)
}

/// Find the plugin directory inside an unpacked archive.
///
/// Looks for a manifest file directly or in a top-level subdirectory.
fn find_plugin_dir(root: &std::path::Path) -> Result<std::path::PathBuf> {
    // Check if manifest is directly in root.
    if root.join(MANIFEST_FILE).exists() || root.join(LEGACY_MANIFEST_FILE).exists() {
        return Ok(root.to_path_buf());
    }
    // Check top-level subdirectories (e.g. `package/`).
    for entry in std::fs::read_dir(root)?.flatten() {
        let path = entry.path();
        if path.is_dir()
            && (path.join(MANIFEST_FILE).exists()
                || path.join(LEGACY_MANIFEST_FILE).exists())
        {
            return Ok(path);
        }
    }
    bail!("no plugin manifest found in archive")
}

/// Install a `.wasm` plugin: load it to read manifest, create directory, generate `plugin.json5`.
async fn install_wasm_file(src: &std::path::Path) -> Result<()> {
    if !src.exists() {
        bail!("file not found: {}", src.display());
    }

    let filename = src
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid file name"))?;

    // Load the wasm to read its embedded manifest (name, version, description, tools).
    println!("  {} loading WASM manifest...", dim("*"));
    let mut wasm_config = wasmtime::Config::new();
    wasm_config.async_support(true);
    let engine = wasmtime::Engine::new(&wasm_config)
        .context("create wasmtime engine")?;

    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("plugin")
        .to_owned();

    // Build a temporary manifest pointing at the source file so load_wasm_plugin can find it.
    let tmp_manifest = crate::plugin::PluginManifest {
        name: stem.clone(),
        id: None,
        version: None,
        description: None,
        runtime: "wasm".to_owned(),
        entry: src.to_string_lossy().to_string(),
        channels: vec![],
        slots: vec![],
        hooks: vec![],
        tools: vec![],
        min_call_interval_ms: 0,
        requires_rsclaw: None,
        browser_cdn: Default::default(),
        extra: Default::default(),
        dir: std::path::PathBuf::from("."),
    };

    let browser = std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let wasm_plugin = crate::plugin::load_wasm_plugin(&tmp_manifest, &engine, browser).await;

    let (name, version, description, tools_count) = match wasm_plugin {
        Ok(ref wp) => (
            wp.name.clone(),
            wp.version.clone().unwrap_or_default(),
            wp.description.clone().unwrap_or_default(),
            wp.tools.len(),
        ),
        Err(ref e) => {
            warn_msg(&format!("could not read WASM manifest: {e:#}"));
            (stem.clone(), String::new(), String::new(), 0)
        }
    };

    let dest_dir = plugins_dir().join(&name);
    if dest_dir.exists() {
        bail!(
            "plugin `{name}` already installed at {}. Remove it first.",
            dest_dir.display()
        );
    }
    std::fs::create_dir_all(&dest_dir)?;

    // Copy .wasm file
    std::fs::copy(src, dest_dir.join(filename))
        .with_context(|| format!("copy {} -> {}", src.display(), dest_dir.display()))?;

    // Generate plugin.json5 with data from the wasm manifest.
    let ver = if version.is_empty() { "0.1.0" } else { &version };
    let desc_line = if description.is_empty() {
        String::new()
    } else {
        format!("\n  description: {:?},", description)
    };
    let manifest_content = format!(
        r#"{{
  name: "{name}",
  version: "{ver}",{desc_line}
  runtime: "wasm",
  entry: "./{filename}",
}}
"#
    );
    std::fs::write(dest_dir.join(MANIFEST_FILE), &manifest_content)?;

    ok(&format!(
        "installed WASM plugin '{}' v{} ({} tools) to {}",
        cyan(&name),
        dim(ver),
        tools_count,
        dim(&dest_dir.display().to_string())
    ));
    Ok(())
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// enable / disable
// ---------------------------------------------------------------------------

fn plugins_set_enabled(name: &str, enabled: bool) -> Result<()> {
    // Verify the plugin is installed
    let plugin_dir = plugins_dir().join(name);
    if !plugin_dir.exists() {
        bail!("plugin `{name}` not found. Install it first.");
    }

    let (path, mut val) = load_config_json()
        .map_err(|_| anyhow::anyhow!("No config file found. Run `rsclaw onboard` first."))?;

    // Ensure plugins.entries.<name>.enabled exists
    {
        let root = val
            .as_object_mut()
            .context("config root is not an object")?;
        let plugins = root
            .entry("plugins")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("plugins is not an object")?;
        let entries = plugins
            .entry("entries")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("plugins.entries is not an object")?;
        let entry = entries
            .entry(name.to_owned())
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("plugin entry is not an object")?;
        entry.insert("enabled".to_owned(), serde_json::json!(enabled));
    }

    std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
    if enabled {
        ok(&format!("plugin '{}' enabled", cyan(name)));
    } else {
        ok(&format!("plugin '{}' disabled", cyan(name)));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

fn plugins_doctor() -> Result<()> {
    banner(&format!("rsclaw plugins doctor v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));

    // Check runtimes
    let runtimes = [
        ("node", "Node.js"),
        ("bun", "Bun"),
        ("deno", "Deno"),
        ("python3", "Python 3"),
        ("python", "Python"),
    ];

    println!("  {}:", bold("Runtimes"));
    let tools_base = crate::config::loader::base_dir().join("tools");
    for (bin, label) in &runtimes {
        // Check tools dir first
        let tools_bin = tools_base.join(bin).join("bin").join(bin);
        match if tools_bin.exists() { Ok(tools_bin) } else { which::which(bin) } {
            Ok(path) => {
                // Try to get version
                let version = std::process::Command::new(bin)
                    .arg("--version")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "(unknown version)".to_string());
                println!(
                    "    {} {:<10} {} -- {}",
                    green("[ok]"),
                    label,
                    dim(&path.display().to_string()),
                    dim(&version)
                );
            }
            Err(_) => println!("    {} {:<10} {}", dim("[--]"), label, dim("not found")),
        }
    }

    println!();

    // Check installed plugins
    let plugins = scan_plugins(&plugins_dir())?;
    if plugins.is_empty() {
        warn_msg("no plugins installed");
        return Ok(());
    }

    println!("  {} ({}):", bold("Plugins"), plugins.len());
    for p in &plugins {
        let runtime_ok = if p.is_wasm() {
            true // WASM runtime is built-in
        } else {
            which::which(&p.runtime).is_ok()
        };
        let status = if runtime_ok {
            green("[ok]")
        } else {
            red("[!!]")
        };
        let note = if runtime_ok {
            String::new()
        } else {
            format!(" -- {}", red(&format!("runtime `{}` not found", p.runtime)))
        };
        println!(
            "    {} {:<24} {:<8} v{}{}",
            status,
            cyan(&p.name),
            dim(&p.runtime),
            p.version.as_deref().unwrap_or("?"),
            note
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

fn plugins_inspect(name: &str) -> Result<()> {
    let plugin_dir = plugins_dir().join(name);
    if !plugin_dir.exists() {
        bail!("plugin `{name}` not found in {}", plugins_dir().display());
    }

    // Try plugin.json5 first, then legacy
    let manifest_path = {
        let json5 = plugin_dir.join(MANIFEST_FILE);
        let legacy = plugin_dir.join(LEGACY_MANIFEST_FILE);
        if json5.exists() {
            json5
        } else if legacy.exists() {
            legacy
        } else {
            bail!("no manifest found for `{name}`");
        }
    };

    let content = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;

    // For json5, parse and re-serialize as pretty JSON for display.
    if manifest_path.ends_with(MANIFEST_FILE) {
        let val: serde_json::Value = json5::from_str(&content)
            .with_context(|| format!("parse {}", manifest_path.display()))?;
        println!("{}", serde_json::to_string_pretty(&val)?);
    } else {
        let val: serde_json::Value =
            serde_json::from_str(&content).context("parse manifest JSON")?;
        println!("{}", serde_json::to_string_pretty(&val)?);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

fn plugins_uninstall(name: &str) -> Result<()> {
    let plugin_dir = plugins_dir().join(name);
    if !plugin_dir.exists() {
        bail!("plugin `{name}` not found in {}", plugins_dir().display());
    }

    std::fs::remove_dir_all(&plugin_dir)
        .with_context(|| format!("remove {}", plugin_dir.display()))?;

    // Remove from config if present.
    if let Ok((path, mut val)) = load_config_json() {
        if let Some(entries) = val
            .pointer_mut("/plugins/entries")
            .and_then(|v| v.as_object_mut())
        {
            entries.remove(name);
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&val).unwrap_or_default());
        }
    }

    ok(&format!("uninstalled plugin '{}'", cyan(name)));
    Ok(())
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

async fn plugins_update(name: Option<&str>) -> Result<()> {
    banner(&format!("rsclaw plugins update v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
    let dir = plugins_dir();
    let plugins = scan_plugins(&dir)?;

    let to_update: Vec<_> = if let Some(name) = name {
        plugins.into_iter().filter(|p| p.name == name).collect()
    } else {
        plugins
    };

    if to_update.is_empty() {
        warn_msg("no plugins to update");
        return Ok(());
    }

    for p in &to_update {
        println!(
            "  {} v{} -- re-install to update.",
            cyan(&p.name),
            p.version.as_deref().unwrap_or("?")
        );
    }
    println!("  {}", dim("Use `rsclaw plugins install <spec>` to update a specific plugin."));
    Ok(())
}
