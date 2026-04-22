//! Web search result parsers — HTML scraping for DuckDuckGo, Bing, Baidu, Sogou.
//!
//! Extracted from `runtime.rs` to reduce file size.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

// Pre-compiled regexes for HTML parsing — avoids recompilation on every call.

static DDG_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+class="result__a"[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#)
        .expect("ddg link regex")
});
static DDG_SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+class="result__snippet"[^>]*>(.*?)</a>"#)
        .expect("ddg snippet regex")
});
static BING_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a[^>]*href="(https?://[^"]*)"[^>]*>(.*?)</a>"#)
        .expect("bing link regex")
});
static BING_SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<p[^>]*>(.*?)</p>"#).expect("bing snippet regex")
});
static BAIDU_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<h3[^>]*>\s*<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#)
        .expect("baidu link regex")
});
static BAIDU_SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<span[^>]*class="content-right[^"]*"[^>]*>(.*?)</span>"#)
        .expect("baidu snippet regex")
});
static SOGOU_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<h3[^>]*>\s*<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#)
        .expect("sogou link regex")
});
static STRIP_TAGS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<[^>]+>").expect("strip tags regex")
});
static TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("title regex")
});
static SCRIPT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<script[^>]*>.*?</script>").expect("script regex")
});
static STYLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<style[^>]*>.*?</style>").expect("style regex")
});
static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s+").expect("whitespace regex")
});

/// Look up search engine URL template from defaults.toml.
pub(crate) fn search_engine_url(name: &str) -> &'static str {
    static URLS: std::sync::LazyLock<std::collections::HashMap<String, String>> =
        std::sync::LazyLock::new(|| {
            #[derive(serde::Deserialize)]
            struct Entry {
                name: String,
                url: String,
            }
            #[derive(serde::Deserialize)]
            struct Defs {
                #[serde(default)]
                search_engines: Vec<Entry>,
            }
            let defaults_str = crate::config::loader::load_defaults_toml();
            let defs: Defs = toml::from_str(&defaults_str).unwrap_or(Defs {
                search_engines: vec![],
            });
            defs.search_engines
                .into_iter()
                .map(|e| (e.name, e.url))
                .collect()
        });
    URLS.get(name).map(|s| s.as_str()).unwrap_or("")
}

/// URL-encode a string for use in query parameters.
pub(crate) mod urlencoding {
    /// Percent-encode a string (RFC 3986 unreserved characters pass through).
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 3);
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char);
                }
                _ => {
                    out.push('%');
                    out.push_str(&format!("{byte:02X}"));
                }
            }
        }
        out
    }
}

/// Map language code to Bing market string.
pub(crate) fn lang_to_bing_mkt(lang: &str) -> &'static str {
    match lang.to_lowercase().as_str() {
        "chinese" | "zh" => "zh-CN",
        "english" | "en" => "en-US",
        "japanese" | "ja" => "ja-JP",
        "korean" | "ko" => "ko-KR",
        "thai" | "th" => "th-TH",
        "vietnamese" | "vi" => "vi-VN",
        "indonesian" | "id" | "bahasa" => "id-ID",
        "malay" | "ms" => "ms-MY",
        "tagalog" | "tl" | "filipino" => "en-PH",
        "burmese" | "my" => "en-US", // Bing has no Burmese market
        "khmer" | "km" => "en-US",   // no Khmer market
        "lao" | "lo" => "en-US",     // no Lao market
        "spanish" | "es" => "es-ES",
        "french" | "fr" => "fr-FR",
        "german" | "de" => "de-DE",
        "portuguese" | "pt" => "pt-BR",
        "russian" | "ru" => "ru-RU",
        "arabic" | "ar" => "ar-SA",
        "hindi" | "hi" => "hi-IN",
        _ => "",
    }
}

/// Parse DuckDuckGo HTML search results into structured results.
pub(crate) fn parse_ddg_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();

    let link_caps: Vec<_> = DDG_LINK_RE.captures_iter(html).collect();
    let snippet_caps: Vec<_> = DDG_SNIPPET_RE.captures_iter(html).collect();

    for (i, cap) in link_caps.iter().enumerate().take(limit) {
        let raw_url = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let snippet = snippet_caps
            .get(i)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");

        // DDG wraps URLs through a redirect; extract the actual URL.
        let url = if let Some(pos) = raw_url.find("uddg=") {
            let start = pos + 5;
            let end = raw_url[start..]
                .find('&')
                .map(|e| start + e)
                .unwrap_or(raw_url.len());
            percent_decode(&raw_url[start..end])
        } else {
            raw_url.to_owned()
        };

        results.push(json!({
            "title": strip_inline_tags(title),
            "url": url,
            "snippet": strip_inline_tags(snippet)
        }));
    }

    results
}

/// Parse Bing HTML search results.
pub(crate) fn parse_bing_html_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();
    let parts: Vec<&str> = html.split("class=\"b_algo\"").collect();
    for block in parts.iter().skip(1).take(limit) {
        let (url, title) = BING_LINK_RE
            .captures(block)
            .map(|c| {
                (
                    c.get(1).map(|m| m.as_str()).unwrap_or(""),
                    c.get(2).map(|m| m.as_str()).unwrap_or(""),
                )
            })
            .unwrap_or(("", ""));
        let snippet = BING_SNIPPET_RE
            .captures(block)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        if !url.is_empty() {
            results.push(json!({
                "title": strip_inline_tags(title),
                "url": url,
                "snippet": strip_inline_tags(snippet)
            }));
        }
    }
    results
}

/// Parse Baidu HTML search results.
pub(crate) fn parse_baidu_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();

    let links: Vec<_> = BAIDU_LINK_RE.captures_iter(html).collect();
    let snippets: Vec<_> = BAIDU_SNIPPET_RE.captures_iter(html).collect();

    for (i, cap) in links.iter().enumerate().take(limit) {
        let url = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let snippet = snippets
            .get(i)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        if !url.is_empty() {
            results.push(json!({
                "title": strip_inline_tags(title),
                "url": url,
                "snippet": strip_inline_tags(snippet)
            }));
        }
    }
    results
}

/// Parse Sogou HTML search results.
pub(crate) fn parse_sogou_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();
    for cap in SOGOU_LINK_RE.captures_iter(html).take(limit) {
        let url = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        if !url.is_empty() {
            results.push(json!({
                "title": strip_inline_tags(title),
                "url": url,
                "snippet": ""
            }));
        }
    }
    results
}

/// Detect if HTML response is a CAPTCHA/verification page.
pub(crate) fn is_captcha_page(html: &str) -> bool {
    let lower = html.to_lowercase();
    lower.contains("captcha") || lower.contains("验证码")
        || lower.contains("人机验证") || lower.contains("verify you are human")
        || lower.contains("robot") || lower.contains("unusual traffic")
        || lower.contains("are you a robot") || lower.contains("security check")
        || lower.contains("challenge-form") || lower.contains("cf-browser-verification")
        || lower.contains("antibot") || lower.contains("recaptcha")
        || lower.contains("hcaptcha") || lower.contains("turnstile")
}

/// Simple percent-decoding for URL extraction.
pub(crate) fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip inline HTML tags (bold, italic, etc.) from a snippet.
pub(crate) fn strip_inline_tags(s: &str) -> String {
    let text = STRIP_TAGS_RE.replace_all(s, "");
    decode_html_entities(&text)
}

/// Truncate a string to at most `max` characters (UTF-8 safe).
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push_str("\n...(truncated)");
        t
    }
}

/// Extract `<title>` content from HTML.
pub(crate) fn extract_html_title(html: &str) -> String {
    TITLE_RE.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| decode_html_entities(m.as_str().trim()))
        .unwrap_or_default()
}

/// Strip all HTML to plain text.
pub(crate) fn strip_html(html: &str) -> String {
    // Remove script and style blocks.
    let no_script = SCRIPT_RE.replace_all(html, "");
    let no_style = STYLE_RE.replace_all(&no_script, "");
    // Remove all remaining tags.
    let no_tags = STRIP_TAGS_RE.replace_all(&no_style, " ");
    // Decode entities.
    let decoded = decode_html_entities(&no_tags);
    // Collapse whitespace.
    WHITESPACE_RE
        .replace_all(&decoded, " ")
        .trim()
        .to_owned()
}

/// Structural HTML dehydration via lol-html (Cloudflare streaming rewriter).
///
/// Removes noise elements (script, style, nav, footer, etc.) as entire
/// subtrees, strips all attributes except `href` on anchors and `src`/`alt`
/// on images, then collapses whitespace.  Produces clean prose suitable for
/// token-efficient LLM input.
///
/// Falls back to [`strip_html`] on parse errors.
pub(crate) fn html_dehydrate(html: &str) -> String {
    use lol_html::{element, rewrite_str, RewriteStrSettings};

    let result = rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: vec![
                // Remove entire noise subtrees.
                element!(
                    "script, style, nav, footer, header, aside, \
                     iframe, svg, canvas, noscript, form, button, \
                     [class*=\"ad\"], [id*=\"banner\"]",
                    |el| {
                        el.remove();
                        Ok(())
                    }
                ),
                // Strip all attributes, keeping only semantically useful ones.
                element!("*", |el| {
                    let tag = el.tag_name();
                    let attrs: Vec<String> =
                        el.attributes().iter().map(|a| a.name()).collect();
                    for attr in attrs {
                        let keep = match tag.as_str() {
                            "a" => attr == "href",
                            "img" => attr == "src" || attr == "alt",
                            _ => false,
                        };
                        if !keep {
                            el.remove_attribute(&attr);
                        }
                    }
                    Ok(())
                }),
            ],
            ..RewriteStrSettings::default()
        },
    );

    static HTML_COMMENT_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("html comment regex"));

    match result {
        // Return clean HTML — no entity decoding here so downstream callers
        // (e.g. htmd) can parse the HTML structure correctly.
        // Only remove comments to avoid leaking conditional blocks.
        Ok(cleaned) => HTML_COMMENT_RE.replace_all(&cleaned, "").to_string(),
        Err(_) => strip_html(html),
    }
}

/// Like `html_dehydrate` but returns plain text for direct LLM consumption.
///
/// Applies `html_dehydrate` then strips remaining tags, decodes entities,
/// and collapses whitespace.
pub(crate) fn html_dehydrate_to_text(html: &str) -> String {
    let clean_html = html_dehydrate(html);
    let no_tags = STRIP_TAGS_RE.replace_all(&clean_html, " ");
    let decoded = decode_html_entities(&no_tags);
    WHITESPACE_RE.replace_all(&decoded, " ").trim().to_owned()
}

/// Decode common HTML entities.
pub(crate) fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}
