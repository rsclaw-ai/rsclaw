//! `/api/v1/knowledge/*` — desktop-facing knowledge base API.
//!
//! Collections are a tag veneer over the single KB store (see project note
//! `kb-desktop-collections`); handlers delegate to `AppState::knowledge`
//! (`KnowledgeService`). P1: collection metadata CRUD. Docs/search land in
//! P2/P3.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::{Stream, StreamExt as _};
use std::convert::Infallible;
use std::time::Duration;

use crate::kb::model::KbCollection;
use crate::kb::service::DocInfo;
use crate::kb::KnowledgeError;
use crate::server::AppState;

/// Max upload size (JSON body or multipart file). Mirrors the spec's
/// `knowledge.maxDocMb` default; could be made configurable later.
const MAX_DOC_BYTES: usize = 50 * 1024 * 1024;

/// Routes nested under `/api/v1/knowledge`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/collections", get(list_collections).post(create_collection))
        .route(
            "/collections/{id}",
            get(get_collection).patch(patch_collection).delete(delete_collection),
        )
        .route(
            "/collections/{id}/docs",
            get(list_docs).post(upload_doc),
        )
        .route(
            "/collections/{id}/docs/{doc_id}",
            get(get_doc).delete(delete_doc),
        )
        .route("/collections/{id}/docs/{doc_id}/content", get(get_doc_content))
        .route("/collections/{id}/docs/{doc_id}/reindex", post(reindex_doc))
        .route("/search", post(search))
        .route("/stats", get(stats))
        .route("/embedders", get(embedders))
        .route("/events", get(events))
        // Allow large document uploads (default axum limit is 2MB).
        .layer(DefaultBodyLimit::max(MAX_DOC_BYTES))
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

async fn list_collections(State(st): State<AppState>) -> Response {
    match st.knowledge.list_collections() {
        Ok(cols) => {
            let dtos: Vec<CollectionDto> = cols.into_iter().map(Into::into).collect();
            Json(serde_json::json!({ "collections": dtos })).into_response()
        }
        Err(e) => err_response(e),
    }
}

async fn create_collection(
    State(st): State<AppState>,
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
    match st
        .knowledge
        .create_collection(name, req.description, req.embed_model)
    {
        Ok(c) => (StatusCode::CREATED, Json(CollectionDto::from(c))).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_collection(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.knowledge.get_collection(&id) {
        Ok(c) => Json(CollectionDto::from(c)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn patch_collection(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PatchCollectionReq>,
) -> Response {
    // `description` present (incl. null) replaces; absent leaves unchanged.
    // (JSON can't distinguish absent from null here, so an explicit clear is
    // a P2 refinement; for now omitting it keeps the existing value.)
    let desc = req.description.map(Some);
    match st.knowledge.update_collection(&id, req.name, desc) {
        Ok(c) => Json(CollectionDto::from(c)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_collection(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.knowledge.delete_collection(&id) {
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
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": code })),
    )
        .into_response()
}

/// Upload a document. Accepts either `application/json` ({title, text, mime?})
/// for text/markdown, or `multipart/form-data` (title field + file field) for
/// binary files (pdf/docx/xlsx/pptx) — the backend canonicalizes either way.
/// Returns 202; indexing runs in the background.
async fn upload_doc(State(st): State<AppState>, Path(cid): Path<String>, req: Request) -> Response {
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ct.starts_with("multipart/form-data") {
        upload_multipart(st, cid, req).await
    } else {
        // Treat everything else as JSON.
        let body = match to_bytes(req.into_body(), MAX_DOC_BYTES).await {
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
            &st,
            &cid,
            parsed.title.trim(),
            parsed.text.as_bytes(),
            parsed.mime.as_deref(),
        )
    }
}

async fn upload_multipart(st: AppState, cid: String, req: Request) -> Response {
    let mut mp = match Multipart::from_request(req, &st).await {
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
    // Title precedence: explicit `title` field, else the uploaded filename.
    // The filename also drives MIME detection (OOXML magic is just zip), so we
    // pass mime=None and let `ingest` detect from the title/extension.
    let title = title
        .filter(|t| !t.trim().is_empty())
        .or(file_name)
        .unwrap_or_default();
    ingest_and_respond(&st, &cid, title.trim(), &bytes, None)
}

/// Validate + ingest + 202. Shared by the JSON and multipart paths.
fn ingest_and_respond(
    st: &AppState,
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
    match st.knowledge.ingest(cid, title, bytes, mime) {
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

async fn list_docs(State(st): State<AppState>, Path(cid): Path<String>) -> Response {
    match st.knowledge.list_docs(&cid) {
        Ok(docs) => {
            let dtos: Vec<DocDto> = docs.into_iter().map(Into::into).collect();
            Json(serde_json::json!({ "docs": dtos, "nextCursor": serde_json::Value::Null }))
                .into_response()
        }
        Err(e) => err_response(e),
    }
}

async fn get_doc(State(st): State<AppState>, Path((cid, did)): Path<(String, String)>) -> Response {
    match st.knowledge.get_doc(&cid, &did) {
        Ok(d) => Json(DocDto::from(d)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_doc_content(
    State(st): State<AppState>,
    Path((cid, did)): Path<(String, String)>,
) -> Response {
    match st.knowledge.doc_content(&cid, &did) {
        Ok((mime, body)) => (
            [(header::CONTENT_TYPE, format!("{mime}; charset=utf-8"))],
            body,
        )
            .into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_doc(
    State(st): State<AppState>,
    Path((cid, did)): Path<(String, String)>,
) -> Response {
    match st.knowledge.delete_doc(&cid, &did) {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn reindex_doc(
    State(st): State<AppState>,
    Path((cid, did)): Path<(String, String)>,
) -> Response {
    match st.knowledge.reindex_doc(&cid, &did) {
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

async fn search(State(st): State<AppState>, Json(req): Json<SearchReq>) -> Response {
    let query = req.query.trim();
    if query.is_empty() || query.chars().count() > 512 {
        return bad_request("invalid_query");
    }
    let top_k = req.top_k.unwrap_or(10).clamp(1, 50);
    let threshold = req.score_threshold.unwrap_or(0.0);
    let t0 = std::time::Instant::now();
    match st
        .knowledge
        .search(query, &req.collection_ids, top_k, threshold)
    {
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

async fn stats(State(st): State<AppState>) -> Response {
    match st.knowledge.stats() {
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

/// SSE stream of `knowledge.doc.status_changed` events, so the UI can react to
/// async indexing finishing without polling. Each event's data is the JSON
/// `{ type, docId, status }`.
async fn events(State(st): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = st.knowledge.subscribe();
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

async fn embedders(State(st): State<AppState>) -> Response {
    let list = st.knowledge.embedders();
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
