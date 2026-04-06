//! Integration tests for the transcription module.
//!
//! Most transcription functionality requires external services (OpenAI, local
//! whisper, Tencent/Aliyun ASR), so we focus on what is testable in isolation:
//! the download helper, key resolution, and basic function signatures.

use rsclaw::channel::transcription::{download_file, resolve_openai_key, transcribe_audio};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// SILK format with 0x02 prefix: the function should not panic regardless of
/// provider availability. On macOS it may succeed via built-in transcriber;
/// on CI it may error. Either is acceptable.
#[tokio::test]
async fn silk_with_prefix_does_not_panic() {
    init_crypto();
    let client = reqwest::Client::new();
    let silk_bytes: Vec<u8> = [0x02u8]
        .iter()
        .chain(b"#!SILK".iter())
        .chain(b"\x00\x00\x00".iter())
        .copied()
        .collect();

    // Just verify it does not panic.
    let _result = transcribe_audio(&client, &silk_bytes, "voice.silk", "audio/silk").await;
}

/// SILK without the 0x02 prefix should also be handled without panic.
#[tokio::test]
async fn silk_without_prefix_does_not_panic() {
    init_crypto();
    let client = reqwest::Client::new();
    let silk_bytes: Vec<u8> = b"#!SILK\x00\x00\x00".to_vec();

    let _result = transcribe_audio(&client, &silk_bytes, "voice.silk", "audio/silk").await;
}

/// Non-SILK audio should also be handled without panic.
#[tokio::test]
async fn non_silk_audio_does_not_panic() {
    init_crypto();
    let client = reqwest::Client::new();
    let ogg_bytes = b"OggS\x00\x00\x00\x00\x00".to_vec();

    let _result = transcribe_audio(&client, &ogg_bytes, "voice.ogg", "audio/ogg").await;
}

/// `download_file` should download bytes from a URL.
#[tokio::test]
async fn download_file_from_mock() {
    init_crypto();
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/audio/test.wav"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(vec![0x52, 0x49, 0x46, 0x46]), // "RIFF" header
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/audio/test.wav", server.uri());
    let bytes = download_file(&client, &url).await.unwrap();
    assert_eq!(bytes, vec![0x52, 0x49, 0x46, 0x46]);
}

/// `download_file` should return an error for non-2xx responses.
#[tokio::test]
async fn download_file_404_errors() {
    init_crypto();
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/missing", server.uri());
    let result = download_file(&client, &url).await;
    assert!(result.is_err());
}

/// `resolve_openai_key` returns None when unset and Some when set.
/// Combined into a single test to avoid env-var races between parallel tests.
#[test]
fn resolve_openai_key_respects_env() {
    let prev = std::env::var("OPENAI_API_KEY").ok();

    // Unset: should return None.
    unsafe { std::env::remove_var("OPENAI_API_KEY"); }
    assert!(resolve_openai_key().is_none(), "should be None when unset");

    // Set: should return the value.
    unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test-key-12345"); }
    assert_eq!(resolve_openai_key().as_deref(), Some("sk-test-key-12345"));

    // Empty string: should return None.
    unsafe { std::env::set_var("OPENAI_API_KEY", ""); }
    assert!(resolve_openai_key().is_none(), "should be None when empty");

    // Restore.
    match prev {
        Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v); },
        None => unsafe { std::env::remove_var("OPENAI_API_KEY"); },
    }
}
