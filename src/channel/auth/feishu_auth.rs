//! Feishu Device Authorization onboarding.
//!
//! Scan-to-login flow that auto-creates and configures a bot:
//!   1. init -> get supported auth methods
//!   2. begin -> get QR code URL + device_code
//!   3. User scans with Feishu app
//!   4. poll -> get app_id + app_secret

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use tracing::info;

const FEISHU_ACCOUNTS_URL: &str = "https://accounts.feishu.cn";
const LARK_ACCOUNTS_URL: &str = "https://accounts.larksuite.com";

fn accounts_url(brand: &str) -> &'static str {
    if brand == "lark" {
        LARK_ACCOUNTS_URL
    } else {
        FEISHU_ACCOUNTS_URL
    }
}

#[derive(Debug, Deserialize)]
struct InitResponse {
    nonce: Option<String>,
    supported_auth_methods: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BeginResponse {
    verification_uri_complete: String,
    device_code: String,
    interval: Option<u64>,
    expire_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PollResponse {
    app_id: Option<String>,
    app_secret: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    user_info: Option<PollUserInfo>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PollUserInfo {
    open_id: Option<String>,
    tenant_brand: Option<String>,
}

/// Run the full Feishu device authorization onboarding.
/// Returns (app_id, app_secret, brand) on success.
pub async fn onboard(client: &Client, brand: &str) -> Result<(String, String, String)> {
    let base = accounts_url(brand);

    // 1. Init
    let init_raw = client
        .post(format!("{base}/oauth/v1/app/registration"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("action=init")
        .send()
        .await?
        .text()
        .await?;
    let init_resp: InitResponse =
        serde_json::from_str(&init_raw).context("feishu: init parse failed")?;

    if !init_resp
        .supported_auth_methods
        .contains(&"client_secret".to_owned())
    {
        bail!("feishu: client_secret auth not supported");
    }

    // 2. Begin (must include nonce from init)
    let nonce = init_resp.nonce.as_deref().unwrap_or("");
    let begin_raw = client
        .post(format!("{base}/oauth/v1/app/registration"))
        .form(&[
            ("action", "begin"),
            ("archetype", "PersonalAgent"),
            ("auth_method", "client_secret"),
            ("request_user_info", "open_id"),
            ("nonce", nonce),
        ])
        .send()
        .await?
        .text()
        .await?;
    let begin_resp: BeginResponse =
        serde_json::from_str(&begin_raw).context("feishu: begin parse failed")?;

    // 3. Display QR code
    let sep = if begin_resp.verification_uri_complete.contains('?') {
        "&"
    } else {
        "?"
    };
    let qr_url = format!("{}{sep}from=onboard", begin_resp.verification_uri_complete);
    println!("=== Feishu Bot Setup ===");
    println!("Scan with Feishu app to create and configure your bot:");
    super::display_qr_terminal(&qr_url)?;

    // 4. Poll
    let interval = begin_resp.interval.unwrap_or(5);
    let expire_in = begin_resp.expire_in.unwrap_or(600);
    let device_code = begin_resp.device_code;
    let mut actual_brand = brand.to_owned();

    println!("Waiting for scan...");

    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_secs() > expire_in {
            bail!("feishu: QR code expired ({}s)", expire_in);
        }

        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

        let poll_base = accounts_url(&actual_brand);
        let poll_body = format!("action=poll&device_code={device_code}");

        let resp = client
            .post(format!("{poll_base}/oauth/v1/app/registration"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(poll_body)
            .send()
            .await?;

        let raw = resp.text().await.context("feishu: poll read failed")?;
        let poll: PollResponse = serde_json::from_str(&raw).context("feishu: poll parse failed")?;

        // Check if we need to switch brand (lark vs feishu)
        if let Some(ref user_info) = poll.user_info
            && let Some(ref tb) = user_info.tenant_brand
            && tb == "lark"
            && actual_brand != "lark"
        {
            actual_brand = "lark".to_owned();
            info!("feishu: detected Lark tenant, switching domain");
        }

        // Check for completion (API returns client_id/client_secret or
        // app_id/app_secret)
        let app_id = poll.app_id.or(poll.client_id);
        let app_secret = poll.app_secret.or(poll.client_secret);
        if let (Some(app_id), Some(app_secret)) = (app_id, app_secret) {
            let open_id = poll.user_info.and_then(|u| u.open_id).unwrap_or_default();

            println!("Bot configured successfully!");
            println!("  App ID: {app_id}");

            // Save to auth store
            super::save_token(
                "feishu",
                &serde_json::json!({
                    "app_id": app_id,
                    "app_secret": app_secret,
                    "open_id": open_id,
                    "brand": actual_brand,
                }),
            )?;

            return Ok((app_id, app_secret, actual_brand));
        }

        // Check for error
        if let Some(ref err) = poll.error {
            if err == "authorization_pending" || err == "slow_down" {
                continue;
            }
            bail!("feishu: poll error: {err}");
        }
    }
}
