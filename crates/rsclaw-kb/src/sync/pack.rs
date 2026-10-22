//! Knowledge-pack subscription: keeps the packs this machine's account may
//! read on its server as local read-only collections (`pack_<pack id>`).
//!
//! Every interval the syncer lists the account's packs, fetches each pack's
//! manifest (conditional on the last one applied), downloads the docs whose
//! sha256 changed, re-ingests them under their path in the pack, and drops
//! the docs and packs the server no longer lists. Only raw documents travel;
//! embedding and search stay local, so a server outage leaves the last synced
//! version searchable.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::{content_store::atomic::sha256_hex, service::KnowledgeService};

/// Seconds between checks when `kb.packs.intervalSecs` is unset.
const DEFAULT_INTERVAL_SECS: u64 = 600;
/// Floor on the check interval.
const MIN_INTERVAL_SECS: u64 = 30;
/// Per-request deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The server's per-doc cap; anything larger is refused rather than stored.
const MAX_DOC_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackList {
    packs: Vec<PackSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackSummary {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackManifest {
    id: String,
    name: String,
    version: u64,
    docs: Vec<PackDoc>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackDoc {
    path: String,
    #[serde(default)]
    title: String,
    sha256: String,
    #[serde(default)]
    mime: Option<String>,
}

/// What one sync pass changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackSyncReport {
    pub packs_unchanged: usize,
    pub packs_updated: usize,
    pub packs_removed: usize,
    pub docs_added: usize,
    pub docs_removed: usize,
    pub errors: Vec<String>,
}

/// Where the packs come from and who asks for them.
#[derive(Debug, Clone)]
pub struct PackSource {
    /// API root, e.g. `https://api.rsclaw.ai/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub interval: Duration,
}

impl PackSource {
    /// The `kb.packs` block of the loaded config, with the URL and key
    /// falling back to the rsclaw provider's. None when the block is absent
    /// or disabled.
    pub fn from_config() -> Option<Self> {
        let cfg = rsclaw_config::load_cached().ok()?;
        let packs = cfg.raw.kb.as_ref()?.packs.clone()?;
        if !packs.enabled.unwrap_or(true) {
            return None;
        }
        let base_url = packs
            .url
            .as_deref()
            .map(|u| u.trim().trim_end_matches('/').to_owned())
            .filter(|u| !u.is_empty())
            .or_else(|| crate::ocr::rsclaw_provider_base_url(&cfg))
            .unwrap_or_else(|| rsclaw_embed::RSCLAW_API_BASE_URL.to_owned());
        let api_key = packs
            .api_key
            .as_ref()
            .and_then(|k| k.resolve_early())
            .filter(|k| !k.is_empty())
            .or_else(|| crate::ocr::rsclaw_provider_key(&cfg));
        let interval = packs
            .interval_secs
            .unwrap_or(DEFAULT_INTERVAL_SECS)
            .max(MIN_INTERVAL_SECS);
        Some(Self {
            base_url,
            api_key,
            interval: Duration::from_secs(interval),
        })
    }
}

/// Keeps the subscribed packs in step with the server.
pub struct PackSyncer {
    source: PackSource,
    http: rsclaw_embed::FleetHttp,
    /// ETag of the manifest last applied in full, per pack.
    applied: HashMap<String, String>,
}

impl PackSyncer {
    pub fn new(source: PackSource) -> Self {
        Self {
            source,
            http: rsclaw_embed::FleetHttp::new(None),
            applied: HashMap::new(),
        }
    }

    /// One pass over every pack the account may read.
    pub async fn sync_once(&mut self, kb: &Arc<KnowledgeService>) -> Result<PackSyncReport> {
        let list: PackList = self
            .get_json(&format!("{}/kb/packs", self.source.base_url), None)
            .await?
            .context("pack list answered 304")?
            .1;
        let mut report = PackSyncReport::default();
        for pack in &list.packs {
            if let Err(e) = valid_pack_id(&pack.id) {
                report.errors.push(format!("{}: {e:#}", pack.id));
                continue;
            }
            match self.sync_pack(kb, &pack.id, &mut report).await {
                Ok(true) => report.packs_updated += 1,
                Ok(false) => report.packs_unchanged += 1,
                Err(e) => report.errors.push(format!("{}: {e:#}", pack.id)),
            }
        }
        let listed: Vec<&str> = list.packs.iter().map(|p| p.id.as_str()).collect();
        let local = blocking(kb, |kb| Ok(kb.local_packs()?)).await?;
        for gone in local
            .into_iter()
            .filter(|id| !listed.contains(&id.as_str()))
        {
            let id = gone.clone();
            match blocking(kb, move |kb| Ok(kb.remove_pack(&id)?)).await {
                Ok(docs) => {
                    tracing::info!(pack = %gone, docs, "kb pack removed: no longer listed for this account");
                    self.applied.remove(&gone);
                    report.packs_removed += 1;
                }
                Err(e) => report.errors.push(format!("{gone}: remove: {e:#}")),
            }
        }
        Ok(report)
    }

    /// Bring one pack up to its latest published version. `Ok(false)` when
    /// the manifest has not changed since it was last applied.
    async fn sync_pack(
        &mut self,
        kb: &Arc<KnowledgeService>,
        pack_id: &str,
        report: &mut PackSyncReport,
    ) -> Result<bool> {
        let url = format!("{}/kb/packs/{pack_id}/manifest", self.source.base_url);
        let Some((etag, manifest)) = self
            .get_json::<PackManifest>(&url, self.applied.get(pack_id).map(String::as_str))
            .await?
        else {
            return Ok(false);
        };
        if manifest.id != pack_id {
            bail!("manifest is for pack {:?}", manifest.id);
        }
        for doc in &manifest.docs {
            valid_doc_path(&doc.path)?;
            valid_sha256(&doc.sha256)?;
        }
        let (id, name) = (pack_id.to_owned(), manifest.name.clone());
        blocking(kb, move |kb| Ok(kb.ensure_pack_collection(&id, &name)?)).await?;
        let id = pack_id.to_owned();
        let local = blocking(kb, move |kb| Ok(kb.pack_docs(&id)?)).await?;

        let mut complete = true;
        for doc in &manifest.docs {
            if local.get(&doc.path).is_some_and(|l| l.sha256 == doc.sha256) {
                continue;
            }
            match self.fetch_doc(kb, pack_id, doc).await {
                Ok(()) => report.docs_added += 1,
                Err(e) => {
                    complete = false;
                    report.errors.push(format!("{pack_id}/{}: {e:#}", doc.path));
                }
            }
        }
        for (path, gone) in &local {
            if manifest.docs.iter().any(|d| &d.path == path) {
                continue;
            }
            let (id, doc_id) = (pack_id.to_owned(), gone.doc_id.clone());
            match blocking(kb, move |kb| Ok(kb.remove_pack_doc(&id, &doc_id)?)).await {
                Ok(()) => report.docs_removed += 1,
                Err(e) => {
                    complete = false;
                    report
                        .errors
                        .push(format!("{pack_id}/{path}: remove: {e:#}"));
                }
            }
        }
        // Only a fully applied manifest is remembered, so a doc that failed
        // is retried on the next pass instead of waiting for a new version.
        match (complete, etag) {
            (true, Some(etag)) => {
                self.applied.insert(pack_id.to_owned(), etag);
            }
            _ => {
                self.applied.remove(pack_id);
            }
        }
        tracing::info!(
            pack = pack_id,
            version = manifest.version,
            docs = manifest.docs.len(),
            complete,
            "kb pack synced"
        );
        Ok(true)
    }

    /// Download one doc, check it is the bytes the manifest names, and store it.
    async fn fetch_doc(
        &self,
        kb: &Arc<KnowledgeService>,
        pack_id: &str,
        doc: &PackDoc,
    ) -> Result<()> {
        let url = format!(
            "{}/kb/packs/{pack_id}/docs/{}",
            self.source.base_url, doc.sha256
        );
        let resp = self
            .http
            .get_following_redirects(
                &url,
                self.source.api_key.as_deref(),
                &[],
                Some(REQUEST_TIMEOUT),
            )
            .await?;
        if !resp.status().is_success() {
            bail!("GET doc: HTTP {}", resp.status());
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned());
        let bytes = resp.bytes().await.context("read doc body")?;
        if bytes.len() > MAX_DOC_BYTES {
            bail!("doc is {} bytes, over the {MAX_DOC_BYTES} cap", bytes.len());
        }
        let got = sha256_hex(&bytes);
        if got != doc.sha256 {
            bail!("doc bytes hash to {got}, the manifest says {}", doc.sha256);
        }
        let (id, path) = (pack_id.to_owned(), doc.path.clone());
        let title = doc_title(doc);
        let mime = doc
            .mime
            .clone()
            .filter(|m| !m.trim().is_empty())
            .or(content_type)
            .filter(|m| m != "application/octet-stream");
        blocking(kb, move |kb| {
            kb.ingest_pack_doc(&id, &path, &title, &bytes, mime.as_deref())?;
            Ok(())
        })
        .await
    }

    /// GET a JSON document, conditional on `etag`. `Ok(None)` on 304;
    /// otherwise the response's ETag and body.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        etag: Option<&str>,
    ) -> Result<Option<(Option<String>, T)>> {
        let headers: Vec<(&str, &str)> = etag.map(|e| ("if-none-match", e)).into_iter().collect();
        let resp = self
            .http
            .get_following_redirects(
                url,
                self.source.api_key.as_deref(),
                &headers,
                Some(REQUEST_TIMEOUT),
            )
            .await?;
        match resp.status() {
            StatusCode::NOT_MODIFIED => return Ok(None),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                bail!(
                    "GET {url}: HTTP {} (check the account's API key)",
                    resp.status()
                )
            }
            s if !s.is_success() => bail!("GET {url}: HTTP {s}"),
            _ => {}
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp
            .json::<T>()
            .await
            .with_context(|| format!("GET {url}: bad JSON"))?;
        Ok(Some((etag, body)))
    }
}

/// Start the background subscription when `kb.packs` is configured. The
/// first pass runs shortly after startup, then one every interval.
pub fn spawn_from_config(kb: Arc<KnowledgeService>) {
    let Some(source) = PackSource::from_config() else {
        return;
    };
    if source.api_key.is_none() {
        tracing::warn!(
            "kb packs: no API key (kb.packs.apiKey or the rsclaw provider's); not syncing"
        );
        return;
    }
    tracing::info!(
        url = %source.base_url,
        interval_secs = source.interval.as_secs(),
        "kb packs: subscription started"
    );
    tokio::spawn(async move {
        let interval = source.interval;
        let mut syncer = PackSyncer::new(source);
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            match syncer.sync_once(&kb).await {
                Ok(report) if report.errors.is_empty() => {
                    tracing::debug!(?report, "kb packs: pass done")
                }
                Ok(report) => tracing::warn!(?report, "kb packs: pass done with errors"),
                Err(e) => tracing::warn!("kb packs: pass failed: {e:#}"),
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Run a KB call off the async executor (redb and indexing are blocking).
async fn blocking<T, F>(kb: &Arc<KnowledgeService>, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&KnowledgeService) -> Result<T> + Send + 'static,
{
    let kb = Arc::clone(kb);
    tokio::task::spawn_blocking(move || f(&kb))
        .await
        .context("kb task panicked")?
}

/// The title a doc is stored under: the manifest's, else its file name
/// without the extension.
fn doc_title(doc: &PackDoc) -> String {
    let title = doc.title.trim();
    if !title.is_empty() {
        return title.to_owned();
    }
    let file = doc.path.rsplit('/').next().unwrap_or(&doc.path);
    file.rsplit_once('.')
        .map(|(stem, _)| stem)
        .filter(|stem| !stem.is_empty())
        .unwrap_or(file)
        .to_owned()
}

fn valid_pack_id(id: &str) -> Result<()> {
    let mut chars = id.chars();
    let ok = id.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        bail!("invalid pack id {id:?}")
    }
}

fn valid_doc_path(path: &str) -> Result<()> {
    let ok = !path.is_empty()
        && path.chars().count() <= 256
        && !path.starts_with('/')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..");
    if ok {
        Ok(())
    } else {
        bail!("invalid doc path {path:?}")
    }
}

fn valid_sha256(sha: &str) -> Result<()> {
    if sha.len() == 64
        && sha
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        Ok(())
    } else {
        bail!("invalid sha256 {sha:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(path: &str, title: &str) -> PackDoc {
        PackDoc {
            path: path.into(),
            title: title.into(),
            sha256: "0".repeat(64),
            mime: None,
        }
    }

    #[test]
    fn pack_ids_follow_the_server_slug_rule() {
        assert!(valid_pack_id("parenting").is_ok());
        assert!(valid_pack_id("acme-products2").is_ok());
        assert!(valid_pack_id("-lead").is_err());
        assert!(valid_pack_id("Upper").is_err());
        assert!(valid_pack_id("a_b").is_err());
        assert!(valid_pack_id("").is_err());
    }

    #[test]
    fn doc_paths_stay_inside_the_pack() {
        assert!(valid_doc_path("喂养/吐奶.md").is_ok());
        assert!(valid_doc_path("faq.md").is_ok());
        assert!(valid_doc_path("/etc/passwd").is_err());
        assert!(valid_doc_path("a/../b.md").is_err());
        assert!(valid_doc_path("a//b.md").is_err());
        assert!(valid_doc_path("a\nb.md").is_err());
        assert!(valid_doc_path("").is_err());
    }

    #[test]
    fn a_doc_without_a_title_is_named_after_its_file() {
        assert_eq!(doc_title(&doc("喂养/吐奶.md", "")), "吐奶");
        assert_eq!(
            doc_title(&doc("喂养/吐奶.md", " 吐奶怎么办 ")),
            "吐奶怎么办"
        );
        assert_eq!(doc_title(&doc(".env", "")), ".env");
    }

    #[test]
    fn sha256_must_be_lowercase_hex() {
        assert!(valid_sha256(&"a".repeat(64)).is_ok());
        assert!(valid_sha256(&"A".repeat(64)).is_err());
        assert!(valid_sha256("abc").is_err());
    }
}
