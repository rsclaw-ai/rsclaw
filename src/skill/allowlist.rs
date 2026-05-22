//! Auto-install allowlist — the security gate for the `skill_install` agent
//! tool. The agent may AUTO-install only audited, content-pinned skills listed
//! here; everything else needs the user (CLI / confirmation). The CLI
//! `rsclaw skills install` is human-initiated and bypasses this gate.
//!
//! Source: `https://api.rsclaw.ai/v1/hub/allowlist/{meta,skills,plugins}.json`,
//! mirrored to `~/.rsclaw/allowlist/`. Fail-closed: on fetch failure with no
//! cache the allowlist is EMPTY → all agent auto-installs are blocked.
//!
//! Design: docs/plans/2026-05-21-skill-allowlist.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Hub root. Layout: `/meta.json` (signed), `/skills/manifest.json`,
/// `/plugins/manifest.json`, `/tools/manifest.json`.
const HUB_BASE: &str = "https://hub.rsclaw.ai";

/// One audited entry. `sha256` pins the audited SKILL.md (same convention as
/// the clawhub lockfile); empty means "not content-pinned yet" → slug-gate only.
#[derive(Debug, Clone, Deserialize)]
pub struct AllowEntry {
    pub slug: String,
    /// Direct download URL for the audited package (zip). The agent installs
    /// ONLY from here — never resolved through a public registry — so it gets
    /// exactly the hub-hosted, audited bytes.
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub registry: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub audited_at: String,
    /// One-line description (from SKILL.md frontmatter / plugin manifest, written
    /// by the hub packager). Empty on older manifests.
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Default)]
pub struct Allowlist {
    skills: HashMap<String, AllowEntry>,
    plugins: HashMap<String, AllowEntry>,
}

impl Allowlist {
    pub fn lookup_skill(&self, slug: &str) -> Option<AllowEntry> {
        self.skills.get(slug).cloned()
    }
    pub fn lookup_plugin(&self, slug: &str) -> Option<AllowEntry> {
        self.plugins.get(slug).cloned()
    }
    pub fn counts(&self) -> (usize, usize) {
        (self.skills.len(), self.plugins.len())
    }
    /// All audited skill entries, sorted by slug (stable for the catalog API).
    pub fn skills_sorted(&self) -> Vec<AllowEntry> {
        let mut v: Vec<_> = self.skills.values().cloned().collect();
        v.sort_by(|a, b| a.slug.cmp(&b.slug));
        v
    }
    /// All audited plugin entries, sorted by slug.
    pub fn plugins_sorted(&self) -> Vec<AllowEntry> {
        let mut v: Vec<_> = self.plugins.values().cloned().collect();
        v.sort_by(|a, b| a.slug.cmp(&b.slug));
        v
    }
}

#[derive(Deserialize, Default)]
struct SkillsFile {
    #[serde(default)]
    skills: Vec<AllowEntry>,
}
#[derive(Deserialize, Default)]
struct PluginsFile {
    #[serde(default)]
    plugins: Vec<AllowEntry>,
}
#[derive(Deserialize, Default)]
struct Meta {
    #[serde(default)]
    version: String,
    #[serde(default)]
    sha256: MetaSha,
    /// ed25519 signature (base64) over the canonical payload — see sig.rs.
    #[serde(default)]
    sig: String,
}
#[derive(Deserialize, Default)]
struct MetaSha {
    #[serde(default)]
    skills: String,
    #[serde(default)]
    plugins: String,
    #[serde(default)]
    tools: String,
}

static CURRENT: LazyLock<RwLock<Arc<Allowlist>>> =
    LazyLock::new(|| RwLock::new(Arc::new(Allowlist::default())));

/// Current snapshot (cheap Arc clone). Empty until loaded → fail-closed.
pub fn snapshot() -> Arc<Allowlist> {
    CURRENT.read().expect("allowlist lock").clone()
}

fn set(a: Allowlist) {
    *CURRENT.write().expect("allowlist lock") = Arc::new(a);
}

fn cache_dir() -> PathBuf {
    crate::config::loader::base_dir().join("allowlist")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn parse(skills_json: &str, plugins_json: &str) -> Allowlist {
    let skills = serde_json::from_str::<SkillsFile>(skills_json)
        .map(|f| f.skills)
        .unwrap_or_default();
    let plugins = serde_json::from_str::<PluginsFile>(plugins_json)
        .map(|f| f.plugins)
        .unwrap_or_default();
    Allowlist {
        skills: skills.into_iter().map(|e| (e.slug.clone(), e)).collect(),
        plugins: plugins.into_iter().map(|e| (e.slug.clone(), e)).collect(),
    }
}

/// Load from the local cache (sync, fast) so the gate has data before the first
/// network refresh completes. Returns the (skills, plugins) counts loaded.
pub fn load_cached() -> (usize, usize) {
    let d = cache_dir();
    let s = std::fs::read_to_string(d.join("skills.json")).unwrap_or_default();
    let p = std::fs::read_to_string(d.join("plugins.json")).unwrap_or_default();
    if s.is_empty() && p.is_empty() {
        return (0, 0);
    }
    let a = parse(&s, &p);
    let counts = a.counts();
    set(a);
    counts
}

/// Verify the hub's ed25519 signature on `meta.json` against the pinned public
/// key. Fail-closed (empty/invalid sig → error). This is the trust anchor: a
/// compromised hub can't forge a signature, so the meta hashes (and thus the
/// lists they pin) are authentic.
fn verify_meta_signature(meta: &str) -> Result<()> {
    let m: Meta = serde_json::from_str(meta).context("parse allowlist meta.json")?;
    crate::skill::sig::verify_meta_sig(
        &m.version, &m.sha256.skills, &m.sha256.plugins, &m.sha256.tools, &m.sig,
    )
}

/// Verify each list against `meta.sha256` (the signed hashes — see
/// `verify_meta_signature`, which must pass first).
fn verify_against_meta(meta: &str, skills: &str, plugins: &str) -> Result<()> {
    let m: Meta = serde_json::from_str(meta).context("parse allowlist meta.json")?;
    if !m.sha256.skills.is_empty() && sha256_hex(skills.as_bytes()) != m.sha256.skills {
        anyhow::bail!("allowlist skills.json sha256 != meta.sha256.skills");
    }
    if !m.sha256.plugins.is_empty() && sha256_hex(plugins.as_bytes()) != m.sha256.plugins {
        anyhow::bail!("allowlist plugins.json sha256 != meta.sha256.plugins");
    }
    let _ = m.version;
    Ok(())
}

/// Fetch from the hub, verify list integrity, mirror to cache, update snapshot.
/// Fail-closed: any error keeps the existing snapshot (cache or empty).
pub async fn refresh() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let fetch = |path: &'static str| {
        let url = format!("{HUB_BASE}/{path}");
        let c = client.clone();
        async move {
            c.get(&url)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await
                .map_err(anyhow::Error::from)
        }
    };
    let meta_txt = fetch("meta.json").await.context("fetch hub meta.json")?;
    let skills_txt = fetch("skills/manifest.json").await.context("fetch hub skills/manifest.json")?;
    let plugins_txt = fetch("plugins/manifest.json").await.context("fetch hub plugins/manifest.json")?;
    // Trust anchor first: the meta signature, then the lists match the signed hashes.
    verify_meta_signature(&meta_txt)?;
    verify_against_meta(&meta_txt, &skills_txt, &plugins_txt)?;

    let d = cache_dir();
    let _ = std::fs::create_dir_all(&d);
    let _ = std::fs::write(d.join("meta.json"), &meta_txt);
    let _ = std::fs::write(d.join("skills.json"), &skills_txt);
    let _ = std::fs::write(d.join("plugins.json"), &plugins_txt);

    let a = parse(&skills_txt, &plugins_txt);
    let (ns, np) = a.counts();
    set(a);
    tracing::info!(skills = ns, plugins = np, "allowlist refreshed from hub");
    Ok(())
}

/// Verify a freshly-installed skill's SKILL.md matches the audited hash, so a
/// registry can't serve different content under an audited slug. No-op when the
/// entry isn't content-pinned yet.
///
/// NOTE: pins SKILL.md (the audited contract + CLI the agent runs), matching the
/// existing clawhub lockfile hash. Hashing `scripts/` too is a hardening
/// follow-up tracked in the allowlist plan.
pub fn verify_skill_content(
    install_dir: &Path,
    entry: &AllowEntry,
    require_pin: bool,
) -> Result<()> {
    if entry.sha256.is_empty() {
        // Fail-closed on the agent auto-install path: an un-pinned entry is not
        // "audited, content-pinned", so the agent must not install it. Only the
        // human CLI path (require_pin = false) may proceed without a hash.
        if require_pin {
            anyhow::bail!(
                "allowlist entry '{}' has no sha256 content pin — agent auto-install \
                 requires a pinned hash; install it via the CLI instead",
                entry.slug
            );
        }
        return Ok(());
    }
    let md = install_dir.join("SKILL.md");
    let data = std::fs::read(&md).with_context(|| format!("read {}", md.display()))?;
    let got = sha256_hex(&data);
    if got != entry.sha256 {
        anyhow::bail!(
            "audited-hash mismatch for '{}': SKILL.md content changed since audit \
             (got {}, expected {})",
            entry.slug,
            &got[..got.len().min(12)],
            &entry.sha256[..entry.sha256.len().min(12)],
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_lookup() {
        let s = r#"{"skills":[{"slug":"hithink-market-query","sha256":"abc","registry":"iwencai"}]}"#;
        let a = parse(s, "{}");
        assert!(a.lookup_skill("hithink-market-query").is_some());
        assert!(a.lookup_skill("not-listed").is_none());
        assert_eq!(a.counts(), (1, 0));
    }

    #[test]
    fn meta_sha_mismatch_rejected() {
        let skills = r#"{"skills":[]}"#;
        let real = sha256_hex(skills.as_bytes());
        let good = format!(r#"{{"version":"v1","sha256":{{"skills":"{real}"}}}}"#);
        assert!(verify_against_meta(&good, skills, "{}").is_ok());
        let bad = r#"{"version":"v1","sha256":{"skills":"deadbeef"}}"#;
        assert!(verify_against_meta(bad, skills, "{}").is_err());
    }

    #[test]
    fn meta_signature_verified_and_tamper_rejected() {
        // Golden vector signed by hub-rsclaw-dist.py (same key as HUB_PUBKEY):
        // payload version=2026-01-01.000000 skills=AAAA plugins=BBBB tools=CCCC.
        const SIG: &str = "DGm3uiJwAeYeyi1km20QNFk8rrUD33QgAJYhgcuPlgF3JOXTUViZBDtkhys5D6wTfSvdBpgyAc7zWRuLdBCiCg==";
        let good = format!(
            r#"{{"version":"2026-01-01.000000","sha256":{{"skills":"AAAA","plugins":"BBBB","tools":"CCCC"}},"sig":"{SIG}"}}"#
        );
        assert!(verify_meta_signature(&good).is_ok());
        // Tamper a hash → signature no longer matches.
        let bad = format!(
            r#"{{"version":"2026-01-01.000000","sha256":{{"skills":"XXXX","plugins":"BBBB","tools":"CCCC"}},"sig":"{SIG}"}}"#
        );
        assert!(verify_meta_signature(&bad).is_err());
        // No signature → fail-closed.
        let nosig = r#"{"version":"2026-01-01.000000","sha256":{"skills":"AAAA","plugins":"BBBB","tools":"CCCC"}}"#;
        assert!(verify_meta_signature(nosig).is_err());
    }

    #[test]
    fn empty_sha_enforcement_depends_on_require_pin() {
        let e = AllowEntry {
            slug: "x".into(),
            url: String::new(),
            registry: String::new(),
            version: String::new(),
            sha256: String::new(),
            publisher: String::new(),
            audited_at: String::new(),
            description: String::new(),
        };
        let dir = std::path::Path::new("/nonexistent");
        // Human CLI path (require_pin = false): no sha256 → skip, legacy behavior.
        assert!(verify_skill_content(dir, &e, false).is_ok());
        // Agent auto-install path (require_pin = true): an un-pinned entry must
        // be rejected — the agent may only install content-pinned skills.
        assert!(verify_skill_content(dir, &e, true).is_err());
    }
}
