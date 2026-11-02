//! UrlSyncer: HTTP GET → canonicalize → ingest. Uses ETag/Last-Modified
//! conditional-get when SyncState has a prior cursor; falls back to
//! content-hash dedupe via the seen_items table.

use std::time::Duration;

use reqwest::header::{ETAG, LAST_MODIFIED};
use rsclaw_util::net::{SafeRequest, read_body_limited, safe_send};

use super::{KbSourceSyncer, SyncContext, SyncError, SyncOutcome, SyncReason};
use crate::{
    canonicalize::{CanonicalizeInput, canonicalize_by_mime, canonicalize_url, detect_mime},
    content_store::atomic::sha256_hex,
    model::{KbSource, KbSourceKind, LogicalSourceId},
    pipeline::{IngestInput, ingest_canonicalized},
    store::seen::{SyncState, is_seen},
    sync::SyncRegistry,
};

const DEFAULT_TIMEOUT_S: u64 = 30;
/// Upper bound on a fetched page / document body.
const MAX_BODY_BYTES: usize = 20 * 1024 * 1024;

pub struct UrlSyncer {
    pub url: String,
    pub tags: Vec<String>,
}

#[async_trait::async_trait]
impl KbSourceSyncer for UrlSyncer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Url
    }
    fn source_id(&self) -> &str {
        &self.url
    }

    async fn sync(&self, ctx: &SyncContext, _reason: SyncReason) -> Result<SyncOutcome, SyncError> {
        let canonical_url = canonicalize_url(&self.url)
            .map_err(|e| SyncError::Parse(format!("url canonicalize: {e}")))?;

        let prior = SyncRegistry::load(&ctx.store, &canonical_url)
            .map_err(|e| SyncError::Permanent(format!("load state: {e}")))?;

        // SSRF-safe fetch: the URL is user/agent supplied, so every hop is
        // resolved, checked against private/loopback/link-local ranges and
        // DNS-pinned, and redirects are re-validated per hop. `safe_send`
        // returns 304 as-is, so conditional requests still work.
        let mut req = SafeRequest::get(canonical_url.clone());
        req.timeout = Duration::from_secs(DEFAULT_TIMEOUT_S);
        if let Some(state) = &prior {
            let cond = if let Some(cur) = state.cursor.strip_prefix("etag:") {
                Some((reqwest::header::IF_NONE_MATCH, cur))
            } else {
                state
                    .cursor
                    .strip_prefix("lastmod:")
                    .map(|cur| (reqwest::header::IF_MODIFIED_SINCE, cur))
            };
            if let Some((name, cur)) = cond
                && let Ok(v) = reqwest::header::HeaderValue::from_str(cur)
            {
                req.headers.insert(name, v);
            }
        }
        let resp = safe_send(
            || reqwest::Client::builder().user_agent("rsclaw-kb-syncer/1.0"),
            req,
        )
        .await
        .map_err(|e| SyncError::Network(format!("get {canonical_url}: {e:#}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(SyncOutcome {
                docs_skipped: 1,
                ..Default::default()
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SyncError::AuthFailed(format!(
                "status {status} for {canonical_url}"
            )));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            return Err(SyncError::RateLimited { retry_after_secs });
        }
        if status.is_client_error() {
            // 4xx other than 401/403/429 — permanent (bad request,
            // not found, gone). Periodic retry won't help.
            return Err(SyncError::Permanent(format!(
                "client error {status} for {canonical_url}"
            )));
        }
        if !status.is_success() {
            // 5xx — transient; cron-style retry should make progress.
            return Err(SyncError::Network(format!(
                "server error {status} for {canonical_url}"
            )));
        }
        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let last_mod = resp
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
        let bytes = read_body_limited(resp, MAX_BODY_BYTES)
            .await
            .map_err(|e| SyncError::Network(format!("body: {e:#}")))?;

        let raw_sha = sha256_hex(&bytes);
        {
            let rtx = ctx
                .store
                .begin_read()
                .map_err(|e| SyncError::Permanent(e.to_string()))?;
            if let Some(prev) = is_seen(&rtx, "url", &canonical_url)
                .map_err(|e| SyncError::Permanent(e.to_string()))?
            {
                if prev.raw_sha256 == raw_sha {
                    return Ok(SyncOutcome {
                        docs_skipped: 1,
                        ..Default::default()
                    });
                }
            }
        }

        let mime = content_type.unwrap_or_else(|| detect_mime(&bytes, Some(&canonical_url)));
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: &bytes,
            mime: &mime,
            hint_title: Some(&canonical_url),
            // Key the logical source on the URL, not the content hash, so a
            // changed page becomes a new VERSION of the same source (the old
            // one is superseded) instead of a second unrelated doc.
            logical_source_id_seed: Some(LogicalSourceId::for_url(&canonical_url)),
        })
        .map_err(|e| SyncError::Parse(format!("canonicalize: {e}")))?
        .ok_or_else(|| SyncError::Parse(format!("no canonical output for mime={mime}")))?;
        let mut canon = canon;
        canon.metadata.tags.extend(self.tags.iter().cloned());
        let raw_ext = mime_to_ext(&mime);
        let source = Some(KbSource::Url {
            url: canonical_url.clone(),
            fetched_at: chrono::Utc::now().timestamp_millis(),
        });
        let out = ingest_canonicalized(
            &ctx.store,
            IngestInput {
                canon: &canon,
                raw_bytes: &bytes,
                raw_ext,
                visibility: None,
                owner_user_id: None,
                seen_key: Some(("url", &canonical_url)),
                source,
                paths: &ctx.paths,
            },
        )
        .map_err(|e| SyncError::Permanent(format!("ingest: {e}")))?;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let cursor = if let Some(e) = etag {
            format!("etag:{e}")
        } else if let Some(lm) = last_mod {
            format!("lastmod:{lm}")
        } else {
            format!("contenthash:{raw_sha}")
        };
        let state = SyncState {
            cursor,
            last_sync_at: now_ms,
        };
        SyncRegistry::save(&ctx.store, &canonical_url, &state)
            .map_err(|e| SyncError::Permanent(format!("save state: {e}")))?;

        Ok(SyncOutcome {
            docs_added: if out.noop { 0 } else { 1 },
            docs_skipped: if out.noop { 1 } else { 0 },
            ..Default::default()
        })
    }
}

fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "text/html" => "html",
        "text/markdown" => "md",
        "text/plain" => "txt",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}
