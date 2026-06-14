//! A2A-Version header negotiation middleware.

use axum::{
    extract::Request,
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};

pub const SUPPORTED_VERSIONS: &[&str] = &["1.0"];

pub async fn a2a_version_layer(req: Request, next: Next) -> Result<Response, StatusCode> {
    if let Some(v) = req.headers().get("a2a-version") {
        let s = v.to_str().unwrap_or("");
        if !SUPPORTED_VERSIONS.contains(&s) {
            return Err(StatusCode::NOT_ACCEPTABLE);
        }
    }
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert("a2a-version", HeaderValue::from_static("1.0"));
    Ok(resp)
}
