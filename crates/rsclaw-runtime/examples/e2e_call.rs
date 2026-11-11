//! Call a gateway through an end-to-end encrypted session, for debugging.
//!
//! ```text
//! cargo run -p rsclaw-runtime --example e2e_call -- \
//!     --url http://127.0.0.1:18888 --pub <e2e_pub> --key ./device.key \
//!     GET /api/v1/inbox/conversations
//! cargo run ... -- --url ... --pub ... --key ... --events /api/v1/inbox/events
//! ```
//!
//! The token comes from `RSCLAW_E2E_TOKEN` (the gateway token or a scoped
//! token), so it stays out of the shell history. `--key` holds this
//! client's device key and is created on first use; a scoped token binds to
//! it, so keep using the same file.

use std::{collections::BTreeMap, time::SystemTime};

use anyhow::{Context as _, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::StreamExt as _;
use rsclaw_runtime::e2e::{InnerRequest, client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut url, mut public, mut key_path, mut events) = (None, None, None, false);
    let mut rest = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--url" => url = args.next(),
            "--pub" => public = args.next(),
            "--key" => key_path = args.next(),
            "--events" => events = true,
            _ => rest.push(a),
        }
    }
    let url = url.context("--url is required")?;
    let url = url.trim_end_matches('/');
    let public = public.context("--pub is required")?;
    let key_path = key_path.context("--key is required")?;
    let token = std::env::var("RSCLAW_E2E_TOKEN").context("set RSCLAW_E2E_TOKEN")?;

    let device = match std::fs::read_to_string(&key_path) {
        Ok(b64) => STANDARD.decode(b64.trim())?,
        Err(_) => {
            let (private, _) = client::device_keypair();
            std::fs::write(&key_path, STANDARD.encode(&private))?;
            private
        }
    };
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    let http = reqwest::Client::builder().no_proxy().build()?;

    let (pending, hello) = client::start(&public, &device, &token, now)?;
    let reply = http
        .post(format!("{url}/api/v1/e2e/handshake"))
        .json(&hello)
        .send()
        .await?;
    if !reply.status().is_success() {
        bail!(
            "handshake refused: {} {}",
            reply.status(),
            reply.text().await?
        );
    }
    let mut session = pending.finish(&reply.json().await?)?;
    eprintln!("session: {}", session.info["principal"]);

    if events {
        let path = rest.first().context("--events needs a path")?;
        let uri = session.events_uri(path);
        let res = http.get(format!("{url}{uri}")).send().await?;
        if !res.status().is_success() {
            bail!("events refused: {} {}", res.status(), res.text().await?);
        }
        let mut body = res.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = body.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(end) = buf.find("\n\n") {
                let frame: String = buf.drain(..end + 2).collect();
                for line in frame.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        let plain = session.open(&serde_json::from_str(data)?)?;
                        print!("{}", String::from_utf8_lossy(&plain));
                    }
                }
            }
        }
        return Ok(());
    }

    let (method, path) = match rest.as_slice() {
        [m, p] => (m.clone(), p.clone()),
        [m, p, _] => (m.clone(), p.clone()),
        _ => bail!("usage: METHOD PATH [JSON BODY]"),
    };
    let body = rest.get(2).map(|b| STANDARD.encode(b));
    let mut headers = BTreeMap::new();
    if body.is_some() {
        headers.insert("content-type".to_owned(), "application/json".to_owned());
    }
    let env = session.seal_call(&InnerRequest {
        method,
        path,
        headers,
        body,
    });
    let res = http
        .post(format!("{url}/api/v1/e2e/call?sid={}", session.sid))
        .json(&env)
        .send()
        .await?;
    if !res.status().is_success() {
        bail!("call refused: {} {}", res.status(), res.text().await?);
    }
    let inner = session.open_reply(&res.json().await?)?;
    eprintln!("status: {}", inner.status);
    println!("{}", String::from_utf8_lossy(&STANDARD.decode(inner.body)?));
    Ok(())
}
