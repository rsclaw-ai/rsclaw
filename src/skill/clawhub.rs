//! clawhub.ai HTTP client and `lock.json` persistence.
//!
//! clawhub is the skill marketplace for the OpenClaw ecosystem.
//! rsclaw supports:
//!   - Fetching skill metadata by slug
//!   - Downloading and installing skills to `~/.rsclaw/skills/<slug>/`
//!   - Reading/writing `~/.rsclaw/skills/.clawhub/lock.json`

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Lock file
// ---------------------------------------------------------------------------

pub const LOCK_FILE_SUBDIR: &str = ".clawhub";
pub const LOCK_FILE_NAME: &str = "lock.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LockFile {
    pub version: u32,
    pub updated: Option<DateTime<Utc>>,
    pub skills: HashMap<String, LockedSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedSkill {
    /// Skill slug, e.g. "web-search".
    pub slug: String,
    /// Installed version string.
    pub version: String,
    /// When the lock entry was created/updated.
    pub resolved_at: DateTime<Utc>,
    /// Where the skill was fetched from.
    pub source: SkillSource,
    /// SHA-256 hex of the installed `SKILL.md`.
    pub checksum: String,
    /// Absolute path to the installed skill directory.
    pub install_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SkillSource {
    Clawhub,
    Skillhub,
    Github,
    Url,
    Local,
}

impl LockFile {
    /// Read the lock file from `skills_dir/.clawhub/lock.json`.
    /// Returns an empty `LockFile` if the file does not exist.
    pub fn read(skills_dir: &Path) -> Result<Self> {
        let path = lock_path(skills_dir);
        if !path.exists() {
            return Ok(LockFile {
                version: 1,
                updated: None,
                skills: HashMap::new(),
            });
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read lock file: {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("invalid lock file: {}", path.display()))
    }

    /// Atomically write the lock file.
    pub fn write(&self, skills_dir: &Path) -> Result<()> {
        let dir = skills_dir.join(LOCK_FILE_SUBDIR);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;

        let path = lock_path(skills_dir);
        let tmp_path = path.with_extension("json.tmp");

        let contents = serde_json::to_string_pretty(self).context("serialize lock")?;
        std::fs::write(&tmp_path, contents)
            .with_context(|| format!("write tmp lock: {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("rename lock: {}", path.display()))?;

        debug!(path = %path.display(), "lock file written");
        Ok(())
    }
}

fn lock_path(skills_dir: &Path) -> PathBuf {
    skills_dir.join(LOCK_FILE_SUBDIR).join(LOCK_FILE_NAME)
}

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

const CLAWHUB_API_BASE: &str = "https://clawhub.ai/api";

/// Load skillhub URLs from defaults.toml (compiled-in fallback).
fn skillhub_urls() -> SkillhubUrls {
    static URLS: std::sync::LazyLock<SkillhubUrls> = std::sync::LazyLock::new(|| {
        #[derive(serde::Deserialize, Default)]
        struct Defs {
            #[serde(default)]
            skill_registries: std::collections::HashMap<String, toml::Value>,
        }
        let defaults_str = crate::config::loader::load_defaults_toml();
        let defs: Defs = toml::from_str(&defaults_str).unwrap_or_default();
        if let Some(sh) = defs.skill_registries.get("skillhub") {
            SkillhubUrls {
                index: sh
                    .get("index_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                download: sh
                    .get("download_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                search: sh
                    .get("search_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                primary_download: sh
                    .get("primary_download_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
            }
        } else {
            SkillhubUrls::default()
        }
    });
    URLS.clone()
}

#[derive(Clone, Default)]
struct SkillhubUrls {
    index: String,
    download: String,
    search: String,
    primary_download: String,
}

/// Raw API response from clawhub `/v1/skills/<slug>`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClawhubSkillResponse {
    skill: ClawhubSkillData,
    latest_version: Option<ClawhubVersionData>,
    owner: Option<ClawhubOwnerData>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClawhubSkillData {
    slug: String,
    summary: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClawhubVersionData {
    version: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ClawhubOwnerData {
    handle: Option<String>,
}

/// Search result from clawhub.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub slug: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub downloads: Option<u64>,
    pub installs: Option<u64>,
    pub stars: Option<u64>,
}

/// Normalized skill metadata.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub slug: String,
    pub version: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub download_url: String,
}

// ---------------------------------------------------------------------------
// ClawhubClient
// ---------------------------------------------------------------------------

pub struct ClawhubClient {
    client: Client,
    base_url: String,
    token: Option<String>,
}

impl ClawhubClient {
    pub fn new() -> Self {
        let token = std::env::var("CLAWHUB_TOKEN").ok();
        Self {
            client: Client::builder()
                .user_agent(concat!("rsclaw/", env!("RSCLAW_BUILD_VERSION")))
                .build()
                .expect("reqwest client"),
            base_url: CLAWHUB_API_BASE.to_owned(),
            token,
        }
    }

    /// Override the base URL (for testing).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            token: std::env::var("CLAWHUB_TOKEN").ok(),
        }
    }

    /// Fetch skill metadata by slug.
    pub async fn fetch_meta(&self, slug: &str) -> Result<SkillMeta> {
        // clawhub API uses short slug (e.g. "self-improving-agent"),
        // but users may pass "author/slug" from the URL.
        let short_slug = slug.rsplit('/').next().unwrap_or(slug);
        let url = format!("{}/v1/skills/{short_slug}", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.with_context(|| format!("GET {url}"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!("skill `{slug}` not found on clawhub");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("clawhub API error {status}: {body}");
        }

        let raw: ClawhubSkillResponse = resp
            .json()
            .await
            .with_context(|| format!("parse skill meta for `{slug}`"))?;

        let version = raw
            .latest_version
            .map(|v| v.version)
            .unwrap_or_else(|| "latest".to_owned());

        Ok(SkillMeta {
            slug: raw.skill.slug.clone(),
            version,
            description: raw.skill.summary,
            author: raw.owner.and_then(|o| o.handle),
            download_url: format!("{}/v1/download?slug={}", self.base_url, raw.skill.slug),
        })
    }

    /// Search for skills on clawhub.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let url = format!("{}/v1/search?q={}", self.base_url, query);
        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("search clawhub for '{query}'"))?;
        if !resp.status().is_success() {
            anyhow::bail!("clawhub search returned {}", resp.status());
        }
        let body: serde_json::Value = resp.json().await.context("parse search response")?;
        let results = body
            .get("skills")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|item| SearchResult {
                        slug: item["slug"].as_str().unwrap_or("unknown").to_owned(),
                        version: item["version"].as_str().map(|s| s.to_owned()),
                        description: item["summary"]
                            .as_str()
                            .or_else(|| item["description"].as_str())
                            .map(|s| s.to_owned()),
                        downloads: item["downloads"]
                            .as_u64()
                            .or_else(|| item["download_count"].as_u64()),
                        installs: item["installs"]
                            .as_u64()
                            .or_else(|| item["install_count"].as_u64()),
                        stars: item["stars"]
                            .as_u64()
                            .or_else(|| item["favorites"].as_u64())
                            .or_else(|| item["star_count"].as_u64()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(results)
    }

    /// Download and install a skill into `install_dir`.
    ///
    /// The tarball is extracted and a lock entry is written.
    /// Returns the `LockedSkill` record.
    pub async fn install(&self, slug: &str, skills_dir: &Path) -> Result<LockedSkill> {
        let meta = self.fetch_meta(slug).await?;
        // Use the slug's last component as dir name (e.g., "pskoett/foo" → "foo").
        let dir_name = slug.rsplit('/').next().unwrap_or(slug);
        let install_dir = skills_dir.join(dir_name);

        debug!(slug, version = %meta.version, "installing skill from clawhub");

        // Download via the download endpoint (returns ZIP).
        let short_slug = slug.rsplit('/').next().unwrap_or(slug);
        let download_url = if meta.download_url.is_empty() {
            format!("{}/v1/download?slug={short_slug}", self.base_url)
        } else {
            meta.download_url.clone()
        };

        let mut dl_req = self.client.get(&download_url);
        if let Some(ref token) = self.token {
            dl_req = dl_req.bearer_auth(token);
        }
        let bytes = dl_req
            .send()
            .await
            .with_context(|| format!("download {download_url}"))?
            .bytes()
            .await
            .context("read download body")?;

        // Extract into `install_dir`.
        std::fs::create_dir_all(&install_dir)
            .with_context(|| format!("create {}", install_dir.display()))?;

        // Try ZIP first, then fall back to tarball.
        if extract_zip(&bytes, &install_dir).is_err() {
            extract_tarball(&bytes, &install_dir)?;
        }

        // Compute checksum of the installed SKILL.md.
        let skill_md = install_dir.join("SKILL.md");
        let checksum = if skill_md.exists() {
            sha256_file(&skill_md)?
        } else {
            String::new()
        };

        let locked = LockedSkill {
            slug: slug.to_owned(),
            version: meta.version,
            resolved_at: Utc::now(),
            source: SkillSource::Clawhub,
            checksum,
            install_dir,
        };

        // Update lock file.
        let mut lock = LockFile::read(skills_dir).unwrap_or_default();
        lock.skills.insert(slug.to_owned(), locked.clone());
        lock.updated = Some(Utc::now());
        lock.write(skills_dir)?;

        Ok(locked)
    }

    /// Install with fallback: clawhub -> skillhub.
    /// Also supports direct URL and GitHub repo installs.
    pub async fn install_with_fallback(
        &self,
        spec: &str,
        skills_dir: &Path,
    ) -> Result<LockedSkill> {
        // 1. Direct URL (https://)
        if spec.starts_with("http://") || spec.starts_with("https://") {
            return self.install_from_url(spec, skills_dir).await;
        }

        // 2. GitHub repo (owner/repo format with no dots)
        if spec.contains('/') && !spec.contains('.') && !spec.starts_with("@") {
            let url = format!("https://github.com/{}/archive/refs/heads/main.tar.gz", spec);
            info!(spec, url = %url, "resolving as GitHub repo");
            return self.install_from_url(&url, skills_dir).await.map(|mut l| {
                l.source = SkillSource::Github;
                l
            });
        }

        // 3. Try clawhub first
        match self.install(spec, skills_dir).await {
            Ok(locked) => return Ok(locked),
            Err(e) => {
                debug!(slug = spec, error = %e, "clawhub install failed, trying skillhub fallback");
            }
        }

        // 4. Fallback to skillhub (different API: direct ZIP download)
        debug!(slug = spec, "trying skillhub fallback");
        let slug = spec.rsplit('/').next().unwrap_or(spec);
        let sh = skillhub_urls();

        // Try primary download first, then COS fallback
        let primary_url = format!("{}?slug={slug}", sh.primary_download);
        let cos_url = sh.download.replace("{slug}", slug);

        for url in [&primary_url, &cos_url] {
            match self.install_from_url(url, skills_dir).await {
                Ok(mut locked) => {
                    locked.source = SkillSource::Skillhub;
                    locked.slug = slug.to_owned();
                    return Ok(locked);
                }
                Err(e) => {
                    debug!(url, error = %e, "skillhub download attempt failed");
                }
            }
        }

        bail!("skill `{spec}` not found on clawhub or skillhub")
    }

    /// Install a skill from a direct URL (tar.gz or zip).
    async fn install_from_url(&self, url: &str, skills_dir: &Path) -> Result<LockedSkill> {
        // Derive dir name from URL: prefer ?slug= param, then path segment
        let dir_name = url
            .split('?')
            .nth(1)
            .and_then(|qs| qs.split('&').find_map(|p| p.strip_prefix("slug=")))
            .unwrap_or_else(|| {
                url.rsplit('/')
                    .find(|s| !s.is_empty() && !s.contains('?'))
                    .unwrap_or("unknown-skill")
            })
            .trim_end_matches(".tar.gz")
            .trim_end_matches(".tgz")
            .trim_end_matches(".zip");
        let install_dir = skills_dir.join(dir_name);

        debug!(url, dir = %install_dir.display(), "installing skill from URL");

        let bytes = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("download {url}"))?
            .bytes()
            .await
            .context("read download body")?;

        std::fs::create_dir_all(&install_dir)
            .with_context(|| format!("create {}", install_dir.display()))?;

        if extract_zip(&bytes, &install_dir).is_err() {
            extract_tarball(&bytes, &install_dir)?;
        }

        let skill_md = install_dir.join("SKILL.md");
        let checksum = if skill_md.exists() {
            sha256_file(&skill_md)?
        } else {
            String::new()
        };

        let locked = LockedSkill {
            slug: dir_name.to_owned(),
            version: "latest".to_owned(),
            resolved_at: Utc::now(),
            source: SkillSource::Url,
            checksum,
            install_dir,
        };

        let mut lock = LockFile::read(skills_dir).unwrap_or_default();
        lock.skills.insert(dir_name.to_owned(), locked.clone());
        lock.updated = Some(Utc::now());
        lock.write(skills_dir)?;

        Ok(locked)
    }

    /// Search with fallback: clawhub -> skillhub.
    pub async fn search_with_fallback(&self, query: &str) -> Result<Vec<SearchResult>> {
        // Try clawhub first
        match self.search(query).await {
            Ok(results) if !results.is_empty() => return Ok(results),
            _ => {}
        }

        // Fallback: skillhub search API
        let sh = skillhub_urls();
        debug!(query, "searching skillhub");
        let url = format!("{}?q={}", sh.search, urlencoding_encode(query));
        let resp = self.client.get(&url).send().await;
        if let Ok(resp) = resp
            && resp.status().is_success()
        {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let results: Vec<SearchResult> = body
                    .get("skills")
                    .or_else(|| body.get("results"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|item| SearchResult {
                                slug: item["slug"]
                                    .as_str()
                                    .or_else(|| item["name"].as_str())
                                    .unwrap_or("unknown")
                                    .to_owned(),
                                version: item["version"].as_str().map(|s| s.to_owned()),
                                description: item["summary"]
                                    .as_str()
                                    .or_else(|| item["description"].as_str())
                                    .map(|s| s.to_owned()),
                                downloads: item["downloads"]
                                    .as_u64()
                                    .or_else(|| item["download_count"].as_u64()),
                                installs: item["installs"]
                                    .as_u64()
                                    .or_else(|| item["install_count"].as_u64()),
                                stars: item["stars"]
                                    .as_u64()
                                    .or_else(|| item["favorites"].as_u64())
                                    .or_else(|| item["star_count"].as_u64()),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !results.is_empty() {
                    return Ok(results);
                }
            }
        }

        // Last resort: try skillhub index (full skill list)
        let resp = self.client.get(&sh.index).send().await;
        if let Ok(resp) = resp
            && resp.status().is_success()
        {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let query_lower = query.to_lowercase();
                let results: Vec<SearchResult> = body
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter(|item| {
                                let slug = item["slug"]
                                    .as_str()
                                    .or_else(|| item["name"].as_str())
                                    .unwrap_or("");
                                let desc = item["summary"]
                                    .as_str()
                                    .or_else(|| item["description"].as_str())
                                    .unwrap_or("");
                                slug.to_lowercase().contains(&query_lower)
                                    || desc.to_lowercase().contains(&query_lower)
                            })
                            .take(10)
                            .map(|item| SearchResult {
                                slug: item["slug"]
                                    .as_str()
                                    .or_else(|| item["name"].as_str())
                                    .unwrap_or("unknown")
                                    .to_owned(),
                                version: item["version"].as_str().map(|s| s.to_owned()),
                                description: item["summary"]
                                    .as_str()
                                    .or_else(|| item["description"].as_str())
                                    .map(|s| s.to_owned()),
                                downloads: item["downloads"]
                                    .as_u64()
                                    .or_else(|| item["download_count"].as_u64()),
                                installs: item["installs"]
                                    .as_u64()
                                    .or_else(|| item["install_count"].as_u64()),
                                stars: item["stars"]
                                    .as_u64()
                                    .or_else(|| item["favorites"].as_u64()),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(results);
            }
        }

        Ok(vec![])
    }
}

impl Default for ClawhubClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    use std::io::Cursor;

    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("not a valid ZIP archive")?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_owned();

        // Skip directories and __MACOSX metadata.
        if name.ends_with('/') || name.starts_with("__MACOSX") {
            continue;
        }

        // Strip the top-level directory if all entries share one.
        let rel_path = name.split_once('/').map(|(_, rest)| rest).unwrap_or(&name);
        if rel_path.is_empty() {
            continue;
        }

        let out_path = dest.join(rel_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut file, &mut out)?;
    }

    Ok(())
}

fn extract_tarball(bytes: &[u8], dest: &Path) -> Result<()> {
    use std::io::Cursor;

    let cursor = Cursor::new(bytes);
    // Try gzip-compressed tarball first.
    let decoder = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decoder);

    archive
        .unpack(dest)
        .with_context(|| format!("extract tarball to {}", dest.display()))
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let data =
        std::fs::read(path).with_context(|| format!("read for checksum: {}", path.display()))?;
    let digest = Sha256::digest(&data);
    Ok(format!("{digest:x}"))
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[(byte & 0xf) as usize]));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_file_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let mut lock = LockFile {
            version: 1,
            updated: Some(Utc::now()),
            skills: HashMap::new(),
        };
        lock.skills.insert(
            "test-skill".to_owned(),
            LockedSkill {
                slug: "test-skill".to_owned(),
                version: "1.0.0".to_owned(),
                resolved_at: Utc::now(),
                source: SkillSource::Clawhub,
                checksum: "abc123".to_owned(),
                install_dir: tmp.path().join("test-skill"),
            },
        );

        lock.write(tmp.path()).expect("write");
        let read_back = LockFile::read(tmp.path()).expect("read");
        assert_eq!(read_back.skills.len(), 1);
        assert_eq!(read_back.skills["test-skill"].version, "1.0.0");
    }

    #[test]
    fn lock_file_missing_returns_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock = LockFile::read(tmp.path()).expect("read");
        assert!(lock.skills.is_empty());
    }
}
