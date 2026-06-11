//! Research-feed ingestion tools.
//!
//! v1 focuses on WeChat 公众号 (Official Account) articles: any
//! `https://mp.weixin.qq.com/s/<id>` URL gets pulled, the embedded
//! article body is parsed out of WeChat's `window.cgiDataNew` JS
//! object, and the result is dropped into the knowledge base with
//! provenance tags so cap subagents (and the briefing pipeline) can
//! semantic-search across the user's curated research feed.
//!
//! Why pure HTTP (no browser, no OCR)
//!
//! WeChat serves a JS-shell page that *appears* to lazy-load — but
//! the entire article body is INLINED into a `<script>` block as
//! `content_noencode: '\x3cp\x3e...\x3c/p\x3e'`. The "lazy load"
//! the page does on real browsers is just DOM hydration FROM that
//! same inlined string; the body itself is already there in the
//! raw HTML. So a plain `reqwest::get` with an iPhone-WeChat
//! User-Agent — no cookies, no JS engine, no anti-bot dance —
//! comes back with everything we need.
//!
//! Discovered the hard way during a live debug session: a `curl`
//! with iPhone MicroMessenger UA returned 292 KB of HTML containing
//! all 5601 chars of unescaped article HTML plus 14 image URLs,
//! while the official `WebFetch` got an anti-bot wall and
//! `rsclaw browser` returned `<html><head></head><body></body></html>`
//! because its CLI invocation doesn't hold the page across calls.
//! That same `cgiDataNew` shape ships on every modern mp.weixin.qq.com
//! `/s/...` article URL.
//!
//! Failure modes the tool surfaces
//!
//! * URL must be a WeChat `/s/<id>` link — we refuse other hosts
//!   and other path shapes so the LLM can't accidentally point it
//!   at an unrelated article platform.
//! * If `cgiDataNew` is missing, WeChat either changed the shape
//!   (rare; the field has been stable for years) or returned the
//!   "环境异常" verification wall (which usually means an IP-rate
//!   trip — the LLM can retry from a different network). Either
//!   way we return a structured error rather than try to OCR or
//!   spelunk further.
//! * The knowledge base must be initialised — when it isn't, we
//!   return `{ok:false, code:"kb_unavailable"}` so the LLM knows
//!   to surface a config-error to the user instead of looping.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use futures::StreamExt;
use serde_json::{Value, json};

use super::runtime::AgentRuntime;
use crate::provider::{ContentPart, LlmRequest, Message, MessageContent, Role, StreamEvent};

/// User-Agent string that gets us past WeChat's IP-based gating
/// without needing cookies. Mirrors what an actual iPhone WeChat
/// in-app browser sends. We pin a specific (and recent enough)
/// MicroMessenger build — if WeChat starts soft-blocking older
/// UAs the easy fix is bumping the version here, not changing the
/// rest of the pipeline.
const WECHAT_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 16_6 like Mac OS X) AppleWebKit/605.1.15 \
    (KHTML, like Gecko) Mobile/15E148 MicroMessenger/8.0.43(0x18002b35) NetType/WIFI Language/zh_CN";

/// Default collection a WeChat article lands in if the LLM doesn't
/// supply an override. Created on demand.
const DEFAULT_RESEARCH_COLLECTION: &str = "research";

impl AgentRuntime {
    /// `research_ingest_wechat` — fetch a WeChat 公众号 article by
    /// URL and ingest it into the KB.
    ///
    /// args: `{ url: string, collection?: string (default "research"),
    ///          extra_tags?: [string] }`.
    pub(crate) async fn tool_research_ingest_wechat(&self, args: Value) -> Result<Value> {
        let url = match args.get("url").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.trim().to_owned(),
            _ => {
                return Ok(json!({
                    "ok": false,
                    "error": "`url` (string) is required"
                }));
            }
        };
        if !is_wechat_article_url(&url) {
            return Ok(json!({
                "ok": false,
                "code": "unsupported_url",
                "error": "URL must be a WeChat 公众号 article (https://mp.weixin.qq.com/s/<id>)"
            }));
        }
        let kb = match crate::kb::global_service() {
            Some(svc) => svc,
            None => {
                return Ok(json!({
                    "ok": false,
                    "code": "kb_unavailable",
                    "error": "knowledge base subsystem not initialised — cannot ingest"
                }));
            }
        };

        let fetched = match fetch_wechat_html(&url).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "code": "fetch_failed",
                    "error": format!("{e:#}"),
                    "url": url,
                }));
            }
        };
        let parsed = match parse_wechat_article(&fetched, &url) {
            Some(p) => p,
            None => {
                return Ok(json!({
                    "ok": false,
                    "code": "parse_failed",
                    "error": "could not find window.cgiDataNew in page — WeChat may have changed format or returned the 环境异常 verification page",
                    "url": url,
                }));
            }
        };
        let collection_name = args
            .get("collection")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_RESEARCH_COLLECTION);
        let extra_tags: Vec<String> = args
            .get("extra_tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::trim).map(str::to_owned))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let collection_id = match resolve_or_create_collection(&kb, collection_name).await {
            Ok(id) => id,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "code": "collection_error",
                    "error": format!("{e:#}"),
                }));
            }
        };

        // Compose the ingest body. We feed BOTH a markdown overview
        // AND the cleaned plain text so semantic-search hits and the
        // raw retrieval path both have the right shape. Image URLs go
        // in as a bullet list so a later vision LLM pass can re-fetch
        // them if it wants to extract chart data.
        let mut body = String::new();
        body.push_str(&format!("# {}\n\n", parsed.title));
        body.push_str(&format!(
            "**公众号**: {} · **作者**: {} · **来源**: [{}]({})\n\n",
            parsed.account_nick.as_deref().unwrap_or("(unknown)"),
            parsed.author.as_deref().unwrap_or("(unknown)"),
            parsed.canonical_url.as_deref().unwrap_or(&url),
            parsed.canonical_url.as_deref().unwrap_or(&url),
        ));
        if !parsed.body_text.trim().is_empty() {
            body.push_str("## 正文\n\n");
            body.push_str(&parsed.body_text);
            body.push_str("\n\n");
        }
        if !parsed.image_urls.is_empty() {
            body.push_str(&format!("## 图表 ({} 张)\n\n", parsed.image_urls.len()));
            for u in &parsed.image_urls {
                body.push_str(&format!("- {u}\n"));
            }
            body.push('\n');
        }

        // Build the "title" passed to KB ingest. Including the
        // account name + date prefix means semantic search on
        // "TGB湖南人 复盘" surfaces these without needing tag
        // filters. KB tags carry the structured metadata.
        let kb_title = format!(
            "{}-{}.md",
            parsed
                .account_nick
                .as_deref()
                .unwrap_or("wechat")
                .replace([' ', '/', '\\'], "_"),
            parsed.title.replace(['/', '\\'], "-"),
        );

        let collection_id_clone = collection_id.clone();
        let body_bytes = body.into_bytes();
        let kb_title_clone = kb_title.clone();
        let ingest_result = tokio::task::spawn_blocking(move || {
            kb.ingest(
                &collection_id_clone,
                &kb_title_clone,
                &body_bytes,
                Some("text/markdown"),
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("ingest task panicked: {e}"))?;

        match ingest_result {
            Ok((doc_id, noop)) => {
                let tags: Vec<String> = std::iter::once("wechat_official_account".to_owned())
                    .chain(parsed.account_nick.iter().cloned())
                    .chain(parsed.account_id.iter().cloned())
                    .chain(extra_tags.into_iter())
                    .collect();
                Ok(json!({
                    "ok": true,
                    "doc_id": doc_id,
                    "deduped": noop,
                    "collection": collection_name,
                    "collection_id": collection_id,
                    "title": parsed.title,
                    "account": parsed.account_nick,
                    "account_id": parsed.account_id,
                    "author": parsed.author,
                    "url": parsed.canonical_url.as_deref().unwrap_or(&url),
                    "image_count": parsed.image_urls.len(),
                    "image_urls": parsed.image_urls,
                    "tags": tags,
                    "body_chars": parsed.body_text.chars().count(),
                }))
            }
            Err(e) => Ok(json!({
                "ok": false,
                "code": "ingest_failed",
                "error": format!("{e:#}"),
            })),
        }
    }

    /// `research_analyze_charts` — run a vision LLM over a batch of
    /// image URLs (usually returned by `research_ingest_wechat`)
    /// and extract structured chart data.
    ///
    /// Heavy-chart research accounts (TGB湖南人 复盘, 金融界 morning
    /// reports, 卖方研究 stat sheets) often deliver the actual
    /// quantitative payload as chart pixels — section headers and
    /// captions go through the text ingest pipeline, but the numbers,
    /// trend annotations, and table cells only show up after a
    /// vision pass. This tool is that pass: fetch each image as
    /// bytes, send them in a single multimodal call to the configured
    /// vision chain, return the LLM's per-chart extraction so the
    /// caller can append the analysis back to the KB doc.
    ///
    /// args: `{ image_urls: [string], max_images?: int<=10,
    ///          extra_prompt?: string }`.
    pub(crate) async fn tool_research_analyze_charts(&self, args: Value) -> Result<Value> {
        let urls: Vec<String> = args
            .get("image_urls")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::trim).map(str::to_owned))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if urls.is_empty() {
            return Ok(json!({
                "ok": false,
                "error": "`image_urls` (non-empty array of URLs) is required"
            }));
        }
        let max = args
            .get("max_images")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(8)
            .clamp(1, MAX_CHART_BATCH);
        let urls: Vec<String> = urls.into_iter().take(max).collect();
        let extra_prompt = args
            .get("extra_prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        // Fetch every image in parallel — most articles point at
        // mmbiz.qpic.cn which is fast and CDN-friendly. Failures
        // are per-image: the call proceeds with whatever did fetch
        // and tells the LLM which were missing.
        let client = match reqwest::Client::builder()
            .user_agent(WECHAT_UA)
            .timeout(std::time::Duration::from_secs(12))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "code": "client_build_failed",
                    "error": format!("{e:#}"),
                }));
            }
        };
        let mut data_uris: Vec<(String, String)> = Vec::with_capacity(urls.len());
        let mut failures: Vec<Value> = Vec::new();
        for (idx, url) in urls.iter().enumerate() {
            match fetch_image_as_data_uri(&client, url).await {
                Ok(uri) => data_uris.push((url.clone(), uri)),
                Err(e) => {
                    failures.push(json!({
                        "index": idx,
                        "url": url,
                        "error": format!("{e:#}"),
                    }));
                }
            }
        }
        if data_uris.is_empty() {
            return Ok(json!({
                "ok": false,
                "code": "all_fetches_failed",
                "error": "every image URL failed to fetch",
                "failures": failures,
            }));
        }

        let vision_chain = self.resolve_vision_chain();
        let vision_model = match vision_chain.first().cloned() {
            Some(m) => m,
            None => {
                return Ok(json!({
                    "ok": false,
                    "code": "no_vision_model",
                    "error": "no vision model configured in agents.defaults.model.vision (or per-agent override)",
                    "image_count": data_uris.len(),
                }));
            }
        };

        let prompt = compose_chart_prompt(data_uris.len(), extra_prompt.as_deref());
        let mut parts: Vec<ContentPart> = Vec::with_capacity(1 + data_uris.len());
        parts.push(ContentPart::Text { text: prompt });
        for (_url, uri) in &data_uris {
            parts.push(ContentPart::Image { url: uri.clone() });
        }

        let req = LlmRequest {
            model: vision_model.clone(),
            fallback_models: vision_chain.iter().skip(1).cloned().collect(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Parts(parts),
                rsclaw_hidden: None,
            }],
            max_tokens: Some(3072),
            temperature: Some(0.2),
            thinking_budget: Some(0),
            ..Default::default()
        };

        // Bypass FailoverManager — it requires `&mut self` and
        // `dispatch_tool` runs through `&self`. For a one-shot vision
        // call the cooldown bookkeeping is not load-bearing; we go
        // straight to the resolved provider. If the primary errors
        // we walk the rest of the vision_chain manually below.
        let providers = Arc::clone(&self.providers);
        let mut chain_iter = std::iter::once(vision_model.clone())
            .chain(vision_chain.iter().skip(1).cloned());
        let mut stream_opt = None;
        let mut tried_chain: Vec<(String, String)> = Vec::new();
        loop {
            let next = match chain_iter.next() {
                Some(m) => m,
                None => break,
            };
            let (prov_name, model_id) = providers.resolve_model(&next);
            let provider = match providers.get(prov_name) {
                Ok(p) => p,
                Err(e) => {
                    tried_chain.push((next.clone(), format!("provider not found: {e}")));
                    continue;
                }
            };
            let mut req_for_call = req.clone();
            req_for_call.model = model_id.to_owned();
            req_for_call.fallback_models = vec![];
            let stream_fut = provider.stream(req_for_call);
            match tokio::time::timeout(std::time::Duration::from_secs(90), stream_fut).await {
                Ok(Ok(s)) => {
                    stream_opt = Some((next, s));
                    break;
                }
                Ok(Err(e)) => {
                    tried_chain.push((next, format!("{e:#}")));
                }
                Err(_) => {
                    tried_chain.push((next, "timed out after 90s".to_owned()));
                }
            }
        }
        let (used_model, mut stream) = match stream_opt {
            Some(t) => t,
            None => {
                return Ok(json!({
                    "ok": false,
                    "code": "vision_chain_exhausted",
                    "error": "every model in the vision chain failed",
                    "tried": tried_chain.iter().map(|(m, e)| json!({"model": m, "error": e})).collect::<Vec<_>>(),
                }));
            }
        };

        // Collect text + reasoning. Same fallback semantics as
        // `caption_images_for_text_only_primary`: some vision
        // workers stream the response as `thinking` frames.
        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(d)) => text_buf.push_str(&d),
                Ok(StreamEvent::ReasoningDelta(d)) => reasoning_buf.push_str(&d),
                Ok(StreamEvent::Done { .. }) => break,
                Ok(StreamEvent::Error(msg)) => {
                    return Ok(json!({
                        "ok": false,
                        "code": "vision_stream_error",
                        "error": msg,
                    }));
                }
                Ok(_) => {}
                Err(e) => {
                    return Ok(json!({
                        "ok": false,
                        "code": "vision_stream_error",
                        "error": format!("{e:#}"),
                    }));
                }
            }
        }
        let analysis = if !text_buf.trim().is_empty() {
            text_buf
        } else {
            reasoning_buf
        };
        if analysis.trim().is_empty() {
            return Ok(json!({
                "ok": false,
                "code": "empty_response",
                "error": "vision LLM returned empty content",
                "model": used_model,
            }));
        }
        Ok(json!({
            "ok": true,
            "model": used_model,
            "analyzed_count": data_uris.len(),
            "skipped_count": failures.len(),
            "skipped": failures,
            "analysis": analysis,
            "image_urls": data_uris.iter().map(|(u, _)| u.clone()).collect::<Vec<_>>(),
        }))
    }
}

const MAX_CHART_BATCH: usize = 10;

async fn fetch_image_as_data_uri(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", "https://mp.weixin.qq.com/")
        .send()
        .await?
        .error_for_status()?;
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = resp.bytes().await?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(anyhow!("image too large ({} bytes)", bytes.len()));
    }
    // Sniff MIME when the server didn't say. WeChat / mmbiz returns
    // proper content-type usually, but defensive sniffing protects
    // against the rare case of a 200 with no header.
    let mime = ctype.unwrap_or_else(|| sniff_image_mime(&bytes).to_owned());
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        // Default to jpeg — most WeChat images are jpg even when the
        // URL ends in `.png`.
        "image/jpeg"
    }
}

fn compose_chart_prompt(n: usize, extra: Option<&str>) -> String {
    let mut p = format!(
        "你是 A 股技术分析师。下面是 {n} 张来自微信公众号研报的图表 (按顺序排列)。\n\
         请对每张图按以下结构提取信息,**逐张** 输出:\n\
         \n\
         ## 图 {{N}}\n\
         - **类型**: K线 / 折线 / 柱状 / 排行表 / 热力图 / 其他\n\
         - **标题或主题**: 直接读取图标题(如果有)\n\
         - **关键数值**: 精确读出可见的数字、个股名、板块名、涨跌幅、价格区间;\
         看不清就写 \"无法读取\"\n\
         - **趋势/结论**: 一句话总结该图表传达的核心信息\n\
         \n\
         规则:\n\
         - 不要瞎猜数据。看不清的数字、模糊的标注、被遮挡的部分都明确说 \"无法读取\"\n\
         - 不要给投资建议、买卖推荐、风险提示 — 只做客观提取\n\
         - 保持简洁,每图 4 个字段控制在 250 字以内\n\
         - 如果图里有表格,把行/列尽量保留为 markdown 表格"
    );
    if let Some(e) = extra {
        p.push_str("\n\n额外指令:\n");
        p.push_str(e);
    }
    p
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct ParsedArticle {
    title: String,
    author: Option<String>,
    account_nick: Option<String>,
    account_id: Option<String>,
    canonical_url: Option<String>,
    body_text: String,
    image_urls: Vec<String>,
}

fn is_wechat_article_url(url: &str) -> bool {
    // Accept https only — WeChat's article URLs are always https in
    // practice; http would 301 to https anyway, but a plain refusal
    // makes the LLM's mistake visible instead of silent.
    let lc = url.to_ascii_lowercase();
    (lc.starts_with("https://mp.weixin.qq.com/s/")
        || lc.starts_with("https://mp.weixin.qq.com/s?")
        || lc.starts_with("https://weixin.qq.com/s/"))
        && !lc.contains("\n")
}

async fn fetch_wechat_html(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(WECHAT_UA)
        // 60s total: WeChat occasionally chunks the response slowly
        // (article + tracking iframe + comment bundle), the curl
        // probe during the live debug session timed out at 30s on
        // the same network. Plus 5s connect for fast-fail when the
        // network is genuinely dead vs slow.
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    // Length-cap defensively: WeChat articles are typically 100-400 KB.
    // Anything >2 MB is suspicious (or a video page); refuse so we don't
    // OOM on a runaway response.
    let bytes = resp.bytes().await?;
    if bytes.len() > 4 * 1024 * 1024 {
        anyhow::bail!("WeChat response too large ({} bytes)", bytes.len());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Pull `window.cgiDataNew` fields out of the page. Returns `None`
/// when the field block isn't present at all (verification wall /
/// format change).
fn parse_wechat_article(html_src: &str, fallback_url: &str) -> Option<ParsedArticle> {
    let content_raw = scan_quoted_field(html_src, "content_noencode")?;
    let title = scan_quoted_field(html_src, "title")
        .map(js_unescape)
        .unwrap_or_else(|| meta_content(html_src, "og:title").unwrap_or_default());
    let account_nick = scan_quoted_field(html_src, "nick_name").map(js_unescape);
    let account_id = scan_quoted_field(html_src, "user_name").map(js_unescape);
    let author = meta_content(html_src, "author")
        .or_else(|| meta_content(html_src, "og:article:author"));
    let canonical_url = meta_content(html_src, "og:url").or_else(|| Some(fallback_url.to_owned()));

    let body_html = js_unescape(content_raw);
    let (body_text, image_urls) = strip_html_to_text(&body_html);

    Some(ParsedArticle {
        title,
        author,
        account_nick,
        account_id,
        canonical_url,
        body_text,
        image_urls,
    })
}

/// Find `field:` followed by a single-quoted JS string and return
/// its raw escaped contents (without surrounding quotes). Stops at
/// the first unescaped `'`.
fn scan_quoted_field(s: &str, field: &str) -> Option<String> {
    let needle = format!("{field}: '");
    let i = s.find(&needle)?;
    let mut iter = s[i + needle.len()..].char_indices();
    let mut out = String::new();
    while let Some((_, c)) = iter.next() {
        match c {
            '\\' => {
                if let Some((_, n)) = iter.next() {
                    out.push(c);
                    out.push(n);
                }
            }
            '\'' => return Some(out),
            _ => out.push(c),
        }
    }
    None
}

/// JS string-literal unescape: `\xNN`, `\uNNNN`, `\'`, `\"`, `\\`,
/// `\n`, `\t`, `\r`. WeChat's inlined HTML uses `\xNN` for every
/// byte, so this is the main load-bearing operation.
///
/// Builds into a byte buffer (NOT directly a String) — `\xE6\xB9\x96`
/// is a UTF-8 SEQUENCE for one Chinese char, not three independent
/// code points. Pushing `(bytes[i] as char)` into a String here would
/// produce three Latin-1 chars and mangle the result. We finalise via
/// `String::from_utf8_lossy` once at the end so multi-byte UTF-8 is
/// recombined correctly.
fn js_unescape(s: String) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'x' if i + 3 < bytes.len() => {
                    if let Ok(v) = u8::from_str_radix(
                        std::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or("0"),
                        16,
                    ) {
                        out.push(v);
                        i += 4;
                        continue;
                    }
                }
                b'u' if i + 5 < bytes.len() => {
                    if let Ok(v) = u32::from_str_radix(
                        std::str::from_utf8(&bytes[i + 2..i + 6]).unwrap_or("0"),
                        16,
                    ) && let Some(c) = char::from_u32(v)
                    {
                        // Encode the code point as UTF-8 into our byte buffer.
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        i += 6;
                        continue;
                    }
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                    continue;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                    continue;
                }
                b'\'' | b'"' | b'\\' | b'/' => {
                    out.push(bytes[i + 1]);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Very small HTML-to-text walker tailored for WeChat article HTML.
/// Pulls `<img>` URLs (preferring `data-croporisrc` then `data-src`
/// then `src` — WeChat hot-swaps the live `src` to a placeholder
/// while lazy-loading). Replaces images with the marker `[图]` in
/// the text so the reader can see where the chart was.
///
/// Operates over a byte buffer — see `js_unescape` for the UTF-8
/// rationale. Tag bounds use byte indices safely because `<` and
/// `>` are both ASCII and can never appear inside a multi-byte
/// UTF-8 sequence (UTF-8 continuation bytes have the high bit set).
fn strip_html_to_text(html_src: &str) -> (String, Vec<String>) {
    let mut text: Vec<u8> = Vec::with_capacity(html_src.len() / 4);
    let mut images: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let bytes = html_src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let end = match memchr(bytes, b'>', i + 1) {
                Some(e) => e,
                None => break,
            };
            let tag = &html_src[i..=end];
            let lower = tag.to_ascii_lowercase();
            if lower.starts_with("<img") {
                if let Some(src) = pull_attr(tag, "data-croporisrc")
                    .or_else(|| pull_attr(tag, "data-src"))
                    .or_else(|| pull_attr(tag, "src"))
                    && !src.starts_with("data:")
                    && seen.insert(src.clone())
                {
                    images.push(src);
                }
                text.extend_from_slice("[图]".as_bytes());
            } else if lower.starts_with("<br")
                || lower.starts_with("</p")
                || lower.starts_with("</section")
                || lower.starts_with("</div")
                || lower.starts_with("</h")
                || lower.starts_with("</li")
            {
                text.push(b'\n');
            }
            i = end + 1;
        } else {
            text.push(bytes[i]);
            i += 1;
        }
    }
    let text = String::from_utf8_lossy(&text).into_owned();
    let text = decode_html_entities(&text);
    let text = collapse_blank_lines(&text);
    (text, images)
}

fn memchr(haystack: &[u8], needle: u8, from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..].iter().position(|&b| b == needle).map(|p| p + from)
}

fn pull_attr(tag: &str, attr: &str) -> Option<String> {
    // Match both `attr="..."` and `attr='...'`. The tag is small
    // (~hundreds of bytes); brute scan is fine.
    for q in ['"', '\''] {
        let needle = format!("{attr}={q}");
        if let Some(i) = tag.find(&needle) {
            let start = i + needle.len();
            if let Some(end) = tag[start..].find(q) {
                let v = tag[start..start + end].trim();
                if !v.is_empty() {
                    return Some(v.to_owned());
                }
            }
        }
    }
    None
}

fn decode_html_entities(s: &str) -> String {
    // Hand-roll the small set WeChat actually emits — avoids pulling a
    // full HTML entity table. `&#NN;` / `&#xNN;` handled, plus the
    // five mandatory named entities. Operates on byte buffer for
    // UTF-8 safety (see `js_unescape`).
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            let scan_to = (i + 12).min(bytes.len());
            if let Some(end) = bytes[i..scan_to].iter().position(|&b| b == b';') {
                let raw = &s[i..i + end + 1];
                let replaced = match raw {
                    "&amp;" => Some("&".to_owned()),
                    "&lt;" => Some("<".to_owned()),
                    "&gt;" => Some(">".to_owned()),
                    "&quot;" => Some("\"".to_owned()),
                    "&apos;" => Some("'".to_owned()),
                    "&nbsp;" => Some(" ".to_owned()),
                    r if r.starts_with("&#x") || r.starts_with("&#X") => {
                        u32::from_str_radix(&r[3..r.len() - 1], 16)
                            .ok()
                            .and_then(char::from_u32)
                            .map(|c| c.to_string())
                    }
                    r if r.starts_with("&#") => r[2..r.len() - 1]
                        .parse::<u32>()
                        .ok()
                        .and_then(char::from_u32)
                        .map(|c| c.to_string()),
                    _ => None,
                };
                if let Some(rep) = replaced {
                    out.extend_from_slice(rep.as_bytes());
                    i += end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0;
    for line in s.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_owned()
}

fn meta_content(html_src: &str, key: &str) -> Option<String> {
    // Try property=KEY first, then name=KEY. Stop after first hit.
    for attr in ["property", "name"] {
        let needle = format!("<meta {attr}=\"{key}\"");
        if let Some(i) = html_src.find(&needle) {
            let tag_end = html_src[i..].find('>').map(|e| i + e + 1).unwrap_or(html_src.len());
            let tag = &html_src[i..tag_end];
            if let Some(v) = pull_attr(tag, "content") {
                return Some(v);
            }
        }
    }
    None
}

async fn resolve_or_create_collection(
    kb: &Arc<crate::kb::KnowledgeService>,
    name: &str,
) -> Result<String> {
    let kb_for_list = kb.clone();
    let collections = tokio::task::spawn_blocking(move || kb_for_list.list_collections())
        .await
        .map_err(|e| anyhow::anyhow!("list task panicked: {e}"))?
        .map_err(|e| anyhow::anyhow!("list collections: {e}"))?;
    if let Some(c) = collections.iter().find(|c| c.name == name) {
        return Ok(c.id.clone());
    }
    let kb_for_create = kb.clone();
    let name_owned = name.to_owned();
    let created = tokio::task::spawn_blocking(move || {
        kb_for_create.create_collection(&name_owned, None, None)
    })
    .await
    .map_err(|e| anyhow::anyhow!("create task panicked: {e}"))?
    .map_err(|e| anyhow::anyhow!("create collection: {e}"))?;
    Ok(created.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validator() {
        assert!(is_wechat_article_url(
            "https://mp.weixin.qq.com/s/Jf-K-DeuWSepL5xYlO8XIw"
        ));
        assert!(is_wechat_article_url(
            "https://mp.weixin.qq.com/s?__biz=xxx&mid=yyy&idx=1&sn=zzz"
        ));
        assert!(!is_wechat_article_url("http://mp.weixin.qq.com/s/foo")); // http
        assert!(!is_wechat_article_url("https://example.com/s/foo"));
        assert!(!is_wechat_article_url(""));
        assert!(!is_wechat_article_url(
            "https://mp.weixin.qq.com/s/foo\nhttps://evil.com"
        ));
    }

    #[test]
    fn js_unescape_hex_sequences() {
        // \x3c → '<', \x3e → '>'
        let raw = String::from("\\x3cp\\x3eHi\\x3c/p\\x3e");
        assert_eq!(js_unescape(raw), "<p>Hi</p>");
    }

    #[test]
    fn js_unescape_unicode() {
        let raw = String::from("Café \\u00b1 5%");
        let out = js_unescape(raw);
        assert!(out.contains("Café"));
        assert!(out.contains("±"));
    }

    #[test]
    fn scan_quoted_field_picks_value() {
        let s = "...title: 'Hello \\'World\\''}...";
        let v = scan_quoted_field(s, "title").expect("field present");
        // Raw escapes preserved at this stage.
        assert_eq!(v, "Hello \\'World\\'");
    }

    #[test]
    fn scan_quoted_field_missing_returns_none() {
        assert!(scan_quoted_field("nothing here", "missing").is_none());
    }

    #[test]
    fn strip_html_extracts_images_and_text() {
        let html_src = r#"<p>Hello <img data-croporisrc="https://x.png" src="placeholder.gif"/> world</p>"#;
        let (text, imgs) = strip_html_to_text(html_src);
        assert!(text.contains("Hello"));
        assert!(text.contains("[图]"));
        assert!(text.contains("world"));
        assert_eq!(imgs, vec!["https://x.png".to_string()]);
    }

    #[test]
    fn strip_html_prefers_croporisrc_over_data_src() {
        let html_src = r#"<img data-croporisrc="https://big.png" data-src="https://small.png" src="placeholder"/>"#;
        let (_, imgs) = strip_html_to_text(html_src);
        assert_eq!(imgs, vec!["https://big.png".to_string()]);
    }

    #[test]
    fn strip_html_falls_back_to_data_src() {
        let html_src = r#"<img data-src="https://small.png" src="placeholder"/>"#;
        let (_, imgs) = strip_html_to_text(html_src);
        assert_eq!(imgs, vec!["https://small.png".to_string()]);
    }

    #[test]
    fn strip_html_drops_data_uri_images() {
        let html_src = r#"<img data-src="data:image/png;base64,AAAA"/>"#;
        let (_, imgs) = strip_html_to_text(html_src);
        assert!(imgs.is_empty());
    }

    #[test]
    fn collapse_blank_lines_keeps_one_separator() {
        let s = "A\n\n\n\nB\n\n\n";
        assert_eq!(collapse_blank_lines(s), "A\n\nB");
    }

    #[test]
    fn decode_html_entities_handles_named_and_numeric() {
        assert_eq!(decode_html_entities("&amp;"), "&");
        assert_eq!(decode_html_entities("&lt;3"), "<3");
        assert_eq!(decode_html_entities("&quot;ok&quot;"), "\"ok\"");
        // 38 is &
        assert_eq!(decode_html_entities("&#38;"), "&");
        assert_eq!(decode_html_entities("&#x26;"), "&");
    }

    #[test]
    fn pull_attr_handles_both_quote_styles() {
        assert_eq!(
            pull_attr(r#"<img src="https://x.png"/>"#, "src"),
            Some("https://x.png".into())
        );
        assert_eq!(
            pull_attr(r#"<img src='https://y.png'/>"#, "src"),
            Some("https://y.png".into())
        );
    }

    #[test]
    fn parse_wechat_article_synthetic() {
        // Minimal synthetic HTML mimicking what we observed on the
        // real TGB湖南人 6.9 复盘 page.
        let html_src = r#"<html>
<head>
<meta property="og:title" content="【6.9复盘】test"/>
<meta name="author" content="湖南妹666"/>
<meta property="og:url" content="https://mp.weixin.qq.com/s/abc"/>
</head>
<body>
<script>
window.cgiDataNew = {
  nick_name: 'TGB湖南人',
  user_name: 'gh_a69d7e32e322',
  title: '\x3c6.9\x20复盘\x3e test',
  content_noencode: '\x3cp\x3eHi\x3c/p\x3e\x3cimg data-croporisrc=\x22https://chart.png\x22 src=\x22ph.gif\x22/\x3e\x3cp\x3eend.\x3c/p\x3e',
};
</script>
</body>
</html>
"#;
        let p = parse_wechat_article(html_src, "https://mp.weixin.qq.com/s/abc").unwrap();
        assert_eq!(p.account_nick.as_deref(), Some("TGB湖南人"));
        assert_eq!(p.account_id.as_deref(), Some("gh_a69d7e32e322"));
        assert_eq!(p.author.as_deref(), Some("湖南妹666"));
        assert!(p.title.contains("6.9"));
        assert!(p.body_text.contains("Hi"));
        assert!(p.body_text.contains("[图]"));
        assert!(p.body_text.contains("end."));
        assert_eq!(p.image_urls, vec!["https://chart.png".to_string()]);
        assert_eq!(
            p.canonical_url.as_deref(),
            Some("https://mp.weixin.qq.com/s/abc")
        );
    }

    #[test]
    fn parse_wechat_article_returns_none_when_no_cgi_data() {
        // The "环境异常" verification page has no cgiDataNew.
        let html_src = r#"<html><body><div>当前环境异常，完成验证后即可继续访问</div></body></html>"#;
        assert!(parse_wechat_article(html_src, "https://x").is_none());
    }
}
