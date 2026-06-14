//! `/api/v1/knowledge/*` — desktop-facing knowledge base API.
//!
//! Collections are a tag veneer over the single KB store (see project note
//! `kb-desktop-collections`); handlers delegate to `AppState::knowledge`
//! (`KnowledgeService`). P1: collection metadata CRUD. Docs/search land in
//! P2/P3.

use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State},
    http::{StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};

use rsclaw_kb::{KnowledgeError, KnowledgeService, model::KbCollection, service::DocInfo};

/// Routes nested under `/api/v1/knowledge`. State is the `KnowledgeService`
/// alone (not the full `AppState`), so the handlers are testable in isolation.
/// `max_doc_bytes` (from `kb.maxDocMb`) caps the request body size.
pub fn routes(max_doc_bytes: usize) -> Router<Arc<KnowledgeService>> {
    Router::new()
        .route(
            "/collections",
            get(list_collections).post(create_collection),
        )
        .route(
            "/collections/{id}",
            get(get_collection)
                .patch(patch_collection)
                .delete(delete_collection),
        )
        .route("/collections/{id}/docs", get(list_docs).post(upload_doc))
        .route("/collections/{id}/docs/from-url", post(upload_from_url))
        .route("/collections/{id}/docs/from-path", post(upload_from_path))
        .route("/collections/{id}/docs/from-dir", post(upload_from_dir))
        .route(
            "/collections/{id}/docs/{doc_id}",
            get(get_doc).delete(delete_doc),
        )
        .route(
            "/collections/{id}/docs/{doc_id}/content",
            get(get_doc_content),
        )
        .route("/collections/{id}/docs/{doc_id}/reindex", post(reindex_doc))
        .route("/search", post(search))
        .route("/stats", get(stats))
        .route("/compact", post(compact))
        .route("/sync-all", post(sync_all))
        .route("/docs", get(list_all_docs))
        .route(
            "/docs/{doc_id}",
            get(get_doc_by_id).delete(delete_doc_by_id),
        )
        .route("/docs/{doc_id}/content", get(get_doc_content_by_id))
        .route("/docs/{doc_id}/chunks", get(get_doc_chunks))
        .route("/docs/{doc_id}/visibility", axum::routing::patch(patch_doc_visibility))
        .route("/embedders", get(embedders))
        .route("/events", get(events))
        // Allow large document uploads (default axum limit is 2MB).
        .layer(DefaultBodyLimit::max(max_doc_bytes))
}

// --- error mapping --------------------------------------------------------

/// Map a service error to (HTTP status, stable error code) per the API's
/// error envelope `{ "error": <code> }`.
fn err_response(e: KnowledgeError) -> Response {
    let (status, code) = match e {
        KnowledgeError::CollectionNotFound => (StatusCode::NOT_FOUND, "collection_not_found"),
        KnowledgeError::DocNotFound => (StatusCode::NOT_FOUND, "doc_not_found"),
        KnowledgeError::DuplicateName => (StatusCode::CONFLICT, "duplicate_name"),
        KnowledgeError::Internal(ref err) => {
            tracing::warn!("knowledge internal error: {err:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    };
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

// --- DTOs -----------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionDto {
    id: String,
    name: String,
    description: Option<String>,
    embed_model: Option<String>,
    /// P2 will populate from the resolved embedder; 0 until then.
    embed_dim: u32,
    /// P2 will populate by counting docs/chunks tagged to this collection.
    doc_count: u64,
    chunk_count: u64,
    bytes: u64,
    created_at: String,
    updated_at: String,
}

fn ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

impl From<KbCollection> for CollectionDto {
    fn from(c: KbCollection) -> Self {
        CollectionDto {
            id: c.id,
            name: c.name,
            description: c.description,
            embed_model: c.embed_model,
            embed_dim: 0,
            doc_count: 0,
            chunk_count: 0,
            bytes: 0,
            created_at: ms_to_rfc3339(c.created_at),
            updated_at: ms_to_rfc3339(c.updated_at),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCollectionReq {
    name: String,
    description: Option<String>,
    embed_model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchCollectionReq {
    name: Option<String>,
    description: Option<String>,
}

// --- handlers -------------------------------------------------------------

async fn list_collections(State(svc): State<Arc<KnowledgeService>>) -> Response {
    match svc.list_collections() {
        Ok(cols) => {
            let dtos: Vec<CollectionDto> = cols.into_iter().map(Into::into).collect();
            Json(serde_json::json!({ "collections": dtos })).into_response()
        }
        Err(e) => err_response(e),
    }
}

async fn create_collection(
    State(svc): State<Arc<KnowledgeService>>,
    Json(req): Json<CreateCollectionReq>,
) -> Response {
    let name = req.name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name_required" })),
        )
            .into_response();
    }
    if name.chars().count() > 100 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name_too_long" })),
        )
            .into_response();
    }
    match svc.create_collection(name, req.description, req.embed_model) {
        Ok(c) => (StatusCode::CREATED, Json(CollectionDto::from(c))).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_collection(
    State(svc): State<Arc<KnowledgeService>>,
    Path(id): Path<String>,
) -> Response {
    match svc.get_collection(&id) {
        Ok(c) => Json(CollectionDto::from(c)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn patch_collection(
    State(svc): State<Arc<KnowledgeService>>,
    Path(id): Path<String>,
    Json(req): Json<PatchCollectionReq>,
) -> Response {
    // `description` present (incl. null) replaces; absent leaves unchanged.
    // (JSON can't distinguish absent from null here, so an explicit clear is
    // a P2 refinement; for now omitting it keeps the existing value.)
    let desc = req.description.map(Some);
    match svc.update_collection(&id, req.name, desc) {
        Ok(c) => Json(CollectionDto::from(c)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_collection(
    State(svc): State<Arc<KnowledgeService>>,
    Path(id): Path<String>,
) -> Response {
    match svc.delete_collection(&id) {
        Ok(deleted_docs) => {
            Json(serde_json::json!({ "deletedDocs": deleted_docs })).into_response()
        }
        Err(e) => err_response(e),
    }
}

// --- documents ------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DocDto {
    id: String,
    title: String,
    source: &'static str,
    mime: String,
    bytes: u64,
    chunk_count: usize,
    status: String,
    /// RFC3339 when the doc became `ready`; null while still indexing.
    indexed_at: Option<String>,
    created_at: String,
}

impl From<DocInfo> for DocDto {
    fn from(d: DocInfo) -> Self {
        let status = d.status().to_string();
        let indexed_at = (status == "ready").then(|| ms_to_rfc3339(d.updated_at));
        DocDto {
            id: d.id,
            title: d.title,
            source: "uploaded",
            mime: d.mime,
            bytes: d.bytes,
            chunk_count: d.chunk_count,
            status,
            indexed_at,
            created_at: ms_to_rfc3339(d.created_at),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadJsonReq {
    title: String,
    text: String,
    mime: Option<String>,
    #[allow(dead_code)]
    source: Option<String>,
}

fn bad_request(code: &str) -> Response {
    status_err(StatusCode::BAD_REQUEST, code)
}

fn status_err(status: StatusCode, code: &str) -> Response {
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

/// Upload a document. Accepts either `application/json` ({title, text, mime?})
/// for text/markdown, or `multipart/form-data` (title field + file field) for
/// binary files (pdf/docx/xlsx/pptx) — the backend canonicalizes either way.
/// Returns 202; indexing runs in the background.
async fn upload_doc(
    State(svc): State<Arc<KnowledgeService>>,
    Path(cid): Path<String>,
    req: Request,
) -> Response {
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ct.starts_with("multipart/form-data") {
        upload_multipart(svc, cid, req).await
    } else {
        // Treat everything else as JSON.
        let body = match to_bytes(req.into_body(), svc.max_doc_bytes()).await {
            Ok(b) => b,
            Err(_) => return bad_request("body_too_large"),
        };
        let parsed: UploadJsonReq = match serde_json::from_slice(&body) {
            Ok(p) => p,
            Err(_) => return bad_request("invalid_json"),
        };
        if parsed.text.is_empty() {
            return bad_request("empty_content");
        }
        ingest_and_respond(
            &svc,
            &cid,
            parsed.title.trim(),
            parsed.text.as_bytes(),
            parsed.mime.as_deref(),
        )
    }
}

async fn upload_multipart(svc: Arc<KnowledgeService>, cid: String, req: Request) -> Response {
    let mut mp = match Multipart::from_request(req, &svc).await {
        Ok(m) => m,
        Err(_) => return bad_request("invalid_multipart"),
    };
    let mut title: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = mp.next_field().await {
        match field.name().unwrap_or("") {
            "title" => title = field.text().await.ok(),
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                bytes = field.bytes().await.ok().map(|b| b.to_vec());
            }
            _ => {}
        }
    }
    let bytes = match bytes {
        Some(b) if !b.is_empty() => b,
        _ => return bad_request("empty_content"),
    };
    // MIME detection MUST key off the real uploaded filename, not the title:
    // a custom `title` field has no extension, and OOXML / .eml / .mbox are
    // distinguished by extension (their magic is just zip / ASCII). Detecting
    // from the title would mis-route them (docx → octet-stream error, .eml →
    // plain text). Detect from the filename here and pass it explicitly.
    let mime = file_name
        .as_deref()
        .map(|f| rsclaw_kb::canonicalize::detect_mime(&bytes, Some(f)));
    // Title precedence for DISPLAY only: explicit `title` field, else filename.
    let title = title
        .filter(|t| !t.trim().is_empty())
        .or(file_name)
        .unwrap_or_default();
    ingest_and_respond(&svc, &cid, title.trim(), &bytes, mime.as_deref())
}

/// Validate + ingest + 202. Shared by the JSON and multipart paths.
fn ingest_and_respond(
    svc: &KnowledgeService,
    cid: &str,
    title: &str,
    bytes: &[u8],
    mime: Option<&str>,
) -> Response {
    if title.is_empty() {
        return bad_request("title_required");
    }
    if bytes.is_empty() {
        return bad_request("empty_content");
    }
    match svc.ingest(cid, title, bytes, mime) {
        Ok((id, _noop)) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "id": id, "title": title, "status": "pending", "bytes": bytes.len()
            })),
        )
            .into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
struct FromPathReq {
    path: String,
}

/// Ingest a document the gateway reads directly off the local filesystem.
///
/// This exists purely as a same-machine optimization for the desktop app:
/// dragging a 50MB PDF would otherwise read it into JS memory, wrap it in
/// multipart, and POST the bytes to a server running on the same box that
/// immediately writes them back to disk. With a path the gateway reads the
/// file itself — no byte round-trip.
///
/// SECURITY: this lets the caller make the gateway read an arbitrary local
/// file, so it is gated twice — (1) loopback peers only (a LAN/WAN client or a
/// remote UI must use the multipart path), and (2) the resolved path must live
/// under an allowed root (home / temp). Without (2) a malicious page hitting
/// `127.0.0.1` via the permissive CORS could exfiltrate `/etc/passwd`.
async fn upload_from_path(
    State(svc): State<Arc<KnowledgeService>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Path(cid): Path<String>,
    Json(req): Json<FromPathReq>,
) -> Response {
    if !crate::server::is_loopback(peer) {
        return status_err(StatusCode::FORBIDDEN, "forbidden_remote");
    }
    let raw = req.path.trim();
    if raw.is_empty() {
        return bad_request("path_required");
    }
    // 404 early if the collection is gone (before touching the filesystem).
    if let Err(e) = svc.get_collection(&cid) {
        return err_response(e);
    }
    let resolved = match validate_local_path(raw, svc.allowed_upload_roots()) {
        Ok(p) => p,
        Err((status, code)) => return status_err(status, code),
    };
    // Enforce the same size cap the multipart path gets from the body limit —
    // std::fs::read bypasses axum's DefaultBodyLimit, so check up front.
    match std::fs::metadata(&resolved) {
        Ok(m) if m.len() as usize > svc.max_doc_bytes() => {
            return status_err(StatusCode::PAYLOAD_TOO_LARGE, "file_too_large");
        }
        Ok(_) => {}
        Err(_) => return status_err(StatusCode::NOT_FOUND, "file_not_found"),
    }
    let bytes = match std::fs::read(&resolved) {
        Ok(b) => b,
        Err(_) => return status_err(StatusCode::NOT_FOUND, "file_not_found"),
    };
    // Title + MIME key off the real filename (extension), exactly like the
    // multipart path — OOXML / email types are distinguished by extension.
    let file_name = resolved
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let mime = rsclaw_kb::canonicalize::detect_mime(&bytes, Some(&file_name));
    ingest_and_respond(&svc, &cid, file_name.trim(), &bytes, Some(&mime))
}

/// Resolve and authorize a local path for `from-path` ingest against the
/// configured allowed roots. Returns the canonical path or an
/// `(HTTP status, error code)` matching the UI contract. Rules: absolute, must
/// canonicalize (resolves `..` + symlinks → defeats symlink escape and confirms
/// existence), must be a regular file, must live under an allowed root.
fn validate_local_path(
    raw: &str,
    allowed_roots: &[std::path::PathBuf],
) -> Result<std::path::PathBuf, (StatusCode, &'static str)> {
    let p = std::path::Path::new(raw);
    // A relative path can't be checked against the absolute allowed roots; the
    // desktop client always sends absolute paths, so treat it as out-of-bounds.
    if !p.is_absolute() {
        return Err((StatusCode::FORBIDDEN, "path_not_allowed"));
    }
    // Canonicalize collapses `..` and resolves symlinks, so a symlink under an
    // allowed root pointing at /etc/passwd resolves to /etc/passwd and fails the
    // root check below. Also errors if the path doesn't exist.
    let canon = std::fs::canonicalize(p).map_err(|_| (StatusCode::NOT_FOUND, "file_not_found"))?;
    if !canon.is_file() {
        return Err((StatusCode::BAD_REQUEST, "not_a_file"));
    }
    if allowed_roots.iter().any(|r| canon.starts_with(r)) {
        Ok(canon)
    } else {
        Err((StatusCode::FORBIDDEN, "path_not_allowed"))
    }
}

/// Recursive directory walk depth cap (defends against pathological trees).
const MAX_DIR_DEPTH: usize = 16;
/// Max files ingested in a single from-dir call (defends against importing a
/// huge tree in one synchronous sweep). Hitting it sets `truncated: true`.
const MAX_DIR_FILES: usize = 2000;

/// Ingest every supported file under a local directory (recursive). Same
/// loopback + allowlist gate as `from-path`; the directory itself must
/// canonicalize under an allowed root. Returns a from-url-style summary
/// `{ docsAdded, docsSkipped, truncated }`. Unsupported / oversized /
/// unreadable files are skipped (counted), never fatal — one bad file can't
/// abort the import. Hidden entries and symlinks are skipped (the latter avoids
/// loops and allowlist escape).
async fn upload_from_dir(
    State(svc): State<Arc<KnowledgeService>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Path(cid): Path<String>,
    Json(req): Json<FromPathReq>,
) -> Response {
    if !crate::server::is_loopback(peer) {
        return status_err(StatusCode::FORBIDDEN, "forbidden_remote");
    }
    let raw = req.path.trim();
    if raw.is_empty() {
        return bad_request("path_required");
    }
    if let Err(e) = svc.get_collection(&cid) {
        return err_response(e);
    }
    let dir = match validate_local_dir(raw, svc.allowed_upload_roots()) {
        Ok(p) => p,
        Err((status, code)) => return status_err(status, code),
    };

    // The walk reads + ingests many files synchronously (redb writes); keep it
    // off the async runtime. Re-check each file stays under the allowed roots
    // so a symlink-free `..` can't escape (canonicalize already collapses it,
    // but defense in depth).
    let roots: Vec<std::path::PathBuf> = svc.allowed_upload_roots().to_vec();
    let max_bytes = svc.max_doc_bytes();
    let svc2 = Arc::clone(&svc);
    let cid2 = cid.clone();
    let summary =
        tokio::task::spawn_blocking(move || walk_and_ingest(&svc2, &cid2, &dir, &roots, max_bytes))
            .await;

    match summary {
        Ok((added, skipped, truncated)) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": if added > 0 { "pending" } else { "skipped" },
                "docsAdded": added,
                "docsSkipped": skipped,
                "truncated": truncated,
            })),
        )
            .into_response(),
        Err(_) => status_err(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// Like `validate_local_path` but requires a directory.
fn validate_local_dir(
    raw: &str,
    allowed_roots: &[std::path::PathBuf],
) -> Result<std::path::PathBuf, (StatusCode, &'static str)> {
    let p = std::path::Path::new(raw);
    if !p.is_absolute() {
        return Err((StatusCode::FORBIDDEN, "path_not_allowed"));
    }
    let canon = std::fs::canonicalize(p).map_err(|_| (StatusCode::NOT_FOUND, "file_not_found"))?;
    if !canon.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "not_a_dir"));
    }
    if allowed_roots.iter().any(|r| canon.starts_with(r)) {
        Ok(canon)
    } else {
        Err((StatusCode::FORBIDDEN, "path_not_allowed"))
    }
}

/// Bounded recursive walk under `dir`, ingesting each supported regular file.
/// Returns `(added, skipped, truncated)`. Skips hidden entries and symlinks.
fn walk_and_ingest(
    svc: &KnowledgeService,
    cid: &str,
    dir: &std::path::Path,
    roots: &[std::path::PathBuf],
    max_bytes: usize,
) -> (usize, usize, bool) {
    let (mut added, mut skipped, mut count) = (0usize, 0usize, 0usize);
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            // Skip dotfiles / dotdirs (.git, .DS_Store, ...).
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Never follow symlinks: avoids directory loops and allowlist escape.
            if ft.is_symlink() {
                continue;
            }
            let p = entry.path();
            if ft.is_dir() {
                if depth < MAX_DIR_DEPTH {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if count >= MAX_DIR_FILES {
                return (added, skipped, true);
            }
            count += 1;
            // Stay under the allowed roots (defense in depth).
            if !roots.iter().any(|r| p.starts_with(r)) {
                skipped += 1;
                continue;
            }
            match std::fs::metadata(&p) {
                Ok(m) if m.len() as usize > max_bytes => {
                    skipped += 1;
                    continue;
                }
                Ok(_) => {}
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            }
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let fname = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let mime = rsclaw_kb::canonicalize::detect_mime(&bytes, Some(&fname));
            match svc.ingest(cid, fname.trim(), &bytes, Some(&mime)) {
                Ok((_, true)) => skipped += 1, // dedupe: identical content already present
                Ok((_, false)) => added += 1,
                Err(_) => skipped += 1, // unsupported / empty / etc — skip, keep going
            }
        }
    }
    (added, skipped, false)
}

#[derive(Deserialize)]
struct FromUrlReq {
    url: String,
}

/// Ingest a document by fetching a URL server-side (delegates to the KB
/// `UrlSyncer`: GET → canonicalize → ingest → enqueue embed). Server-side
/// fetch avoids browser CORS and records `KbSource::Url` provenance so the
/// doc can be re-synced later. Returns 202; indexing runs in the background.
async fn upload_from_url(
    State(svc): State<Arc<KnowledgeService>>,
    Path(cid): Path<String>,
    Json(req): Json<FromUrlReq>,
) -> Response {
    let url = req.url.trim();
    if url.is_empty() {
        return bad_request("url_required");
    }
    if let Err(code) = validate_public_http_url(url) {
        return bad_request(code);
    }
    // 404 early if the collection is gone (before any network fetch).
    if let Err(e) = svc.get_collection(&cid) {
        return err_response(e);
    }
    match svc.ingest_url(&cid, url).await {
        Ok(outcome) => {
            let status = if outcome.docs_added > 0 {
                "pending"
            } else {
                "skipped"
            };
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "status": status,
                    "docsAdded": outcome.docs_added,
                    "docsSkipped": outcome.docs_skipped,
                })),
            )
                .into_response()
        }
        Err(e) => {
            use rsclaw_kb::sync::SyncError;
            let (status, code) = match e {
                SyncError::RateLimited { .. } => {
                    (StatusCode::TOO_MANY_REQUESTS, "url_rate_limited")
                }
                SyncError::AuthFailed(_) => (StatusCode::BAD_GATEWAY, "url_auth_failed"),
                SyncError::Network(_) | SyncError::Permanent(_) => {
                    (StatusCode::BAD_GATEWAY, "url_fetch_failed")
                }
                SyncError::Parse(_) => (StatusCode::UNPROCESSABLE_ENTITY, "url_unprocessable"),
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
            };
            tracing::warn!("kb url ingest failed: {e}");
            (status, Json(serde_json::json!({ "error": code }))).into_response()
        }
    }
}

/// SSRF guard for user-supplied fetch URLs. Requires http(s) and rejects
/// targets that resolve to loopback / private / link-local / unspecified
/// addresses (and the literal `localhost`). NOTE: this validates at request
/// time; a fully hardened impl would also pin the resolved IP through to the
/// connector to defeat DNS-rebinding (deferred — v1 accepts the TOCTOU
/// window since the fetcher re-resolves immediately after).
fn validate_public_http_url(raw: &str) -> Result<(), &'static str> {
    use std::net::ToSocketAddrs;
    let parsed = url::Url::parse(raw).map_err(|_| "invalid_url")?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("invalid_url"),
    }
    let host = parsed.host_str().ok_or("invalid_url")?;
    let host_l = host.to_ascii_lowercase();
    if host_l == "localhost" || host_l.ends_with(".localhost") {
        return Err("url_not_allowed");
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| "url_unresolved")?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if !is_public_ip(&addr.ip()) {
            return Err("url_not_allowed");
        }
    }
    if !any {
        return Err("url_unresolved");
    }
    Ok(())
}

/// True only for globally-routable addresses (best-effort, std-only).
fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || o[0] == 0
                // CGNAT 100.64.0.0/10 (is_shared is unstable)
                || (o[0] == 100 && (o[1] & 0xc0) == 0x40))
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                // unique-local fc00::/7
                || (s[0] & 0xfe00) == 0xfc00
                // link-local fe80::/10
                || (s[0] & 0xffc0) == 0xfe80)
        }
    }
}

async fn list_docs(State(svc): State<Arc<KnowledgeService>>, Path(cid): Path<String>) -> Response {
    match svc.list_docs(&cid) {
        Ok(docs) => {
            let dtos: Vec<DocDto> = docs.into_iter().map(Into::into).collect();
            Json(serde_json::json!({ "docs": dtos, "nextCursor": serde_json::Value::Null }))
                .into_response()
        }
        Err(e) => err_response(e),
    }
}

async fn get_doc(
    State(svc): State<Arc<KnowledgeService>>,
    Path((cid, did)): Path<(String, String)>,
) -> Response {
    match svc.get_doc(&cid, &did) {
        Ok(d) => Json(DocDto::from(d)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_doc_content(
    State(svc): State<Arc<KnowledgeService>>,
    Path((cid, did)): Path<(String, String)>,
) -> Response {
    match svc.doc_content(&cid, &did) {
        Ok((mime, body)) => (
            [(header::CONTENT_TYPE, format!("{mime}; charset=utf-8"))],
            body,
        )
            .into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_doc(
    State(svc): State<Arc<KnowledgeService>>,
    Path((cid, did)): Path<(String, String)>,
) -> Response {
    match svc.delete_doc(&cid, &did) {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn reindex_doc(
    State(svc): State<Arc<KnowledgeService>>,
    Path((cid, did)): Path<(String, String)>,
) -> Response {
    match svc.reindex_doc(&cid, &did) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "indexing" })),
        )
            .into_response(),
        Err(e) => err_response(e),
    }
}

// --- search / stats / embedders ------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchReq {
    query: String,
    #[serde(default)]
    collection_ids: Vec<String>,
    top_k: Option<usize>,
    score_threshold: Option<f32>,
}

async fn search(State(svc): State<Arc<KnowledgeService>>, Json(req): Json<SearchReq>) -> Response {
    let query = req.query.trim();
    if query.is_empty() || query.chars().count() > 512 {
        return bad_request("invalid_query");
    }
    let top_k = req.top_k.unwrap_or(10).clamp(1, 50);
    let threshold = req.score_threshold.unwrap_or(0.0);
    let t0 = std::time::Instant::now();
    match svc.search(query, &req.collection_ids, top_k, threshold) {
        Ok(hits) => {
            let dtos: Vec<_> = hits
                .into_iter()
                .map(|h| {
                    serde_json::json!({
                        "docId": h.doc_id,
                        "collectionId": h.collection_id,
                        "collectionName": h.collection_name,
                        "sourceTitle": h.source_title,
                        "chunkText": h.chunk_text,
                        "score": h.score,
                    })
                })
                .collect();
            Json(serde_json::json!({ "hits": dtos, "queryMs": t0.elapsed().as_millis() as u64 }))
                .into_response()
        }
        Err(e) => err_response(e),
    }
}

async fn stats(State(svc): State<Arc<KnowledgeService>>) -> Response {
    match svc.stats() {
        Ok(s) => Json(serde_json::json!({
            "collectionCount": s.collection_count,
            "docCount": s.doc_count,
            "chunkCount": s.chunk_count,
            "bytes": s.bytes,
        }))
        .into_response(),
        Err(e) => err_response(e),
    }
}

/// Run one compactor tick (orphan content-file cleanup + ledger advance)
/// plus an HNSW snapshot dump, going through the gateway so the redb
/// write lock isn't fought with. CLI `kb compact` calls this when the
/// gateway is up.
async fn compact(State(svc): State<Arc<KnowledgeService>>) -> Response {
    // Compactor walks the whole content tree and may issue redb writes;
    // keep it off the axum runtime so we don't stall request handling.
    let svc_clone = Arc::clone(&svc);
    let res = tokio::task::spawn_blocking(move || svc_clone.compact()).await;
    match res {
        Ok(Ok((stats, snapshot_ok))) => Json(serde_json::json!({
            "orphansDeleted": stats.orphans_deleted,
            "ledgerAdvancedToCleanup": stats.ledger_advanced_to_cleanup,
            "ledgerAdvancedToDone": stats.ledger_advanced_to_done,
            "hnswSnapshot": if snapshot_ok { "ok" } else { "failed" },
        }))
        .into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => {
            tracing::warn!("compact task join error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

/// SSE stream of `knowledge.doc.status_changed` events, so the UI can react to
/// async indexing finishing without polling. Each event's data is the JSON
/// `{ type, docId, status }`.
async fn events(
    State(svc): State<Arc<KnowledgeService>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = svc.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|msg| async move {
        let data = msg.ok()?;
        Some(Ok(Event::default().data(data)))
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

async fn embedders(State(svc): State<Arc<KnowledgeService>>) -> Response {
    let list = svc.embedders();
    let default = list.iter().find(|e| e.is_default).map(|e| e.id.clone());
    let available: Vec<_> = list
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id, "label": e.label, "dim": e.dim, "downloaded": e.downloaded
            })
        })
        .collect();
    Json(serde_json::json!({ "default": default, "available": available })).into_response()
}

// ---------------------------------------------------------------------------
// Doc-id-only convenience routes.
//
// These mirror the existing `/collections/{cid}/docs/{did}*` routes but
// require only `doc_id`, which is what every CLI workflow (`kb show`,
// `kb rm`, `kb export`, `kb visibility`) actually has. The service-layer
// `find_doc` decodes the doc's `collection:` tag to surface the
// containing collection on read responses.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListAllDocsQuery {
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    source_kind: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

async fn list_all_docs(
    State(svc): State<Arc<KnowledgeService>>,
    axum::extract::Query(q): axum::extract::Query<ListAllDocsQuery>,
) -> Response {
    let tags: Vec<String> = q.tag.into_iter().collect();
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let svc_clone = Arc::clone(&svc);
    let res = tokio::task::spawn_blocking(move || {
        svc_clone.list_all_docs(tags, q.source_kind, limit, q.cursor)
    })
    .await;
    match res {
        Ok(Ok(out)) => Json(serde_json::json!({
            "docs": out.docs,
            "nextCursor": out.next_cursor,
        }))
        .into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => {
            tracing::warn!("list_all_docs join error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

async fn get_doc_by_id(
    State(svc): State<Arc<KnowledgeService>>,
    Path(did): Path<String>,
) -> Response {
    match svc.find_doc(&did) {
        Ok((d, cid)) => Json(serde_json::json!({
            "docId": d.id,
            "title": d.title,
            "version": d.version,
            "sourceKind": d.source_kind.as_str(),
            "mime": d.mime,
            "tags": d.tags,
            "visibility": d.visibility,
            "collectionId": cid,
        }))
        .into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
struct DeleteDocByIdQuery {
    /// Bulk delete: tombstone every Active doc tagged with `tag`.
    #[serde(default)]
    tag: Option<String>,
}

async fn delete_doc_by_id(
    State(svc): State<Arc<KnowledgeService>>,
    Path(did): Path<String>,
    axum::extract::Query(q): axum::extract::Query<DeleteDocByIdQuery>,
) -> Response {
    // Bulk-by-tag mode: the route path's `{doc_id}` is ignored; clients
    // hit `/docs/_bulk?tag=<name>` (any path segment works as the slot
    // filler) since axum needs a path param to match the route. Keeping
    // it on the same route avoids a second handler and the auth layer's
    // path-allowlisting only knows about `/docs/{doc_id}`.
    if let Some(tag) = q.tag.filter(|s| !s.is_empty()) {
        return match svc.tombstone_by_tag(&tag) {
            Ok(n) => Json(serde_json::json!({ "tombstoned": n, "tag": tag })).into_response(),
            Err(e) => err_response(e),
        };
    }
    match svc.delete_doc_by_id(&did) {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_doc_content_by_id(
    State(svc): State<Arc<KnowledgeService>>,
    Path(did): Path<String>,
) -> Response {
    match svc.doc_body(&did) {
        Ok((mime, body)) => (
            [(header::CONTENT_TYPE, format!("{mime}; charset=utf-8"))],
            body,
        )
            .into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_doc_chunks(
    State(svc): State<Arc<KnowledgeService>>,
    Path(did): Path<String>,
) -> Response {
    match svc.doc_chunks(&did) {
        Ok(chunks) => {
            let arr: Vec<_> = chunks
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "chunkId": c.id,
                        "seq": c.seq,
                        "headingPath": c.heading_path,
                        "text": c.indexed_text,
                    })
                })
                .collect();
            Json(serde_json::json!({ "chunks": arr })).into_response()
        }
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
struct VisibilityReq {
    visibility: String,
}

async fn patch_doc_visibility(
    State(svc): State<Arc<KnowledgeService>>,
    Path(did): Path<String>,
    Json(req): Json<VisibilityReq>,
) -> Response {
    let Some(vis) = parse_visibility_string(&req.visibility) else {
        return bad_request("invalid_visibility");
    };
    match svc.set_doc_visibility(&did, vis) {
        Ok(()) => Json(serde_json::json!({ "updated": true, "visibility": req.visibility }))
            .into_response(),
        Err(e) => err_response(e),
    }
}

fn parse_visibility_string(s: &str) -> Option<rsclaw_kb::model::KbVisibility> {
    use rsclaw_kb::model::KbVisibility;
    match s {
        "global" => Some(KbVisibility::Global),
        "private" => Some(KbVisibility::Private),
        other => other.strip_prefix("agent:").map(|id| KbVisibility::Agent {
            agent_id: id.to_owned(),
        }),
    }
}

#[derive(Deserialize)]
struct SyncAllReq {
    #[serde(default = "default_sync_interval_min")]
    interval_min: u64,
    #[serde(default = "default_sync_max")]
    max: usize,
    #[serde(default)]
    dry_run: bool,
}

fn default_sync_interval_min() -> u64 {
    60
}
fn default_sync_max() -> usize {
    100
}

async fn sync_all(
    State(svc): State<Arc<KnowledgeService>>,
    Json(req): Json<SyncAllReq>,
) -> Response {
    let svc_clone = Arc::clone(&svc);
    let res = tokio::task::spawn_blocking(move || {
        svc_clone.sync_all_urls(req.interval_min, req.max, req.dry_run)
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => {
            tracing::warn!("sync_all join error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod http_tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*; // oneshot

    type App = Router;

    fn app() -> (TempDir, Arc<KnowledgeService>, App) {
        let tmp = TempDir::new().unwrap();
        let svc = Arc::new(KnowledgeService::open(tmp.path().join("kb")).unwrap());
        let app = routes(svc.max_doc_bytes()).with_state(svc.clone());
        (tmp, svc, app)
    }

    async fn send(
        app: &App,
        method: &str,
        uri: &str,
        json: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        let body = match json {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let val = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, val)
    }

    #[tokio::test]
    async fn full_collection_doc_search_flow_over_http() {
        let (_t, svc, app) = app();

        // create
        let (st, body) = send(
            &app,
            "POST",
            "/collections",
            Some(serde_json::json!({ "name": "手册" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let cid = body["id"].as_str().unwrap().to_string();

        // duplicate name → 409
        let (st, body) = send(
            &app,
            "POST",
            "/collections",
            Some(serde_json::json!({ "name": "手册" })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(body["error"], "duplicate_name");

        // list
        let (st, body) = send(&app, "GET", "/collections", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["collections"].as_array().unwrap().len(), 1);

        // upload doc (202)
        let (st, body) = send(
            &app,
            "POST",
            &format!("/collections/{cid}/docs"),
            Some(serde_json::json!({
                "title": "a.md",
                "text": "# A\n\nquantum entanglement links two particles.",
                "mime": "text/markdown"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let doc_id = body["id"].as_str().unwrap().to_string();

        // drive the (otherwise background) indexing
        while svc.drain_once().unwrap() {}

        // list docs → ready
        let (st, body) = send(&app, "GET", &format!("/collections/{cid}/docs"), None).await;
        assert_eq!(st, StatusCode::OK);
        let docs = body["docs"].as_array().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["status"], "ready");

        // content
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/collections/{cid}/docs/{doc_id}/content"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("two particles"));

        // search
        let (st, body) = send(
            &app,
            "POST",
            "/search",
            Some(serde_json::json!({ "query": "two particles", "collectionIds": [cid] })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(!body["hits"].as_array().unwrap().is_empty());

        // stats
        let (st, body) = send(&app, "GET", "/stats", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["docCount"], 1);
        assert_eq!(body["collectionCount"], 1);

        // delete collection (cascades)
        let (st, body) = send(&app, "DELETE", &format!("/collections/{cid}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["deletedDocs"], 1);

        // gone → 404
        let (st, _) = send(&app, "GET", &format!("/collections/{cid}"), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_collection_404() {
        let (_t, _svc, app) = app();
        let (st, body) = send(&app, "GET", "/collections/col_nope", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "collection_not_found");
    }

    #[test]
    fn ssrf_guard_blocks_private_and_loopback() {
        // Loopback / private / link-local / localhost must be rejected; these
        // use IP literals or `localhost` so the test needs no external DNS.
        for bad in [
            "http://localhost/x",
            "http://127.0.0.1/x",
            "https://10.0.0.1/x",
            "http://192.168.1.1/x",
            "http://169.254.169.254/latest/meta-data", // cloud metadata SSRF classic
            "http://[::1]/x",
            "http://0.0.0.0/x",
        ] {
            assert!(
                validate_public_http_url(bad).is_err(),
                "should reject {bad}"
            );
        }
        // Bad scheme / not-a-url.
        assert_eq!(
            validate_public_http_url("ftp://example.com").unwrap_err(),
            "invalid_url"
        );
        assert_eq!(
            validate_public_http_url("file:///etc/passwd").unwrap_err(),
            "invalid_url"
        );
        assert_eq!(
            validate_public_http_url("not a url").unwrap_err(),
            "invalid_url"
        );
        // A public IP literal passes (no DNS needed).
        assert!(validate_public_http_url("https://8.8.8.8/").is_ok());
    }

    #[test]
    fn from_path_validation() {
        use std::io::Write;

        // Use the temp dir as the single allowed root for a hermetic test.
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let roots = vec![root.clone()];

        // Relative path → not allowed (can't be checked against absolute roots).
        assert_eq!(
            validate_local_path("relative/x.txt", &roots).unwrap_err(),
            (StatusCode::FORBIDDEN, "path_not_allowed")
        );
        // Nonexistent absolute path → 404.
        assert_eq!(
            validate_local_path("/nonexistent/definitely/not/here.txt", &roots).unwrap_err(),
            (StatusCode::NOT_FOUND, "file_not_found")
        );
        // A real file under an allowed root is accepted.
        let f = root.join(format!("rsclaw_frompath_test_{}.txt", std::process::id()));
        std::fs::File::create(&f).unwrap().write_all(b"hi").unwrap();
        assert!(validate_local_path(f.to_str().unwrap(), &roots).is_ok());
        // A directory is not a file.
        assert_eq!(
            validate_local_path(root.to_str().unwrap(), &roots).unwrap_err(),
            (StatusCode::BAD_REQUEST, "not_a_file")
        );
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn from_dir_validation() {
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let roots = vec![root.clone()];
        // A directory under an allowed root is accepted.
        assert!(validate_local_dir(root.to_str().unwrap(), &roots).is_ok());
        // A file is not a directory.
        use std::io::Write;
        let f = root.join(format!("rsclaw_fromdir_test_{}.txt", std::process::id()));
        std::fs::File::create(&f).unwrap().write_all(b"x").unwrap();
        assert_eq!(
            validate_local_dir(f.to_str().unwrap(), &roots).unwrap_err(),
            (StatusCode::BAD_REQUEST, "not_a_dir")
        );
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn walk_ingests_supported_files_recursively() {
        use std::io::Write;
        let (_tmp, svc, _app) = app();
        let cid = svc.create_collection("c", None, None).unwrap().id;

        // Build a tree under a scratch dir: 2 supported at top, 1 in a subdir,
        // 1 unsupported extension, 1 hidden file. roots = the scratch dir.
        let base = std::env::temp_dir().join(format!("rsclaw_walk_{}", std::process::id()));
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let write =
            |p: &std::path::Path, b: &[u8]| std::fs::File::create(p).unwrap().write_all(b).unwrap();
        write(&base.join("a.md"), b"# hello");
        write(&base.join("b.txt"), b"plain text body");
        write(&sub.join("c.md"), b"# nested");
        write(&base.join("data.unknownext"), &[0x00, 0x01, 0x02, 0xff]); // unsupported
        write(&base.join(".hidden.md"), b"# secret"); // hidden → skipped entirely

        let roots = vec![std::fs::canonicalize(&base).unwrap()];
        let (added, skipped, truncated) = walk_and_ingest(
            &svc,
            &cid,
            &std::fs::canonicalize(&base).unwrap(),
            &roots,
            svc.max_doc_bytes(),
        );

        assert_eq!(added, 3, "a.md + b.txt + sub/c.md");
        assert!(skipped >= 1, "the unsupported file is counted as skipped");
        assert!(!truncated);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    #[cfg(unix)]
    fn from_path_rejects_outside_allowed_roots() {
        // /etc/hosts exists and is a regular file, but lives outside the
        // allowed root — the allowlist must reject it (arbitrary-read defense).
        let roots = vec![std::fs::canonicalize(std::env::temp_dir()).unwrap()];
        if std::path::Path::new("/etc/hosts").is_file() {
            assert_eq!(
                validate_local_path("/etc/hosts", &roots).unwrap_err(),
                (StatusCode::FORBIDDEN, "path_not_allowed")
            );
        }
    }

    #[test]
    fn is_public_ip_classification() {
        use std::net::IpAddr;
        assert!(is_public_ip(&"8.8.8.8".parse::<IpAddr>().unwrap()));
        assert!(is_public_ip(&"1.1.1.1".parse::<IpAddr>().unwrap()));
        assert!(!is_public_ip(&"10.1.2.3".parse::<IpAddr>().unwrap()));
        assert!(!is_public_ip(&"172.16.0.1".parse::<IpAddr>().unwrap()));
        assert!(!is_public_ip(&"100.64.0.1".parse::<IpAddr>().unwrap())); // CGNAT
        assert!(!is_public_ip(&"::1".parse::<IpAddr>().unwrap()));
        assert!(!is_public_ip(&"fc00::1".parse::<IpAddr>().unwrap())); // ULA
        assert!(!is_public_ip(&"fe80::1".parse::<IpAddr>().unwrap())); // link-local
    }

    #[tokio::test]
    async fn from_url_rejects_bad_input() {
        let (_t, _svc, app) = app();
        // empty url → 400 url_required
        let (st, body) = send(
            &app,
            "POST",
            "/collections/col_x/docs/from-url",
            Some(serde_json::json!({ "url": "" })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "url_required");
        // SSRF target → 400 url_not_allowed (rejected before any collection/network)
        let (st, body) = send(
            &app,
            "POST",
            "/collections/col_x/docs/from-url",
            Some(serde_json::json!({ "url": "http://127.0.0.1:1/x" })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "url_not_allowed");
    }
}
