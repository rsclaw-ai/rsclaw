//! DingTalk OAuth2 QR code login.
//!
//! Flow:
//!   1. Generate auth URL → display QR code
//!   2. User scans with DingTalk app → authorizes
//!   3. Poll for auth result with auth code
//!   4. Exchange code for access token
//!   5. Store token persistently

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use super::{display_qr_terminal, load_token, save_token};

const AUTH_URL: &str = "https://login.dingtalk.com/oauth2/auth";
const TOKEN_URL: &str = "https://api.dingtalk.com/v1.0/oauth2/userAccessToken";
const CORP_TOKEN_URL: &str = "https://oapi.dingtalk.com/gettoken";

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CorpTokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    errcode: Option<i64>,
    errmsg: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    corp_id: Option<String>,
}

/// Run the interactive DingTalk QR code login flow.
///
/// Returns the user access token on success.
pub async fn login(
    client: &Client,
    app_key: &str,
    app_secret: &str,
    redirect_uri: Option<&str>,
) -> Result<String> {
    // Check for existing token
    if let Some(saved) = load_token("dingtalk")
        && let Some(token) = saved.get("access_token").and_then(|v| v.as_str())
    {
        info!("using saved DingTalk token");
        return Ok(token.to_owned());
    }

    // 1. Build authorization URL
    let redirect = redirect_uri.unwrap_or("https://oapi.dingtalk.com/connect/oauth2/sns_authorize");
    let auth_url = format!(
        "{AUTH_URL}?client_id={app_key}&response_type=code&scope=openid&redirect_uri={redirect}&state=rsclaw&prompt=consent"
    );

    // 2. Display QR code
    println!("=== DingTalk Login ===");
    display_qr_terminal(&auth_url)?;
    println!("Scan with DingTalk app, then paste the authorization code below.");

    // 3. Wait for user to paste the auth code
    print!("Authorization code: ");
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut code = String::new();
    std::io::stdin().read_line(&mut code)?;
    let code = code.trim().to_owned();

    if code.is_empty() {
        bail!("no authorization code provided");
    }

    // 4. Exchange code for user access token
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&json!({
            "clientId": app_key,
            "clientSecret": app_secret,
            "code": code,
            "grantType": "authorization_code",
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("DingTalk token exchange failed: {status} {body}");
    }

    let data: UserTokenResponse = resp.json().await?;
    let access_token = data
        .access_token
        .context("no access_token in DingTalk response")?;

    info!(
        corp_id = data.corp_id.as_deref().unwrap_or("?"),
        "DingTalk login successful"
    );

    // 5. Save token
    save_token(
        "dingtalk",
        &json!({
            "access_token": access_token,
            "refresh_token": data.refresh_token,
            "expires_in": data.expires_in,
            "corp_id": data.corp_id,
        }),
    )?;

    Ok(access_token)
}

/// Get corp-level access token (for robot/app API calls).
pub async fn get_corp_token(client: &Client, app_key: &str, app_secret: &str) -> Result<String> {
    let resp: CorpTokenResponse = client
        .get(CORP_TOKEN_URL)
        .query(&[("appkey", app_key), ("appsecret", app_secret)])
        .send()
        .await?
        .json()
        .await?;

    if resp.errcode.unwrap_or(0) != 0 {
        bail!(
            "DingTalk corp token failed: {} (code {})",
            resp.errmsg.as_deref().unwrap_or("unknown"),
            resp.errcode.unwrap_or(-1)
        );
    }

    resp.access_token.context("no access_token in response")
}
