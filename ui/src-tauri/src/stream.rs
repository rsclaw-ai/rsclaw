//
//

use std::{
    collections::HashMap,
    error::Error,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName},
};
use tauri::Emitter;

static REQUEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn validate_stream_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = raw
        .parse::<reqwest::Url>()
        .map_err(|error| format!("failed to parse URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("stream URL must use HTTP or HTTPS".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("stream URL must not contain credentials or a fragment".to_owned());
    }
    let is_loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !is_loopback {
        return Err("stream URL must target the local RsClaw gateway".to_owned());
    }
    Ok(url)
}

fn header_is_forbidden(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "content-length"
            | "host"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamResponse {
    request_id: u32,
    status: u16,
    status_text: String,
    headers: HashMap<String, String>,
}

#[derive(Clone, serde::Serialize)]
pub struct EndPayload {
    request_id: u32,
    status: u16,
}

#[derive(Clone, serde::Serialize)]
pub struct ChunkPayload {
    request_id: u32,
    chunk: Vec<u8>,
}

#[tauri::command]
pub async fn stream_fetch(
    window: tauri::WebviewWindow,
    method: String,
    url: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
) -> Result<StreamResponse, String> {
    let event_name = "stream-response";
    let request_id = REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst);

    let mut _headers = HeaderMap::new();
    for (key, value) in &headers {
        let name = key
            .parse::<HeaderName>()
            .map_err(|error| format!("invalid request header name `{key}`: {error}"))?;
        if header_is_forbidden(&name) {
            return Err(format!("request header is not allowed: {name}"));
        }
        let value = value
            .parse::<reqwest::header::HeaderValue>()
            .map_err(|error| format!("invalid value for request header `{key}`: {error}"))?;
        _headers.insert(name, value);
    }

    // println!("method: {:?}", method);
    // println!("url: {:?}", url);
    // println!("headers: {:?}", headers);
    // println!("headers: {:?}", _headers);

    let method = method
        .parse::<reqwest::Method>()
        .map_err(|err| format!("failed to parse method: {err}"))?;
    if !matches!(
        method,
        reqwest::Method::GET
            | reqwest::Method::HEAD
            | reqwest::Method::POST
            | reqwest::Method::PUT
            | reqwest::Method::PATCH
            | reqwest::Method::DELETE
    ) {
        return Err(format!("request method is not allowed: {method}"));
    }
    let url = validate_stream_url(&url)?;
    let client = Client::builder()
        .default_headers(_headers)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::new(3, 0))
        .build()
        .map_err(|err| format!("failed to generate client: {}", err))?;

    let mut request = client.request(method.clone(), url);

    if method == reqwest::Method::POST
        || method == reqwest::Method::PUT
        || method == reqwest::Method::PATCH
    {
        let body = bytes::Bytes::from(body);
        // println!("body: {:?}", body);
        request = request.body(body);
    }

    // println!("client: {:?}", client);
    // println!("request: {:?}", request);

    let response_future = request.send();

    let res = response_future.await;
    let response = match res {
        Ok(res) => {
            // get response and emit to client
            let mut headers = HashMap::new();
            for (name, value) in res.headers() {
                let value = value.to_str().map_err(|error| {
                    format!("response header `{name}` is not valid text: {error}")
                })?;
                headers.insert(name.as_str().to_string(), value.to_owned());
            }
            let status = res.status().as_u16();

            tauri::async_runtime::spawn(async move {
                let mut stream = res.bytes_stream();

                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            // println!("chunk: {:?}", bytes);
                            if let Err(e) = window.emit(
                                event_name,
                                ChunkPayload {
                                    request_id,
                                    chunk: bytes.to_vec(),
                                },
                            ) {
                                println!("Failed to emit chunk payload: {:?}", e);
                            }
                        }
                        Err(err) => {
                            println!("Error chunk: {:?}", err);
                        }
                    }
                }
                if let Err(e) = window.emit(
                    event_name,
                    EndPayload {
                        request_id,
                        status: 0,
                    },
                ) {
                    println!("Failed to emit end payload: {:?}", e);
                }
            });

            StreamResponse {
                request_id,
                status,
                status_text: "OK".to_string(),
                headers,
            }
        }
        Err(err) => {
            let error: String = err
                .source()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Unknown error occurred".to_string());
            println!("Error response: {:?}", error);
            tauri::async_runtime::spawn(async move {
                if let Err(e) = window.emit(
                    event_name,
                    ChunkPayload {
                        request_id,
                        chunk: error.into_bytes(),
                    },
                ) {
                    println!("Failed to emit chunk payload: {:?}", e);
                }
                if let Err(e) = window.emit(
                    event_name,
                    EndPayload {
                        request_id,
                        status: 0,
                    },
                ) {
                    println!("Failed to emit end payload: {:?}", e);
                }
            });
            StreamResponse {
                request_id,
                status: 599,
                status_text: "Error".to_string(),
                headers: HashMap::new(),
            }
        }
    };
    // println!("Response: {:?}", response);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_urls_are_limited_to_loopback_http() {
        for allowed in [
            "http://localhost:18888/v1/chat/completions",
            "https://127.0.0.1:18888/api/v1/a2a?stream=true",
            "http://[::1]:18888/health",
        ] {
            assert!(
                validate_stream_url(allowed).is_ok(),
                "expected allowed: {allowed}"
            );
        }
        for denied in [
            "https://api.openai.com/v1/chat/completions",
            "http://169.254.169.254/latest/meta-data",
            "http://192.168.1.2:18888/",
            "file:///etc/passwd",
            "http://user:secret@localhost:18888/",
            "http://localhost:18888/#fragment",
        ] {
            assert!(
                validate_stream_url(denied).is_err(),
                "expected denied: {denied}"
            );
        }
    }

    #[test]
    fn hop_by_hop_and_proxy_headers_are_forbidden() {
        for denied in [
            "host",
            "connection",
            "content-length",
            "transfer-encoding",
            "proxy-authorization",
        ] {
            let name = denied.parse::<HeaderName>().expect("valid test header");
            assert!(header_is_forbidden(&name));
        }
        let authorization = "authorization"
            .parse::<HeaderName>()
            .expect("valid authorization header");
        assert!(!header_is_forbidden(&authorization));
    }
}
