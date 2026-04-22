//! Web-related agent tools: search, fetch, download, browser.
//!
//! These methods are split out of `runtime.rs` for maintainability. They remain
//! methods on `AgentRuntime` via a separate `impl` block (Rust allows multiple
//! impl blocks for the same type across files in the same crate).

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::platform::{detect_chrome, has_display};
use super::runtime::{AgentRuntime, RunContext, expand_tilde};
use super::web_parsers::{
    extract_html_title, html_dehydrate_to_text, is_captcha_page, lang_to_bing_mkt,
    parse_baidu_results, parse_bing_html_results, parse_ddg_results, parse_sogou_results,
    search_engine_url, truncate_chars, urlencoding,
};
use crate::provider::{Message, MessageContent, Role, StreamEvent};

impl AgentRuntime {
    pub(crate) async fn tool_web_search(&self, args: Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow!("web_search: `query` required"))?;

        // Read config
        let ws_cfg = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.web_search.as_ref());
        let limit = args["limit"]
            .as_u64()
            .unwrap_or_else(|| ws_cfg.and_then(|c| c.max_results).unwrap_or(5) as u64)
            as usize;
        let provider_raw = args["provider"].as_str().unwrap_or("");
        // Normalize: "auto-detect", "auto", "default" -> empty (trigger auto-detect
        // logic)
        let provider = match provider_raw {
            "auto-detect" | "auto" | "default" | "none" => "",
            other => other,
        };

        // Resolve API keys: config first, then env vars
        let resolve_key = |cfg_key: Option<&crate::config::schema::SecretOrString>,
                           env_name: &str|
         -> Option<String> {
            cfg_key
                .and_then(|k| k.resolve_early())
                .or_else(|| std::env::var(env_name).ok())
                .filter(|k| !k.is_empty())
        };
        let brave_key = resolve_key(
            ws_cfg.and_then(|c| c.brave_api_key.as_ref()),
            "BRAVE_API_KEY",
        );
        let google_key = resolve_key(
            ws_cfg.and_then(|c| c.google_api_key.as_ref()),
            "GOOGLE_SEARCH_API_KEY",
        );
        let google_cx = ws_cfg
            .and_then(|c| c.google_cx.clone())
            .or_else(|| std::env::var("GOOGLE_SEARCH_CX").ok());
        let bing_key = resolve_key(ws_cfg.and_then(|c| c.bing_api_key.as_ref()), "BING_API_KEY");
        let serper_key = resolve_key(
            ws_cfg.and_then(|c| c.serper_api_key.as_ref()),
            "SERPER_API_KEY",
        );

        // Auto-detect provider: explicit arg > config default > keyed provider >
        // DuckDuckGo
        let chosen = if !provider.is_empty() {
            provider.to_owned()
        } else if let Some(default) = ws_cfg.and_then(|c| c.provider.as_deref()) {
            default.to_owned()
        } else if serper_key.is_some() {
            "serper".to_owned()
        } else if brave_key.is_some() {
            "brave".to_owned()
        } else if google_key.is_some() && google_cx.is_some() {
            "google".to_owned()
        } else if bing_key.is_some() {
            "bing".to_owned()
        } else {
            "bing-free".to_owned()
        };

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .timeout(Duration::from_secs(15))
            .build()?;

        let mut results: Vec<Value> = match chosen.as_str() {
            "duckduckgo-free" => {
                let base = search_engine_url("duckduckgo");
                let url = format!(
                    "{}?q={}",
                    if base.is_empty() {
                        "https://html.duckduckgo.com/html/"
                    } else {
                        base
                    },
                    urlencoding::encode(query)
                );
                let html = client.get(&url).send().await?.text().await?;
                parse_ddg_results(&html, limit)
            }
            "google" => {
                let (key, cx) = match (google_key, google_cx) {
                    (Some(k), Some(c)) => (k, c),
                    _ => {
                        // Missing google credentials, fall back to DuckDuckGo
                        tracing::warn!(
                            "web_search: google credentials incomplete, falling back to DuckDuckGo"
                        );
                        let url = format!(
                            "{}?q={}",
                            {
                                let b = search_engine_url("duckduckgo");
                                if b.is_empty() {
                                    "https://html.duckduckgo.com/html/"
                                } else {
                                    b
                                }
                            },
                            urlencoding::encode(query)
                        );
                        let html = client.get(&url).send().await?.text().await?;
                        return Ok(
                            json!({"results": parse_ddg_results(&html, limit), "provider": "duckduckgo (fallback)"}),
                        );
                    }
                };
                let base = search_engine_url("google");
                let resp: Value = client
                    .get(if base.is_empty() {
                        "https://www.googleapis.com/customsearch/v1"
                    } else {
                        base
                    })
                    .query(&[
                        ("key", key.as_str()),
                        ("cx", cx.as_str()),
                        ("q", query),
                        ("num", &limit.min(10).to_string()),
                    ])
                    .send()
                    .await?
                    .json()
                    .await?;
                resp["items"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit)
                            .map(|item| {
                                json!({
                                    "title": item["title"].as_str().unwrap_or(""),
                                    "url": item["link"].as_str().unwrap_or(""),
                                    "snippet": item["snippet"].as_str().unwrap_or("")
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "bing" => {
                let key = bing_key.ok_or_else(|| anyhow!("web_search: bing API key not set (config tools.webSearch.bingApiKey or env BING_API_KEY)"))?;
                let base = search_engine_url("bing");
                let resp: Value = client
                    .get(if base.is_empty() {
                        "https://api.bing.microsoft.com/v7.0/search"
                    } else {
                        base
                    })
                    .query(&[("q", query), ("count", &limit.to_string())])
                    .header("Ocp-Apim-Subscription-Key", &key)
                    .send()
                    .await?
                    .json()
                    .await?;
                resp["webPages"]["value"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit)
                            .map(|item| {
                                json!({
                                    "title": item["name"].as_str().unwrap_or(""),
                                    "url": item["url"].as_str().unwrap_or(""),
                                    "snippet": item["snippet"].as_str().unwrap_or("")
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "brave" => {
                let key = brave_key.ok_or_else(|| anyhow!("web_search: brave API key not set (config tools.webSearch.braveApiKey or env BRAVE_API_KEY)"))?;
                let base = search_engine_url("brave");
                let resp: Value = client
                    .get(if base.is_empty() {
                        "https://api.search.brave.com/res/v1/web/search"
                    } else {
                        base
                    })
                    .query(&[("q", query), ("count", &limit.to_string())])
                    .header("X-Subscription-Token", &key)
                    .send()
                    .await?
                    .json()
                    .await?;
                resp["web"]["results"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit)
                            .map(|item| {
                                json!({
                                    "title": item["title"].as_str().unwrap_or(""),
                                    "url": item["url"].as_str().unwrap_or(""),
                                    "snippet": item["description"].as_str().unwrap_or("")
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "serper" => {
                let key = serper_key.ok_or_else(|| anyhow!("web_search: serper API key not set (config tools.webSearch.serperApiKey or env SERPER_API_KEY)"))?;
                let resp: Value = client
                    .post("https://google.serper.dev/search")
                    .header("X-API-KEY", &key)
                    .header("Content-Type", "application/json")
                    .json(&json!({ "q": query, "num": limit.min(10) }))
                    .send()
                    .await?
                    .json()
                    .await?;
                resp["organic"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit)
                            .map(|item| {
                                json!({
                                    "title": item["title"].as_str().unwrap_or(""),
                                    "url": item["link"].as_str().unwrap_or(""),
                                    "snippet": item["snippet"].as_str().unwrap_or("")
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            // Free HTML scraping providers (no API key needed)
            "bing-free" => {
                let lang = self
                    .config
                    .raw
                    .gateway
                    .as_ref()
                    .and_then(|g| g.language.as_deref())
                    .unwrap_or("");
                let is_zh = lang.to_lowercase().starts_with("zh")
                    || lang.to_lowercase().starts_with("chinese");
                let bing_host = if is_zh { "cn.bing.com" } else { "www.bing.com" };
                let mkt = lang_to_bing_mkt(lang);
                let mkt_param = if mkt.is_empty() {
                    String::new()
                } else {
                    format!("&mkt={mkt}&setlang={}", &mkt[..2])
                };
                let url = format!(
                    "https://{bing_host}/search?q={}&count={limit}{mkt_param}",
                    urlencoding::encode(query)
                );
                let html = client
                    .get(&url)
                    .header(
                        "User-Agent",
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
                    )
                    .send()
                    .await?
                    .text()
                    .await?;
                parse_bing_html_results(&html, limit)
            }
            "baidu-free" => {
                let url = format!(
                    "https://www.baidu.com/s?wd={}&rn={limit}",
                    urlencoding::encode(query)
                );
                let html = client
                    .get(&url)
                    .header(
                        "User-Agent",
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
                    )
                    .send()
                    .await?
                    .text()
                    .await?;
                parse_baidu_results(&html, limit)
            }
            "sogou-free" => {
                let url = format!(
                    "https://www.sogou.com/web?query={}&num={limit}",
                    urlencoding::encode(query)
                );
                let html = client
                    .get(&url)
                    .header(
                        "User-Agent",
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
                    )
                    .send()
                    .await?
                    .text()
                    .await?;
                parse_sogou_results(&html, limit)
            }
            other => return Err(anyhow!("web_search: unknown provider `{other}`")),
        };

        // Fallback: if DDG returned empty (captcha), try bing-free
        if results.is_empty() && chosen == "duckduckgo-free" {
            tracing::warn!("web_search: DuckDuckGo returned 0 results, falling back to bing-free");
            let lang = self
                .config
                .raw
                .gateway
                .as_ref()
                .and_then(|g| g.language.as_deref())
                .unwrap_or("");
            let is_zh = lang.to_lowercase().starts_with("zh")
                || lang.to_lowercase().starts_with("chinese");
            let bing_host = if is_zh { "cn.bing.com" } else { "www.bing.com" };
            let mkt = lang_to_bing_mkt(lang);
            let mkt_param = if mkt.is_empty() {
                String::new()
            } else {
                format!("&mkt={mkt}&setlang={}", &mkt[..2])
            };
            let url = format!(
                "https://{bing_host}/search?q={}&count={limit}{mkt_param}",
                urlencoding::encode(query)
            );
            let html = client
                .get(&url)
                .header(
                    "User-Agent",
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
                )
                .send()
                .await?
                .text()
                .await?;
            let fallback = parse_bing_html_results(&html, limit);
            if !fallback.is_empty() {
                results = fallback;
            }
        }

        // --- Multi-provider parallel merge (free providers only) ---
        // When no API key is configured (free scraping mode), run 2 providers
        // concurrently for better coverage. Provider pair selected by language:
        //   zh → random 2 from [bing-free, baidu, sogou, 360]
        //   other → bing-free + duckduckgo
        let free_providers = ["duckduckgo-free", "bing-free", "baidu-free", "sogou-free"];
        let is_free_mode = free_providers.contains(&chosen.as_str());
        if is_free_mode {
            let lang = self.config.raw.gateway.as_ref()
                .and_then(|g| g.language.as_deref())
                .unwrap_or("");
            let is_zh = lang.starts_with("zh")
                || std::env::var("LANG").unwrap_or_default().to_lowercase().contains("zh");

            let pair: [&str; 2] = if is_zh {
                // Chinese: random 2 from 4 free Chinese-friendly providers.
                #[allow(clippy::useless_vec)]
                let mut pool = vec!["bing-free", "baidu-free", "sogou-free"];
                use rand::seq::SliceRandom;
                pool.shuffle(&mut rand::rng());
                [pool[0], pool[1]]
            } else {
                ["bing-free", "duckduckgo-free"]
            };

            // Run both in parallel.
            let (r1, r2) = tokio::join!(
                self.search_provider(pair[0], query, limit, &client),
                self.search_provider(pair[1], query, limit, &client),
            );

            // Merge both into results, dedup by URL.
            results.clear();
            let mut seen_urls = std::collections::HashSet::new();
            for batch in [r1, r2] {
                if let Ok(items) = batch {
                    for r in items {
                        if let Some(url) = r["url"].as_str() {
                            if seen_urls.insert(url.to_owned()) {
                                results.push(r);
                            }
                        }
                    }
                }
            }
        }

        // --- Browser fallback: when all free providers are blocked by CAPTCHA ---
        if results.is_empty() && is_free_mode {
            info!("web_search: all free providers returned empty, trying browser fallback");
            match self.browser_search(query, limit).await {
                Ok(browser_results) if !browser_results.is_empty() => {
                    info!(count = browser_results.len(), "web_search: browser fallback succeeded");
                    results = browser_results;
                }
                Ok(_) => warn!("web_search: browser fallback also returned empty"),
                Err(e) => warn!("web_search: browser fallback failed: {e:#}"),
            }
        }

        // --- Auto-fetch relevant results for deeper content ---
        // Score each result by relevance to the query, only deep-fetch those
        // that are likely useful.  This avoids wasting time on unrelated pages.
        let query_terms: Vec<String> = query.to_lowercase()
            .split_whitespace()
            .filter(|w| w.len() > 1)
            .map(String::from)
            .collect();

        let fetch_urls: Vec<String> = results.iter()
            .filter(|r| {
                let title = r["title"].as_str().unwrap_or("").to_lowercase();
                let snippet = r["snippet"].as_str().unwrap_or("").to_lowercase();
                let haystack = format!("{title} {snippet}");

                // Count how many query terms appear in title+snippet.
                let hits = query_terms.iter()
                    .filter(|t| haystack.contains(t.as_str()))
                    .count();

                // Require at least half the query terms to match, or always
                // include the first 2 results as fallback.
                hits * 2 >= query_terms.len() || hits > 0
            })
            .take(5)
            .filter_map(|r| r["url"].as_str().map(String::from))
            .collect();

        if !fetch_urls.is_empty() {
            let fetch_client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());

            // Fetch all URLs concurrently.
            let fetches = fetch_urls.iter().map(|url| {
                let client = fetch_client.clone();
                let url = url.clone();
                async move {
                    let resp = client.get(&url).send().await.ok()?;
                    let html = resp.text().await.ok()?;
                    let content_type = "text/html"; // assume HTML
                    let md = if content_type.contains("text/html") {
                        html_dehydrate_to_text(&html)
                    } else {
                        html
                    };
                    // Truncate to 2000 chars.
                    let truncated = truncate_chars(&md, 2000);
                    Some((url, truncated))
                }
            });
            let fetched: Vec<Option<(String, String)>> = futures::future::join_all(fetches).await;

            // Attach content to matching results.
            for (url, content) in fetched.into_iter().flatten() {
                for r in results.iter_mut() {
                    if r["url"].as_str() == Some(url.as_str()) {
                        r["content"] = json!(content);
                        break;
                    }
                }
            }
        }

        // If still empty after all attempts, add a hint about API keys.
        if results.is_empty() && is_free_mode {
            let i18n_lang = crate::i18n::default_lang();
            return Ok(json!({
                "results": [],
                "provider": chosen,
                "error": crate::i18n::t("search_captcha_blocked", i18n_lang)
            }));
        }

        Ok(json!({ "results": results, "provider": chosen }))
    }

    /// Helper: run a free scraping search provider and return results.
    pub(crate) async fn search_provider(
        &self,
        provider: &str,
        query: &str,
        limit: usize,
        client: &reqwest::Client,
    ) -> Result<Vec<Value>> {
        let lang = self.config.raw.gateway.as_ref()
            .and_then(|g| g.language.as_deref())
            .unwrap_or("");
        let is_zh = lang.to_lowercase().starts_with("zh")
            || lang.to_lowercase().starts_with("chinese");
        let (html, results) = match provider {
            "bing-free" => {
                let bing_host = if is_zh { "cn.bing.com" } else { "www.bing.com" };
                let mkt = lang_to_bing_mkt(lang);
                let mkt_param = if mkt.is_empty() {
                    String::new()
                } else {
                    format!("&mkt={mkt}&setlang={}", &mkt[..2])
                };
                let url = format!(
                    "https://{bing_host}/search?q={}&count={limit}{mkt_param}",
                    urlencoding::encode(query)
                );
                let html = client
                    .get(&url)
                    .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
                    .send().await?.text().await?;
                let r = parse_bing_html_results(&html, limit);
                (html, r)
            }
            "duckduckgo-free" => {
                let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding::encode(query));
                let html = client.get(&url).send().await?.text().await?;
                let r = parse_ddg_results(&html, limit);
                (html, r)
            }
            "baidu-free" => {
                let url = format!("https://www.baidu.com/s?wd={}&rn={limit}", urlencoding::encode(query));
                let html = client.get(&url)
                    .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
                    .send().await?.text().await?;
                let r = parse_baidu_results(&html, limit);
                (html, r)
            }
            "sogou-free" => {
                let url = format!("https://www.sogou.com/web?query={}", urlencoding::encode(query));
                let html = client.get(&url)
                    .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
                    .send().await?.text().await?;
                let r = parse_sogou_results(&html, limit);
                (html, r)
            }
            _ => return Ok(vec![]),
        };

        if results.is_empty() && is_captcha_page(&html) {
            warn!(provider, "web_search: CAPTCHA detected, provider may be rate-limited");
        }

        Ok(results)
    }

    pub(crate) async fn tool_web_fetch(&self, args: Value) -> Result<Value> {
        use moka::future::Cache;
        use std::sync::LazyLock;

        /// LRU cache: URL -> (title, markdown). 15 min TTL, ~50 MB.
        static FETCH_CACHE: LazyLock<Cache<String, (String, String)>> = LazyLock::new(|| {
            Cache::builder()
                .max_capacity(500)
                .time_to_live(Duration::from_secs(15 * 60))
                .build()
        });

        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow!("web_fetch: `url` required"))?;
        let prompt = args.get("prompt").and_then(|v| v.as_str());

        let max_length = self.config.ext.tools.as_ref()
            .and_then(|t| t.web_fetch.as_ref())
            .and_then(|f| f.max_length)
            .unwrap_or(100_000);
        let user_agent = self.config.ext.tools.as_ref()
            .and_then(|t| t.web_fetch.as_ref())
            .and_then(|f| f.user_agent.clone())
            .unwrap_or_else(|| "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_owned());

        // Upgrade http -> https.
        let fetch_url = if url.starts_with("http://") {
            url.replacen("http://", "https://", 1)
        } else {
            url.to_owned()
        };

        // Check cache.
        if let Some((cached_title, cached_md)) = FETCH_CACHE.get(&fetch_url).await {
            let text = truncate_chars(&cached_md, max_length);
            let text = self.maybe_summarize(&text, prompt).await;
            return Ok(json!({
                "url": url,
                "title": cached_title,
                "text": text,
                "length": text.len(),
            }));
        }

        // Build HTTP client with same-host-only redirect policy.
        let original_host = reqwest::Url::parse(&fetch_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_owned()));
        let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > 10 {
                return attempt.error(anyhow!("too many redirects"));
            }
            // Allow same-host (ignoring www. prefix).
            let new_host = attempt.url().host_str().unwrap_or("");
            let strip_www = |h: &str| h.strip_prefix("www.").unwrap_or(h).to_owned();
            let orig = original_host.as_deref().map(strip_www).unwrap_or_default();
            if strip_www(new_host) == orig {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });

        let client = reqwest::Client::builder()
            .user_agent(&user_agent)
            .timeout(Duration::from_secs(30))
            .redirect(redirect_policy)
            .build()?;

        let response = client.get(&fetch_url).send().await?;

        // Cross-host redirect: report to agent, let it decide.
        if response.status().is_redirection() {
            if let Some(loc) = response.headers().get("location").and_then(|v| v.to_str().ok()) {
                return Ok(json!({
                    "url": url,
                    "redirect": loc,
                    "text": format!("Redirected to different host: {loc}. Fetch that URL if needed."),
                }));
            }
        }

        // Enforce 10 MB content-length limit.
        if let Some(len) = response.content_length() {
            if len > 10 * 1024 * 1024 {
                bail!("web_fetch: content too large ({} bytes, max 10MB)", len);
            }
        }

        let content_type = response.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let html = response.text().await?;

        let title = extract_html_title(&html);

        // Convert HTML → clean plain text via lol-html structural dehydration.
        // Removes script/style/nav/footer/aside entirely, strips all non-semantic
        // attributes, then strips remaining tags and collapses whitespace.
        // This reliably eliminates JS bundles and CSS noise without the
        // htmd Markdown conversion overhead.
        let markdown = if content_type.contains("text/html") {
            html_dehydrate_to_text(&html)
        } else {
            html.clone()
        };

        // Detect SPA (large HTML but almost no text) -> fallback to browser.
        // Use the already-computed dehydrated text length for the check.
        let plain_len = markdown.trim().len();
        let is_spa = content_type.contains("text/html") && plain_len < 200 && html.len() > 10_000;

        let (final_title, final_md) = if is_spa {
            // Try browser fallback for JS-rendered pages.
            match self.browser_get_article(&fetch_url).await {
                Ok((t, md)) if !md.is_empty() => (t, md),
                _ => (title.clone(), markdown.clone()),
            }
        } else {
            (title.clone(), markdown.clone())
        };

        // Cache the result.
        FETCH_CACHE.insert(fetch_url, (final_title.clone(), final_md.clone())).await;

        let text = truncate_chars(&final_md, max_length);
        let text = self.maybe_summarize(&text, prompt).await;

        Ok(json!({
            "url": url,
            "title": final_title,
            "text": text,
            "length": text.len(),
        }))
    }

    /// Use web_browser to fetch JS-rendered page content via get_article.
    pub(crate) async fn browser_get_article(&self, url: &str) -> Result<(String, String)> {
        let tab = crate::browser::pool::BrowserPool::global().acquire_tab().await?;
        tab.navigate(url).await?;

        // Wait for content to load, then extract article text.
        let _ = tab.wait_for_selector("article, main, .content, body", 10).await;
        let js = r#"(function(){
            var el = document.querySelector('article') || document.querySelector('main')
                || document.querySelector('.content') || document.body;
            var title = document.title || '';
            var html = el ? el.innerHTML || '' : '';
            return JSON.stringify({title: title, html: html});
        })()"#;
        let result = tab.evaluate(js).await?;
        let result_str = result.as_str().unwrap_or("{}");
        let parsed: Value = serde_json::from_str(result_str).unwrap_or_default();
        let title = parsed["title"].as_str().unwrap_or("").to_owned();
        let html = parsed["html"].as_str().unwrap_or("");
        let md = html_dehydrate_to_text(html);
        Ok((title, md))
    }

    /// Browser-based search fallback: open a search engine in the shared browser pool,
    /// extract results from the rendered page. Uses a pooled tab (not per-agent Chrome).
    pub(crate) async fn browser_search(&self, query: &str, limit: usize) -> Result<Vec<Value>> {
        let tab = crate::browser::pool::BrowserPool::global().acquire_tab().await?;

        // Try multiple search engines, auto-switch on CAPTCHA/empty results.
        let lang = self.config.raw.gateway.as_ref()
            .and_then(|g| g.language.as_deref())
            .unwrap_or("");
        let is_zh = lang.to_lowercase().starts_with("zh")
            || lang.to_lowercase().starts_with("chinese");

        // Engine list: (name, url_template, result_css, snippet_css)
        // Round-robin start index to distribute concurrent searches across engines,
        // avoiding CAPTCHA triggers from hitting the same engine simultaneously.
        let q = urlencoding::encode(query);
        let mut engines: Vec<(&str, String, &str, &str)> = if is_zh {
            vec![
                ("baidu", format!("https://www.baidu.com/s?wd={q}"), ".result.c-container", "p, .c-abstract"),
                ("sogou", format!("https://www.sogou.com/web?query={q}"), ".vrwrap, .rb", "p, .ft"),
                ("bing", format!("https://cn.bing.com/search?q={q}"), ".b_algo", "p"),
                ("google", format!("https://www.google.com/search?q={q}"), "div.g", "span.st, div[data-sncf]"),
            ]
        } else {
            vec![
                ("google", format!("https://www.google.com/search?q={q}"), "div.g", "span.st, div[data-sncf]"),
                ("bing", format!("https://www.bing.com/search?q={q}"), ".b_algo", "p"),
                ("duckduckgo", format!("https://html.duckduckgo.com/html/?q={q}"), ".result", ".result__snippet"),
            ]
        };
        let rotation = crate::browser::pool::BrowserPool::global().next_engine_index() as usize;
        let len = engines.len();
        engines.rotate_left(rotation % len);

        for (name, url, result_selector, snippet_selector) in &engines {
            info!(engine = name, "browser_search: trying");
            if let Err(e) = tab.navigate(url).await {
                warn!(engine = name, "browser_search: open failed: {e}");
                continue;
            }
            let _ = tab.wait_for_selector(result_selector, 8).await;

            // Check for CAPTCHA: look for common challenge indicators
            let captcha_js = r#"(function(){
                var t = document.body ? document.body.innerText.toLowerCase() : '';
                var hasCaptcha = t.includes('captcha') || t.includes('验证') || t.includes('robot')
                    || t.includes('unusual traffic') || t.includes('人机验证')
                    || document.querySelector('iframe[src*="captcha"]') !== null
                    || document.querySelector('#captcha, .captcha, .g-recaptcha') !== null;
                return hasCaptcha ? 'captcha' : 'ok';
            })()"#;
            if let Ok(v) = tab.evaluate(captcha_js).await {
                let status = v.as_str().unwrap_or("");
                if status == "captcha" {
                    warn!(engine = name, "browser_search: CAPTCHA detected, trying next engine");
                    continue;
                }
            }

            // Extract results
            let js = format!(r#"(function(){{
                var results = [];
                var items = document.querySelectorAll('{result_selector}');
                for (var i = 0; i < Math.min(items.length, {limit}); i++) {{
                    var a = items[i].querySelector('a');
                    var p = items[i].querySelector('{snippet_selector}');
                    if (a && a.href && !a.href.startsWith('javascript:')) {{
                        results.push({{
                            title: a.innerText || '',
                            url: a.href || '',
                            snippet: p ? p.innerText || '' : ''
                        }});
                    }}
                }}
                return JSON.stringify(results);
            }})()"#);

            if let Ok(result) = tab.evaluate(&js).await {
                let result_str = result.as_str().unwrap_or("[]");
                let parsed: Vec<Value> = serde_json::from_str(
                    if result_str.starts_with('[') { result_str } else { "[]" }
                ).unwrap_or_default();

                if !parsed.is_empty() {
                    info!(engine = name, count = parsed.len(), "browser_search: got results");
                    return Ok(parsed);
                }
            }
            warn!(engine = name, "browser_search: no results, trying next engine");
        }

        // Tab is automatically closed when dropped.
        Ok(vec![])
    }

    /// If summaryModel is configured and a prompt is provided, summarize
    /// the content with a secondary model. Otherwise return content as-is.
    pub(crate) async fn maybe_summarize(&self, content: &str, prompt: Option<&str>) -> String {
        let summary_model = self.config.ext.tools.as_ref()
            .and_then(|t| t.web_fetch.as_ref())
            .and_then(|f| f.summary_model.clone());

        let (Some(model_str), Some(prompt)) = (summary_model, prompt) else {
            return content.to_owned();
        };

        // Resolve provider/model and call directly (bypass failover for simplicity).
        let (provider_name, model_id) = self.providers.resolve_model(&model_str);

        let provider = match self.providers.get(provider_name) {
            Ok(p) => p,
            Err(e) => {
                warn!("web_fetch: provider '{provider_name}' not available: {e}");
                return content.to_owned();
            }
        };

        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text(format!(
                "Web page content:\n---\n{content}\n---\n\n{prompt}\n\n\
                 Provide a concise response based on the content above."
            )),
        }];

        let req = crate::provider::LlmRequest {
            model: model_id.to_owned(),
            messages,
            tools: vec![],
            system: None,
            max_tokens: Some(2000),
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None, kv_cache_mode: 0, session_key: None,
        };

        match provider.stream(req).await {
            Ok(mut stream) => {
                let mut buf = String::new();
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(StreamEvent::TextDelta(d)) => buf.push_str(&d),
                        Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                if buf.is_empty() { content.to_owned() } else { buf }
            }
            Err(e) => {
                warn!("web_fetch summary model failed: {e:#}");
                content.to_owned()
            }
        }
    }

    pub(crate) async fn tool_web_download(&self, args: Value) -> Result<Value> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow!("web_download: `url` required"))?;
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("web_download: `path` required"))?;

        // Resolve path: always under workspace/downloads.
        // Strip common prefixes that models hallucinate (~/Downloads/, ~/,  /workspace/).
        let mut cleaned = path_str
            .trim_start_matches("~/Downloads/")
            .trim_start_matches("~/downloads/")
            .trim_start_matches("~/")
            .trim_start_matches("/workspace/")
            .trim_start_matches("/");
        if cleaned.is_empty() {
            cleaned = "download";
        }
        let workspace = self.handle.config.workspace.as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));
        let full = workspace.join("downloads").join(cleaned);

        // Ensure parent directory exists.
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| anyhow!("web_download: cannot create directory {}: {e}", parent.display()))?;
        }

        // Build cookie header: manual cookies param > auto from browser session
        let mut cookie_header = String::new();
        if let Some(cookies) = args["cookies"].as_str() {
            cookie_header = cookies.to_owned();
        } else if args["use_browser_cookies"].as_bool().unwrap_or(false) {
            // Extract cookies from active browser session via CDP
            let mut guard = self.browser.lock().await;
            if let Some(ref mut session) = *guard {
                match session.execute("cookies", &json!({})).await {
                    Ok(resp) => {
                        if let Some(cookies) = resp["cookies"].as_array() {
                            let url_parsed = reqwest::Url::parse(url).ok();
                            let domain = url_parsed.as_ref().and_then(|u| u.host_str());
                            let parts: Vec<String> = cookies.iter()
                                .filter(|c| {
                                    // Filter cookies matching the download URL domain
                                    if let (Some(d), Some(cd)) = (domain, c["domain"].as_str()) {
                                        let cd = cd.trim_start_matches('.');
                                        d == cd || d.ends_with(&format!(".{cd}"))
                                    } else {
                                        true
                                    }
                                })
                                .filter_map(|c| {
                                    let name = c["name"].as_str()?;
                                    let value = c["value"].as_str()?;
                                    Some(format!("{name}={value}"))
                                })
                                .collect();
                            cookie_header = parts.join("; ");
                            tracing::debug!(cookies_count = parts.len(), "web_download: extracted browser cookies");
                        }
                    }
                    Err(e) => {
                        tracing::warn!("web_download: failed to get browser cookies: {e}");
                    }
                }
            }
        }

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .timeout(Duration::from_secs(300))
            .build()?;

        // Resume support: if file exists, try Range request to continue download.
        let existing_size = tokio::fs::metadata(&full).await.map(|m| m.len()).unwrap_or(0);
        let mut req = client.get(url);
        if !cookie_header.is_empty() {
            req = req.header("Cookie", &cookie_header);
        }
        // Set Referer — use custom referer if provided, otherwise derive from URL.
        // Jimeng CDN (byteimg.com) requires Referer from jimeng.jianying.com.
        if let Some(referer) = args["referer"].as_str() {
            req = req.header("Referer", referer);
        } else if let Ok(parsed) = reqwest::Url::parse(url) {
            if let Some(host) = parsed.host_str() {
                // For known CDN domains, use their parent service as referer
                let referer = if host.contains("byteimg.com") || host.contains("dreamina") {
                    "https://jimeng.jianying.com/".to_string()
                } else {
                    format!("{}://{}/", parsed.scheme(), host)
                };
                req = req.header("Referer", referer);
            }
        }
        if existing_size > 0 {
            req = req.header("Range", format!("bytes={existing_size}-"));
        }

        let resp = req.send().await
            .map_err(|e| anyhow!("web_download: request failed: {e}"))?;

        if !resp.status().is_success() && resp.status().as_u16() != 206 {
            bail!("web_download: HTTP {} for {url}", resp.status());
        }

        // Warn if response is HTML (likely a redirect/login page, not the actual file).
        let content_type = resp.headers().get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        if content_type.contains("text/html") {
            bail!("web_download: server returned HTML instead of file. The URL may require different cookies or is a redirect page. Content-Type: {content_type}");
        }

        let resumed = resp.status().as_u16() == 206;

        // Stream to file (low memory). Append if resuming, create otherwise.
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut file = if resumed {
            tokio::fs::OpenOptions::new().append(true).open(&full).await
                .map_err(|e| anyhow!("web_download: cannot open for append {}: {e}", full.display()))?
        } else {
            tokio::fs::File::create(&full).await
                .map_err(|e| anyhow!("web_download: cannot create {}: {e}", full.display()))?
        };
        let mut downloaded: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow!("web_download: stream error: {e}"))?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
        }
        file.flush().await?;

        let total = existing_size + downloaded;
        Ok(json!({
            "status": "ok",
            "path": full.to_string_lossy(),
            "size_bytes": total,
            "resumed": resumed,
        }))
    }

    pub(crate) async fn tool_web_browser(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("web_browser: `action` required"))?;

        // Get or init browser session. On each call we check if the existing
        // session has been idle for too long -- if so, drop it (ChromeProcess::Drop
        // kills the Chrome process) and reinitialize.
        {
            let mut guard = self.browser.lock().await;

            // Check if existing session is idle-expired; if so, drop it.
            if let Some(ref session) = *guard {
                if session.is_idle_expired() {
                    info!("Chrome idle timeout expired, closing session");
                    *guard = None;
                }
            }

            // Determine headed mode: per-request `headed` param overrides config.
            // Task agents (non-main) always use headless to save resources.
            let wb_cfg = self.config.ext.tools.as_ref()
                .and_then(|t| t.web_browser.as_ref());
            let is_main = self.handle.id == "main";
            let config_headed = if is_main {
                wb_cfg.and_then(|b| b.headed).unwrap_or_else(has_display)
            } else {
                false // task agents always headless
            };
            let request_headed = args.get("headed").and_then(|v| v.as_bool());
            let headed = if is_main {
                request_headed.unwrap_or(config_headed)
            } else {
                false // task agents cannot override to headed
            };
            let profile = wb_cfg.and_then(|b| b.profile.clone());

            // If headed mode changed, restart the session.
            if let Some(ref session) = *guard {
                if request_headed.is_some() && session.headed != headed {
                    info!(headed, "browser headed mode changed, restarting session");
                    *guard = None;
                }
            }

            // If no session, initialize one.
            if guard.is_none() {
                // Check Chrome availability
                let chrome_path = match wb_cfg
                    .and_then(|b| b.chrome_path.clone())
                    .or_else(|| detect_chrome())
                {
                    Some(p) => p,
                    None => {
                        let lang = crate::i18n::default_lang();
                        let msg = crate::i18n::t_fmt("tool_missing", lang, &[("tool", "chromium")]);
                        warn!("{}", msg);
                        if let Some(ref tx) = self.notification_tx {
                            let _ = tx.send(crate::channel::OutboundMessage {
                                target_id: ctx.peer_id.clone(),
                                is_group: false,
                                text: msg.clone(),
                                reply_to: None,
                                images: vec![],
                                files: vec![],
                                channel: Some(ctx.channel.clone()),
                            });
                        }
                        return Err(anyhow!(msg));
                    }
                };

                let bs = if headed {
                    // Try connecting to user's existing Chrome first.
                    let default_ports: Vec<u16> = vec![9222, 9223];
                    let ports = wb_cfg
                        .and_then(|b| b.remote_debug_ports.as_ref())
                        .unwrap_or(&default_ports);
                    if let Some(ws_url) = crate::browser::detect_existing_chrome(ports).await {
                        info!("connecting to user Chrome (remote debugging)");
                        crate::browser::BrowserSession::connect_existing(&ws_url).await?
                    } else {
                        // No existing Chrome found, launch with visible window
                        // using RsClaw's own profile (not user's default — that
                        // would conflict with any running Chrome).
                        crate::browser::can_launch_chrome()?;
                        crate::browser::BrowserSession::start(&chrome_path, true, profile.as_deref()).await?
                    }
                } else {
                    // Headless mode.
                    crate::browser::can_launch_chrome()?;
                    crate::browser::BrowserSession::start(&chrome_path, false, profile.as_deref()).await?
                };
                *guard = Some(bs);
            }
        }

        // capture_video is now handled by the browser module directly.
        // Falls through to the browser execute() call below.

        // Now lock again for execute -- guard is dropped, avoiding borrow issues.
        let mut browser = self.browser.lock().await;
        browser.as_mut().unwrap().execute(action, &args).await
    }
}
