use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::config_json::load_config_json;
use super::style::*;
use crate::{
    cli::PluginsCommand,
    config::loader::base_dir,
    plugin::manifest::{load_manifest, scan_plugins},
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
            banner(&format!("rsclaw plugins marketplace v{}", env!("RSCLAW_BUILD_VERSION")));
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
    banner(&format!("rsclaw plugins v{}", env!("RSCLAW_BUILD_VERSION")));
    let dir = plugins_dir();
    let plugins = scan_plugins(&dir)?;

    if plugins.is_empty() {
        warn_msg(&format!("no plugins installed in {}", dim(&dir.display().to_string())));
        return Ok(());
    }

    println!(
        "  {:<24} {:<10} {}",
        bold("NAME"),
        bold("VERSION"),
        bold("DESCRIPTION")
    );
    for p in &plugins {
        let version = p.version.as_deref().unwrap_or("-");
        let desc = p.description.as_deref().unwrap_or("-");
        println!("  {:<24} {:<10} {}", cyan(&p.name), dim(version), desc);
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
        .with_context(|| format!("no valid openclaw.plugin.json in {}", src.display()))?;

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

    // Expect .tar.gz
    if !url.ends_with(".tar.gz") && !url.ends_with(".tgz") {
        bail!("Only .tar.gz archives are supported for URL installs");
    }

    let tmp_dir = tempfile::tempdir().context("create temp dir")?;
    let cursor = std::io::Cursor::new(bytes);
    let gz = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(tmp_dir.path()).context("unpack tar.gz")?;

    // Find the unpacked plugin dir (top-level dir in the archive)
    let plugin_dir = std::fs::read_dir(tmp_dir.path())?
        .flatten()
        .find(|e| e.path().is_dir())
        .map(|e| e.path())
        .ok_or_else(|| anyhow::anyhow!("no top-level directory found in archive"))?;

    install_from_path(&plugin_dir)
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
    banner(&format!("rsclaw plugins doctor v{}", env!("RSCLAW_BUILD_VERSION")));

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
        let runtime_ok = which::which(&p.runtime).is_ok();
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
            "    {} {:<24} v{}{}",
            status,
            cyan(&p.name),
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

    let manifest_path = plugin_dir.join("openclaw.plugin.json");
    if !manifest_path.exists() {
        bail!(
            "no manifest found for `{name}` at {}",
            manifest_path.display()
        );
    }

    let content = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let val: serde_json::Value =
        serde_json::from_str(&content).context("parse manifest JSON")?;
    println!("{}", serde_json::to_string_pretty(&val)?);
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
    banner(&format!("rsclaw plugins update v{}", env!("RSCLAW_BUILD_VERSION")));
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
