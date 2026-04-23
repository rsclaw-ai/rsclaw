//! Query planner — takes a raw user query, asks the **flash** (cheap/fast)
//! model to segment it into intents + entities, and returns a structured
//! `QueryPlan`. Callers (currently `tool_web_search`) dispatch sub-queries
//! per intent: weather → wttr.in, general → real web_search, etc.
//!
//! Fail-open: if the flash model errors or returns non-JSON, we fall back
//! to a single `Intent::General` plan containing the original query. The
//! caller's existing path continues to work — no regression.
//!
//! The planner always uses the "flash" model. See
//! `AgentRuntime::resolve_flash_model_name` for resolution order (per-agent
//! override → defaults.flash_model → main model).
//!
//! Typical flash model call is <200 ms and <500 input tokens.
//!
//! Example:
//!   Query: "曼谷、广州、武汉未来7天的天气"
//!   Plan : {
//!     sub_queries: [
//!       { q:"Bangkok weather",  intent:Weather { location:"Bangkok"  } },
//!       { q:"Guangzhou weather", intent:Weather { location:"Guangzhou" } },
//!       { q:"Wuhan weather",    intent:Weather { location:"Wuhan"    } },
//!     ]
//!   }

use anyhow::{Result, anyhow};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::provider::{LlmRequest, Message, MessageContent, Role};
use crate::provider::registry::ProviderRegistry;

/// A single planned sub-query with its recognized intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubQuery {
    /// Rewritten, keyword-optimized query string (use this for search, not the original).
    pub q: String,
    /// Detected intent — caller dispatches accordingly.
    pub intent: Intent,
}

/// Recognized intent categories. Add new variants here and the planner prompt
/// when extending coverage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Intent {
    /// Weather lookup — route to wttr.in or similar.
    Weather { location: String },
    /// Currency/FX rate — route to exchangerate.host.
    Currency { from: String, to: String },
    /// Timezone clock — route to worldtimeapi.
    Timezone { location: String },
    /// Wikipedia page summary.
    Wikipedia { topic: String },
    /// GitHub repo info.
    GithubRepo { owner: String, repo: String },
    /// Flight search — browser pool → ctrip/google flights.
    /// `trip` is "oneway" or "roundtrip". Default "oneway" if user doesn't specify.
    Flight { from: String, to: String, date: String, trip: String },
    /// Train/rail search — browser pool → 12306/ctrip.
    Train { from: String, to: String, date: String },
    /// Hotel search — browser pool → ctrip/meituan.
    Hotel { city: String, checkin: String },
    /// Movie listings or info — browser pool → maoyan/douban.
    Movie { query: String },
    /// Concert/show tickets — browser pool → damai.
    Concert { query: String },
    /// Restaurant/food search — browser pool → dianping.
    Restaurant { query: String, city: String },
    /// Shopping/price comparison — browser pool → jd/smzdm.
    Shopping { query: String },
    /// Stock/fund quote — browser pool → eastmoney.
    Stock { query: String },
    /// Express/package tracking — browser pool → kuaidi100.
    Express { number: String },
    /// News headlines — browser pool → toutiao/baidu news.
    News { query: String },
    /// Map/route/navigation — browser pool → amap.
    Map { query: String },
    /// Translation — browser pool → fanyi.baidu.
    Translate { text: String, to: String },
    /// Crypto price — API → coingecko.
    CryptoPrice { coin: String },
    /// Everything else — caller should fall back to regular web_search.
    General,
}

/// Full plan returned by the planner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlan {
    pub sub_queries: Vec<SubQuery>,
}

impl QueryPlan {
    /// Single-query fallback. Used when planner isn't configured or errors out.
    pub fn passthrough(query: &str) -> Self {
        Self {
            sub_queries: vec![SubQuery {
                q: query.to_owned(),
                intent: Intent::General,
            }],
        }
    }
}

/// Build the planner system prompt with current time and timezone injected.
fn planner_system() -> String {
    let now = chrono::Local::now();
    let tz = now.format("%Z").to_string();
    let ts = now.format("%Y-%m-%d %H:%M %A").to_string();
    format!(
r#"Current time: {ts} ({tz})

You analyze a user search query and decide how to answer it.

Output ONLY valid JSON matching this schema (no prose, no markdown, no code fences):
{{"sub_queries":[{{"q":"<cleaned keywords>","intent":{{"kind":"<intent>", ...fields}}}}]}}

Intent kinds and required fields:
  weather      : {{"kind":"weather","location":"<city in English>"}}
  currency     : {{"kind":"currency","from":"<ISO code>","to":"<ISO code>"}}
  timezone     : {{"kind":"timezone","location":"<IANA zone ONLY, e.g. Asia/Shanghai>"}}
  wikipedia    : {{"kind":"wikipedia","topic":"<topic phrase>"}}
  github_repo  : {{"kind":"github_repo","owner":"<owner>","repo":"<name>"}}
  flight       : {{"kind":"flight","from":"<city>","to":"<city>","date":"<YYYY-MM-DD or empty>","trip":"oneway|roundtrip"}}
  train        : {{"kind":"train","from":"<city>","to":"<city>","date":"<YYYY-MM-DD or empty>"}}
  hotel        : {{"kind":"hotel","city":"<city>","checkin":"<YYYY-MM-DD or empty>"}}
  movie        : {{"kind":"movie","query":"<movie name or keyword>"}}
  concert      : {{"kind":"concert","query":"<artist or show name>"}}
  restaurant   : {{"kind":"restaurant","query":"<cuisine or keyword>","city":"<city>"}}
  shopping     : {{"kind":"shopping","query":"<product name>"}}
  stock        : {{"kind":"stock","query":"<stock name or code>"}}
  express      : {{"kind":"express","number":"<tracking number>"}}
  news         : {{"kind":"news","query":"<topic>"}}
  map          : {{"kind":"map","query":"<place or route>"}}
  translate    : {{"kind":"translate","text":"<text to translate>","to":"<target language code>"}}
  crypto_price : {{"kind":"crypto_price","coin":"<coin id, e.g. bitcoin>"}}
  general      : {{"kind":"general"}}

Rules:
- SPLIT multi-entity queries: if the query asks about N cities/entities,
  output N sub_queries.
- CLEAN keywords: drop filler words and dates. The current time is already
  known, so never include dates/years in the "q" field for live-data queries
  (weather, currency, stock, etc.).
- PREFER English city names for "weather" intent so API lookups hit.
- For date fields: use YYYY-MM-DD if user specifies a date, empty string if not.
- If unsure of intent, use "general" — don't force a wrong match.
- Max 5 sub_queries. Never output more.

Examples:

Input: "曼谷、广州、武汉未来7天的天气"
Output: {{"sub_queries":[
  {{"q":"Bangkok weather","intent":{{"kind":"weather","location":"Bangkok"}}}},
  {{"q":"Guangzhou weather","intent":{{"kind":"weather","location":"Guangzhou"}}}},
  {{"q":"Wuhan weather","intent":{{"kind":"weather","location":"Wuhan"}}}}
]}}

Input: "美元兑人民币汇率"
Output: {{"sub_queries":[
  {{"q":"USD to CNY","intent":{{"kind":"currency","from":"USD","to":"CNY"}}}}
]}}

Input: "rust async fn 用法"
Output: {{"sub_queries":[
  {{"q":"rust async fn usage","intent":{{"kind":"general"}}}}
]}}

Input: "tokio 仓库什么情况"
Output: {{"sub_queries":[
  {{"q":"tokio-rs/tokio","intent":{{"kind":"github_repo","owner":"tokio-rs","repo":"tokio"}}}}
]}}

Input: "下周三北京飞曼谷的机票"
Output: {{"sub_queries":[
  {{"q":"北京飞曼谷机票","intent":{{"kind":"flight","from":"北京","to":"曼谷","date":"","trip":"oneway"}}}}
]}}

Input: "茅台股价"
Output: {{"sub_queries":[
  {{"q":"茅台股票","intent":{{"kind":"stock","query":"茅台"}}}}
]}}

Input: "顺丰 SF1234567890 到哪了"
Output: {{"sub_queries":[
  {{"q":"SF1234567890","intent":{{"kind":"express","number":"SF1234567890"}}}}
]}}

Input: "比特币现在多少钱"
Output: {{"sub_queries":[
  {{"q":"bitcoin price","intent":{{"kind":"crypto_price","coin":"bitcoin"}}}}
]}}

Input: "附近好吃的火锅"
Output: {{"sub_queries":[
  {{"q":"火锅推荐","intent":{{"kind":"restaurant","query":"火锅","city":""}}}}
]}}

Input: "iPhone 16 多少钱"
Output: {{"sub_queries":[
  {{"q":"iPhone 16 价格","intent":{{"kind":"shopping","query":"iPhone 16"}}}}
]}}

Input: "周杰伦演唱会门票"
Output: {{"sub_queries":[
  {{"q":"周杰伦演唱会","intent":{{"kind":"concert","query":"周杰伦"}}}}
]}}

Input: "翻译 hello world 成中文"
Output: {{"sub_queries":[
  {{"q":"hello world","intent":{{"kind":"translate","text":"hello world","to":"zh"}}}}
]}}"#)
}

/// Plan a query using the flash model. Returns a structured `QueryPlan`.
///
/// Arguments:
/// - `query` — the raw user query string.
/// - `flash_model` — fully-qualified model string (e.g. `"custom/qwen-turbo"`).
/// - `providers` — provider registry used to resolve and call the model.
///
/// On any error (missing provider, non-JSON output, timeout) we fall back
/// to `QueryPlan::passthrough` so callers can't be blocked.
pub async fn plan(
    query: &str,
    flash_model: &str,
    providers: &ProviderRegistry,
) -> QueryPlan {
    // Hard timeout — planner is a cheap call, 5s is very generous. If the
    // flash provider is hung, we fall back to passthrough rather than
    // blocking the main search path.
    let fut = try_plan(query, flash_model, providers);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
    match result {
        Ok(Ok(p)) => {
            tracing::info!(
                query = %query,
                sub_count = p.sub_queries.len(),
                "query_planner: planned"
            );
            p
        }
        Ok(Err(e)) => {
            tracing::warn!(
                query = %query,
                error = %e,
                "query_planner: fallback to passthrough"
            );
            QueryPlan::passthrough(query)
        }
        Err(_) => {
            tracing::warn!(query = %query, "query_planner: 5s timeout, fallback to passthrough");
            QueryPlan::passthrough(query)
        }
    }
}

async fn try_plan(
    query: &str,
    flash_model: &str,
    providers: &ProviderRegistry,
) -> Result<QueryPlan> {
    let (provider_name, model_id) = providers.resolve_model(flash_model);
    let provider = providers
        .get(provider_name)
        .map_err(|e| anyhow!("flash provider '{provider_name}' unavailable: {e}"))?;

    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(format!(
            "Query: {query}\n\nOutput the JSON plan now."
        )),
    }];

    let req = LlmRequest {
        model: model_id.to_owned(),
        messages,
        tools: vec![],
        system: Some(planner_system()),
        max_tokens: Some(400),
        temperature: Some(0.0),
        frequency_penalty: None,
        thinking_budget: None,
        kv_cache_mode: 0,
        session_key: None,
    };

    // Stream the response and accumulate — we don't need streaming semantics
    // for a 200-token structured output but the provider trait is streaming.
    let mut stream = provider.stream(req).await?;
    let mut buf = String::new();
    while let Some(ev) = stream.next().await {
        use crate::provider::StreamEvent;
        match ev? {
            StreamEvent::TextDelta(t) => buf.push_str(&t),
            StreamEvent::ReasoningDelta(_) => {}
            StreamEvent::ToolCall { .. } => { /* planner shouldn't emit tool calls */ }
            StreamEvent::Done { .. } => break,
            StreamEvent::Error(e) => return Err(anyhow!("planner stream error: {e}")),
        }
    }

    // Strip leading/trailing whitespace and accept common wrappers the LLM
    // might add even after being told not to (```json ... ```).
    let json = extract_json_object(&buf).ok_or_else(|| {
        anyhow!("planner output has no JSON object; got: {}", truncate(&buf, 200))
    })?;

    tracing::debug!(raw = %truncate(json, 500), "query_planner: raw LLM output");

    let plan: QueryPlan = serde_json::from_str(json).map_err(|e| {
        anyhow!("planner JSON parse failed: {e}; raw: {}", truncate(json, 200))
    })?;

    if plan.sub_queries.is_empty() {
        return Err(anyhow!("planner returned empty sub_queries"));
    }
    // Cap at 5 to protect against runaway planner output.
    let mut plan = plan;
    plan.sub_queries.truncate(5);
    Ok(plan)
}

/// Find the first `{...}` balanced JSON object in the text. Handles the case
/// where the model wraps JSON in ```json ... ``` or adds leading commentary.
fn extract_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escape {
            escape = false;
            continue;
        }
        if in_str {
            match b {
                b'\\' => escape = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_owned() } else { format!("{}…", &s[..n]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_plain() {
        let out = extract_json_object(r#"{"sub_queries":[]}"#).unwrap();
        assert!(out.contains("sub_queries"));
    }

    #[test]
    fn extract_json_with_fence() {
        let s = "```json\n{\"sub_queries\":[]}\n```";
        let out = extract_json_object(s).unwrap();
        assert_eq!(out, r#"{"sub_queries":[]}"#);
    }

    #[test]
    fn extract_json_with_prose() {
        let s = "Sure, here's the plan: {\"sub_queries\":[{\"q\":\"x\",\"intent\":{\"kind\":\"general\"}}]}";
        let out = extract_json_object(s).unwrap();
        assert!(out.starts_with("{"));
        assert!(out.ends_with("}"));
    }

    #[test]
    fn extract_json_handles_nested() {
        let s = r#"{"a":{"b":1},"c":"}"}"#;
        let out = extract_json_object(s).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn passthrough_preserves_query() {
        let p = QueryPlan::passthrough("hello world");
        assert_eq!(p.sub_queries.len(), 1);
        assert_eq!(p.sub_queries[0].q, "hello world");
        assert!(matches!(p.sub_queries[0].intent, Intent::General));
    }

    #[test]
    fn parses_weather_multi() {
        let json = r#"{"sub_queries":[
          {"q":"Bangkok weather","intent":{"kind":"weather","location":"Bangkok"}},
          {"q":"Guangzhou weather","intent":{"kind":"weather","location":"Guangzhou"}}
        ]}"#;
        let p: QueryPlan = serde_json::from_str(json).unwrap();
        assert_eq!(p.sub_queries.len(), 2);
        match &p.sub_queries[0].intent {
            Intent::Weather { location } => assert_eq!(location, "Bangkok"),
            _ => panic!("expected weather"),
        }
    }

    #[test]
    fn parses_currency() {
        let json = r#"{"sub_queries":[
          {"q":"USD to CNY","intent":{"kind":"currency","from":"USD","to":"CNY"}}
        ]}"#;
        let p: QueryPlan = serde_json::from_str(json).unwrap();
        assert!(matches!(&p.sub_queries[0].intent, Intent::Currency { from, to } if from == "USD" && to == "CNY"));
    }
}
