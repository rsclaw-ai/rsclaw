//! Web search result parsers — HTML scraping for DuckDuckGo, Bing, Baidu, Sogou.
//!
//! Extracted from `runtime.rs` to reduce file size.

use serde_json::{Value, json};

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

    // Match result links: <a class="result__a" href="...">title</a>
    let link_re =
        regex::Regex::new(r#"<a\s+class="result__a"[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    // Match snippets: <a class="result__snippet"...>snippet</a>
    let snippet_re = regex::Regex::new(r#"<a\s+class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();

    let link_caps: Vec<_> = link_re.captures_iter(html).collect();
    let snippet_caps: Vec<_> = snippet_re.captures_iter(html).collect();

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
    let link_re = regex::Regex::new(r#"<a[^>]*href="(https?://[^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    let snippet_re = regex::Regex::new(r#"<p[^>]*>(.*?)</p>"#).unwrap();

    for block in parts.iter().skip(1).take(limit) {
        let (url, title) = link_re
            .captures(block)
            .map(|c| {
                (
                    c.get(1).map(|m| m.as_str()).unwrap_or(""),
                    c.get(2).map(|m| m.as_str()).unwrap_or(""),
                )
            })
            .unwrap_or(("", ""));
        let snippet = snippet_re
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
    let link_re = regex::Regex::new(r#"<h3[^>]*>\s*<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    let snippet_re =
        regex::Regex::new(r#"<span[^>]*class="content-right[^"]*"[^>]*>(.*?)</span>"#).unwrap();

    let links: Vec<_> = link_re.captures_iter(html).collect();
    let snippets: Vec<_> = snippet_re.captures_iter(html).collect();

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
    let link_re = regex::Regex::new(r#"<h3[^>]*>\s*<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    for cap in link_re.captures_iter(html).take(limit) {
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
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re.replace_all(s, "");
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
    let re = regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| decode_html_entities(m.as_str().trim()))
        .unwrap_or_default()
}

/// Strip all HTML to plain text.
pub(crate) fn strip_html(html: &str) -> String {
    // Remove script and style blocks.
    let no_script = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>")
        .unwrap()
        .replace_all(html, "");
    let no_style = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>")
        .unwrap()
        .replace_all(&no_script, "");
    // Remove all remaining tags.
    let no_tags = regex::Regex::new(r"<[^>]+>")
        .unwrap()
        .replace_all(&no_style, " ");
    // Decode entities.
    let decoded = decode_html_entities(&no_tags);
    // Collapse whitespace.
    regex::Regex::new(r"\s+")
        .unwrap()
        .replace_all(&decoded, " ")
        .trim()
        .to_owned()
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
