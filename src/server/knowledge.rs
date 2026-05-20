//! `/api/v1/knowledge/*` — desktop-facing knowledge base API.
//!
//! Collections are a tag veneer over the single KB store (see project note
//! `kb-desktop-collections`); handlers delegate to `AppState::knowledge`
//! (`KnowledgeService`). P1: collection metadata CRUD. Docs/search land in
//! P2/P3.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::kb::model::KbCollection;
use crate::kb::service::DocInfo;
use crate::kb::KnowledgeError;
use crate::server::AppState;

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
        // Doc/chunk cascade counts land in P2; report 0 for now.
        Ok(()) => Json(serde_json::json!({ "deletedDocs": 0, "deletedChunks": 0 })).into_response(),
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

/// Upload a text/markdown document as JSON. Binary files (pdf/docx/...) use
/// the multipart path (added next); the backend canonicalizes either way.
/// Returns 202 — indexing runs in the background.
async fn upload_doc(
    State(st): State<AppState>,
    Path(cid): Path<String>,
    Json(req): Json<UploadJsonReq>,
) -> Response {
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return bad_request("title_required");
    }
    if req.text.is_empty() {
        return bad_request("empty_content");
    }
    let bytes = req.text.len();
    match st
        .knowledge
        .ingest(&cid, &title, req.text.as_bytes(), req.mime.as_deref())
    {
        Ok((id, _noop)) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "id": id, "title": title, "status": "pending", "bytes": bytes
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
