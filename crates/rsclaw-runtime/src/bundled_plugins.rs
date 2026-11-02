//! Install the plugins a brand bundles with its install package.
//!
//! The brand file's directory may carry `plugins/<name>/` (see
//! `docs/branding.md`); `rsclaw_brand::BUNDLED_PLUGINS` records each one's
//! name, version and content digest at build time. Packages ship the files
//! next to the engine; on setup and on every gateway start the copy is
//! verified against the recorded digest and installed into
//! `$base_dir/plugins/` when missing or older. A newer installed version is
//! never downgraded. Builds without bundled plugins do nothing here.
//!
//! Trust: a bundled plugin is part of the signed install package, so it skips
//! the hub allowlist that gates installs from the network; the WASM sandbox,
//! host capability checks and `host.cli` allowlist still apply at run time.

use std::path::{Path, PathBuf};

use rsclaw_brand::{BUNDLED_PLUGINS, BundledPlugin, plugin_digest};

/// Marker written into an installed bundled plugin.
pub const MARKER_FILE: &str = ".rsclaw-bundled.json";

/// Override for the directory holding bundled plugins (tests, unusual
/// package layouts).
pub const DIR_ENV: &str = "RSCLAW_BUNDLED_PLUGINS_DIR";

/// What happened to one bundled plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed { version: String },
    Upgraded { from: String, to: String },
    UpToDate,
    /// The installed copy is newer than the bundled one; left alone.
    KeptNewer { installed: String },
    /// The package does not contain the plugin's files.
    Missing,
    /// The shipped files do not match the digest recorded at build time.
    DigestMismatch { found: String },
    Failed(String),
}

/// True when `name` is one of this build's bundled plugins.
pub fn is_bundled(name: &str) -> bool {
    BUNDLED_PLUGINS.iter().any(|p| p.name == name)
}

/// Where the install package keeps bundled plugins: `$RSCLAW_BUNDLED_PLUGINS_DIR`,
/// else next to the engine binary (macOS `../Resources/plugins`, desktop
/// packages `../plugins`, or `./plugins`).
pub fn source_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DIR_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let bin_dir = exe.parent()?;
    [
        bin_dir.join("../Resources/plugins"),
        bin_dir.join("../plugins"),
        bin_dir.join("plugins"),
    ]
    .into_iter()
    .find(|p| p.is_dir())
}

/// Install or upgrade every bundled plugin into `plugins_dir`. No-op (and no
/// file system access) when the build bundles none.
pub fn install_all(plugins_dir: &Path) -> Vec<(&'static str, Outcome)> {
    if BUNDLED_PLUGINS.is_empty() {
        return Vec::new();
    }
    let Some(src_root) = source_dir() else {
        tracing::warn!("bundled plugins: install package has no plugins directory");
        return BUNDLED_PLUGINS
            .iter()
            .map(|p| (p.name, Outcome::Missing))
            .collect();
    };
    BUNDLED_PLUGINS
        .iter()
        .map(|p| {
            let outcome = install_one(p, &src_root, plugins_dir);
            log_outcome(p.name, &outcome);
            (p.name, outcome)
        })
        .collect()
}

fn log_outcome(name: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Installed { version } => {
            tracing::info!(plugin = name, version, "bundled plugin installed")
        }
        Outcome::Upgraded { from, to } => {
            tracing::info!(plugin = name, from, to, "bundled plugin upgraded")
        }
        Outcome::UpToDate => tracing::debug!(plugin = name, "bundled plugin up to date"),
        Outcome::KeptNewer { installed } => tracing::info!(
            plugin = name,
            installed,
            "installed plugin is newer than the bundled one; keeping it"
        ),
        Outcome::Missing => {
            tracing::warn!(plugin = name, "bundled plugin files missing from the install package")
        }
        Outcome::DigestMismatch { found } => tracing::warn!(
            plugin = name,
            found,
            "bundled plugin does not match this build's digest; refusing to install"
        ),
        Outcome::Failed(e) => tracing::warn!(plugin = name, error = %e, "bundled plugin install failed"),
    }
}

/// Install `plugin` from `src_root/<name>` into `plugins_dir/<name>`.
pub fn install_one(plugin: &BundledPlugin, src_root: &Path, plugins_dir: &Path) -> Outcome {
    let src = src_root.join(plugin.name);
    if !src.is_dir() {
        return Outcome::Missing;
    }
    match plugin_digest::tree_digest(&src) {
        Ok(found) if found == plugin.digest => {}
        Ok(found) => return Outcome::DigestMismatch { found },
        Err(e) => return Outcome::Failed(format!("hash {}: {e}", src.display())),
    }

    let dest = plugins_dir.join(plugin.name);
    let installed = installed_version(&dest);
    let previous = match installed {
        None => None,
        Some(v) => match plugin_digest::version_cmp(&v, plugin.version) {
            std::cmp::Ordering::Less => Some(v),
            std::cmp::Ordering::Equal => {
                // Same version: repair a copy that lost its marker or was
                // edited, otherwise nothing to do.
                if marker_digest(&dest).as_deref() == Some(plugin.digest)
                    && plugin_digest::tree_digest(&dest).ok().as_deref() == Some(plugin.digest)
                {
                    return Outcome::UpToDate;
                }
                Some(v)
            }
            std::cmp::Ordering::Greater => return Outcome::KeptNewer { installed: v },
        },
    };

    if let Err(e) = replace_dir(&src, &dest, plugins_dir, plugin) {
        return Outcome::Failed(format!("{e:#}"));
    }
    match previous {
        None => Outcome::Installed { version: plugin.version.to_owned() },
        Some(from) => Outcome::Upgraded { from, to: plugin.version.to_owned() },
    }
}

/// Version of an installed plugin from its manifest, if it is installed.
fn installed_version(dest: &Path) -> Option<String> {
    if !dest.is_dir() {
        return None;
    }
    Some(
        rsclaw_plugin::manifest::load_manifest(dest)
            .ok()
            .and_then(|m| m.version)
            .unwrap_or_else(|| "0".to_owned()),
    )
}

fn marker_digest(dest: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dest.join(MARKER_FILE)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("digest").and_then(|d| d.as_str()).map(str::to_owned)
}

/// Copy `src` into a staging dir, then swap it in for `dest` so a failed
/// copy never leaves a half-installed plugin.
fn replace_dir(
    src: &Path,
    dest: &Path,
    plugins_dir: &Path,
    plugin: &BundledPlugin,
) -> anyhow::Result<()> {
    use anyhow::Context;

    std::fs::create_dir_all(plugins_dir)
        .with_context(|| format!("create {}", plugins_dir.display()))?;
    let pid = std::process::id();
    let staging = plugins_dir.join(format!(".{}.staging-{pid}", plugin.name));
    let old = plugins_dir.join(format!(".{}.old-{pid}", plugin.name));
    let _ = std::fs::remove_dir_all(&staging);
    copy_tree(src, &staging).with_context(|| format!("copy {}", src.display()))?;
    let marker = serde_json::json!({
        "name": plugin.name,
        "version": plugin.version,
        "digest": plugin.digest,
        "bundled": true,
    });
    std::fs::write(staging.join(MARKER_FILE), serde_json::to_string_pretty(&marker)?)?;

    if dest.exists() {
        std::fs::rename(dest, &old).with_context(|| format!("move aside {}", dest.display()))?;
    }
    if let Err(e) = std::fs::rename(&staging, dest) {
        if old.exists() {
            let _ = std::fs::rename(&old, dest);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e).with_context(|| format!("install {}", dest.display()));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if plugin_digest::DIGEST_IGNORED.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path, name: &str, version: &str, wasm: &[u8]) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("plugin.json5"),
            format!("{{ name: \"{name}\", version: \"{version}\", runtime: \"wasm\", entry: \"./p.wasm\" }}"),
        )
        .expect("manifest");
        std::fs::write(dir.join("p.wasm"), wasm).expect("wasm");
        dir
    }

    fn spec(name: &'static str, version: &'static str, dir: &Path) -> BundledPlugin {
        let digest: &'static str =
            Box::leak(plugin_digest::tree_digest(dir).expect("digest").into_boxed_str());
        BundledPlugin { name, version, digest }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rsclaw-bundled-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    #[test]
    fn installs_then_reports_up_to_date() {
        let root = tmp("install");
        let src = root.join("src");
        let dir = fixture(&src, "demo", "0.2.0", b"\0asm-a");
        let p = spec("demo", "0.2.0", &dir);
        let plugins = root.join("plugins");
        assert_eq!(install_one(&p, &src, &plugins), Outcome::Installed { version: "0.2.0".into() });
        assert!(plugins.join("demo/p.wasm").is_file());
        assert!(plugins.join("demo").join(MARKER_FILE).is_file());
        assert_eq!(install_one(&p, &src, &plugins), Outcome::UpToDate);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_tampered_package() {
        let root = tmp("tamper");
        let src = root.join("src");
        let dir = fixture(&src, "demo", "0.2.0", b"\0asm-a");
        let p = spec("demo", "0.2.0", &dir);
        std::fs::write(dir.join("p.wasm"), b"\0asm-evil").expect("tamper");
        let plugins = root.join("plugins");
        assert!(matches!(install_one(&p, &src, &plugins), Outcome::DigestMismatch { .. }));
        assert!(!plugins.join("demo").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn upgrades_older_and_keeps_newer() {
        let root = tmp("upgrade");
        let plugins = root.join("plugins");
        fixture(&plugins, "demo", "0.1.0", b"\0asm-old");
        let src = root.join("src");
        let dir = fixture(&src, "demo", "0.2.0", b"\0asm-new");
        let p = spec("demo", "0.2.0", &dir);
        assert_eq!(
            install_one(&p, &src, &plugins),
            Outcome::Upgraded { from: "0.1.0".into(), to: "0.2.0".into() }
        );
        assert_eq!(std::fs::read(plugins.join("demo/p.wasm")).expect("read"), b"\0asm-new");

        fixture(&plugins, "demo", "0.9.0", b"\0asm-user");
        assert_eq!(
            install_one(&p, &src, &plugins),
            Outcome::KeptNewer { installed: "0.9.0".into() }
        );
        assert_eq!(std::fs::read(plugins.join("demo/p.wasm")).expect("read"), b"\0asm-user");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn repairs_edited_copy_of_same_version() {
        let root = tmp("repair");
        let src = root.join("src");
        let dir = fixture(&src, "demo", "0.2.0", b"\0asm-a");
        let p = spec("demo", "0.2.0", &dir);
        let plugins = root.join("plugins");
        install_one(&p, &src, &plugins);
        std::fs::write(plugins.join("demo/p.wasm"), b"\0asm-edited").expect("edit");
        assert_eq!(
            install_one(&p, &src, &plugins),
            Outcome::Upgraded { from: "0.2.0".into(), to: "0.2.0".into() }
        );
        assert_eq!(std::fs::read(plugins.join("demo/p.wasm")).expect("read"), b"\0asm-a");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_package_files() {
        let root = tmp("missing");
        let p = BundledPlugin { name: "demo", version: "0.1.0", digest: "sha256:00" };
        assert_eq!(install_one(&p, &root.join("src"), &root.join("plugins")), Outcome::Missing);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn default_build_bundles_nothing() {
        if rsclaw_brand::ID != "rsclaw" {
            return;
        }
        assert!(BUNDLED_PLUGINS.is_empty());
        assert!(install_all(Path::new("/nonexistent")).is_empty());
        assert!(!is_bundled("anything"));
    }
}
