//! Skill registry abstraction.
//!
//! All registries — clawhub.ai, skillhub (Tencent), skills.sh — implement the
//! same `Registry` enum so search and install logic is uniform. Callers pick
//! which registries to activate; the concurrent merge is always the same.

use reqwest::Client;
use tracing::debug;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single skill search result from any registry.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub slug: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub downloads: Option<u64>,
    pub installs: Option<u64>,
    pub stars: Option<u64>,
    /// Which registry returned this result.
    pub registry: String,
}

// ---------------------------------------------------------------------------
// Registry enum
// ---------------------------------------------------------------------------

/// A single skill registry that can be searched.
///
/// All variants share the same `search()` method so concurrent search and
/// result merging work uniformly regardless of the backend.
pub enum Registry {
    /// clawhub.ai — default for non-CN locales.
    Clawhub {
        client: Client,
        api_base: String,
        token: Option<String>,
    },
    /// skillhub (Tencent COS + lightmake.site) — preferred for CN locales.
    Skillhub {
        client: Client,
        search_url: String,
        index_url: String,
    },
    /// skills.sh community directory — always searched, 91K+ skills ranked by installs.
    Skillsh {
        client: Client,
    },
}

impl Registry {
    /// Human-readable registry name for display.
    pub fn name(&self) -> &str {
        match self {
            Registry::Clawhub { .. } => "clawhub.ai",
            Registry::Skillhub { .. } => "skillhub",
            Registry::Skillsh { .. } => "skills.sh",
        }
    }

    /// Search this registry for skills matching `query`.
    pub async fn search(&self, query: &str) -> Vec<SearchResult> {
        match self {
            Registry::Clawhub { client, api_base, token } => {
                search_clawhub(client, api_base, token.as_deref(), query).await
            }
            Registry::Skillhub { client, search_url, index_url } => {
                search_skillhub(client, search_url, index_url, query).await
            }
            Registry::Skillsh { client } => {
                search_skillsh(client, query).await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Concurrent multi-registry search
// ---------------------------------------------------------------------------

/// Search all `registries` concurrently, merge results, and sort by installs.
///
/// Deduplication uses the normalized slug (e.g. `"owner/repo@skill"` → `"skill"`).
/// When the same skill appears in multiple registries the variant with the higher
/// install count wins; missing fields are filled in from the other entry.
pub async fn search_concurrent(registries: &[Registry], query: &str) -> Vec<SearchResult> {
    // Fire all searches in parallel.
    let futures: Vec<_> = registries.iter().map(|r| r.search(query)).collect();
    let all_results: Vec<Vec<SearchResult>> = futures::future::join_all(futures).await;

    debug!(
        registries = registries.iter().map(|r| r.name()).collect::<Vec<_>>().join(", "),
        counts = all_results.iter().map(|v| v.len().to_string()).collect::<Vec<_>>().join("+"),
        "concurrent search complete"
    );

    // Merge and dedup.
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut merged: Vec<SearchResult> = Vec::new();

    for result in all_results.into_iter().flatten() {
        let key = normalize_slug(&result.slug);
        if let Some(&idx) = seen.get(&key) {
            let existing = &mut merged[idx];
            if result.installs.unwrap_or(0) > existing.installs.unwrap_or(0) {
                existing.installs = result.installs;
            }
            if existing.description.is_none() {
                existing.description = result.description;
            }
            if existing.version.is_none() {
                existing.version = result.version;
            }
        } else {
            seen.insert(key, merged.len());
            merged.push(result);
        }
    }

    // Sort by installs descending; unranked results go to the bottom.
    merged.sort_by(|a, b| b.installs.unwrap_or(0).cmp(&a.installs.unwrap_or(0)));
    merged
}

// ---------------------------------------------------------------------------
// Per-registry search implementations
// ---------------------------------------------------------------------------

async fn search_clawhub(
    client: &Client,
    api_base: &str,
    token: Option<&str>,
    query: &str,
) -> Vec<SearchResult> {
    let url = format!("{}/v1/search?q={}", api_base, url_encode(query));
    let mut req = client.get(&url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let Ok(resp) = req.send().await else { return vec![] };
    if !resp.status().is_success() { return vec![]; }
    let Ok(body) = resp.json::<serde_json::Value>().await else { return vec![] };
    parse_standard_response(&body, "clawhub.ai")
}

async fn search_skillhub(
    client: &Client,
    search_url: &str,
    index_url: &str,
    query: &str,
) -> Vec<SearchResult> {
    // Try search API first.
    let url = format!("{}?q={}", search_url, url_encode(query));
    if let Ok(resp) = client.get(&url).send().await {
        if resp.status().is_success() {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let results = parse_standard_response(&body, "skillhub");
                if !results.is_empty() {
                    return results;
                }
            }
        }
    }

    // Fallback: full index with client-side keyword filter.
    if let Ok(resp) = client.get(index_url).send().await {
        if resp.status().is_success() {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let q = query.to_lowercase();
                return body
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter(|item| {
                                let slug = item["slug"].as_str().or_else(|| item["name"].as_str()).unwrap_or("");
                                let desc = item["summary"].as_str().or_else(|| item["description"].as_str()).unwrap_or("");
                                slug.to_lowercase().contains(&q) || desc.to_lowercase().contains(&q)
                            })
                            .take(10)
                            .map(|item| to_result(item, "skillhub"))
                            .collect()
                    })
                    .unwrap_or_default();
            }
        }
    }

    vec![]
}

async fn search_skillsh(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!("https://skills.sh/api/search?q={}&limit=20", url_encode(query));
    let Ok(resp) = client.get(&url).send().await else { return vec![] };
    if !resp.status().is_success() { return vec![]; }
    let Ok(body) = resp.json::<serde_json::Value>().await else { return vec![] };

    body.get("skills")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    // skills.sh: {id, skillId, name, installs, source: "owner/repo"}
                    let source = item["source"].as_str().unwrap_or("");
                    let skill_id = item["skillId"].as_str()
                        .or_else(|| item["name"].as_str())
                        .unwrap_or("unknown");
                    let slug = if source.is_empty() {
                        skill_id.to_owned()
                    } else {
                        format!("{source}@{skill_id}")
                    };
                    SearchResult {
                        slug,
                        version: None,
                        description: item["description"].as_str()
                            .or_else(|| item["summary"].as_str())
                            .map(|s| s.to_owned()),
                        downloads: None,
                        installs: item["installs"].as_u64(),
                        stars: item["stars"].as_u64(),
                        registry: "skills.sh".to_owned(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_standard_response(body: &serde_json::Value, registry: &str) -> Vec<SearchResult> {
    body.get("skills")
        .or_else(|| body.get("results"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|item| to_result(item, registry)).collect())
        .unwrap_or_default()
}

fn to_result(item: &serde_json::Value, registry: &str) -> SearchResult {
    SearchResult {
        slug: item["slug"].as_str()
            .or_else(|| item["name"].as_str())
            .unwrap_or("unknown")
            .to_owned(),
        version: item["version"].as_str().map(|s| s.to_owned()),
        description: item["summary"].as_str()
            .or_else(|| item["description"].as_str())
            .map(|s| s.to_owned()),
        downloads: item["downloads"].as_u64()
            .or_else(|| item["download_count"].as_u64()),
        installs: item["installs"].as_u64()
            .or_else(|| item["install_count"].as_u64()),
        stars: item["stars"].as_u64()
            .or_else(|| item["favorites"].as_u64())
            .or_else(|| item["star_count"].as_u64()),
        registry: registry.to_owned(),
    }
}

/// Normalize slug to a short name for deduplication.
///
/// `"owner/repo@skill"` → `"skill"`, `"owner/repo"` → `"repo"`, `"skill"` → `"skill"`
pub fn normalize_slug(slug: &str) -> String {
    if let Some((_, after)) = slug.rsplit_once('@') {
        return after.to_lowercase();
    }
    slug.rsplit('/').next().unwrap_or(slug).to_lowercase()
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[(byte & 0xf) as usize]));
            }
        }
    }
    out
}
