//! Integration tests for the SSE stream and health endpoints.
//!
//! The server is started on a free port; tests hit it over real TCP.

mod common;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// GET /api/v1/health must return 200 with JSON body `{"status":"ok"}`.
#[tokio::test]
async fn health_endpoint_returns_200_ok() {
    let addr = common::free_addr();
    common::start_server(addr).await;

    let resp = reqwest::get(format!("http://{addr}/api/v1/health"))
        .await
        .expect("GET /api/v1/health");

    assert_eq!(resp.status(), 200, "health should be 200");

    let body: serde_json::Value = resp.json().await.expect("JSON body");
    assert_eq!(
        body.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "health body should contain {{\"status\":\"ok\"}}, got: {body}"
    );
}

/// GET /api/v1/health must return Content-Type: application/json.
#[tokio::test]
async fn health_endpoint_returns_json_content_type() {
    let addr = common::free_addr();
    common::start_server(addr).await;

    let resp = reqwest::get(format!("http://{addr}/api/v1/health"))
        .await
        .expect("GET /api/v1/health");

    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        ct.contains("application/json"),
        "expected application/json content-type, got: {ct}"
    );
}

/// GET /api/v1/stream must return 200 with Content-Type: text/event-stream.
#[tokio::test]
async fn sse_stream_returns_text_event_stream() {
    let addr = common::free_addr();
    common::start_server(addr).await;

    // Use a client that does NOT follow redirects and does NOT buffer the body
    // (SSE streams are infinite; we only inspect headers).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .expect("client");

    let resp = client
        .get(format!("http://{addr}/api/v1/stream"))
        .send()
        .await
        .expect("GET /api/v1/stream");

    assert_eq!(resp.status(), 200, "SSE stream should return 200");

    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        ct.contains("text/event-stream"),
        "SSE endpoint should return text/event-stream, got: {ct}"
    );
}
