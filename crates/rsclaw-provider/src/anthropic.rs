//! Anthropic Messages API provider.
//!
//! Implements streaming via `anthropic-version: 2023-06-01` SSE.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result};
use futures::{StreamExt, future::BoxFuture};
use reqwest::Client;
use serde_json::{Value, json};

use super::{
    ContentPart, LlmProvider, LlmRequest, LlmStream, Message, MessageContent, Role, StreamEvent,
    TokenUsage,
};

pub const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Minimum `budget_tokens` accepted by budget-style extended thinking.
const MIN_THINKING_BUDGET: u32 = 1024;
/// Bound on the time-to-response-headers. The body is NOT covered: a
/// `RequestBuilder::timeout` would span the whole streamed response and cut
/// long generations mid-stream.
const HEADERS_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-chunk read-idle bound for the SSE body. Anthropic emits `ping`
/// events while the model is working, so a gap this long means the
/// connection stalled.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    user_agent: Option<String>,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, ANTHROPIC_API_BASE)
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            user_agent: None,
        }
    }

    /// Create a provider with custom User-Agent.
    pub fn with_user_agent(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self {
            client: super::http_client_with_ua(user_agent.as_deref()),
            api_key: api_key.into(),
            base_url: base_url.into(),
            user_agent,
        }
    }
}

/// Build the Messages endpoint URL from a configured base.
///
/// The builtin / `defaults.toml` base already carries the `/v1` segment
/// (`https://api.anthropic.com/v1`) while the bare-host constant and some
/// anthropic-compatible third parties do not. Append only `/messages` when
/// the base ends with `/v1`, else `/v1/messages` — mirrors the setup probe.
fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn stream(&self, req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move {
            super::warn_unsupported_kv_cache_mode_2(self.name(), &req);
            let body = build_request_body(&req)?;
            let url = messages_url(&self.base_url);
            // Capture model + URL up-front so the failure path can
            // surface them in the error message — a bare "404 Not
            // Found" is otherwise impossible to triage.
            let model_for_log = req.model.clone();

            let send_fut = self
                .client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .header(
                    "user-agent",
                    self.user_agent
                        .as_deref()
                        .unwrap_or(super::DEFAULT_USER_AGENT),
                )
                .json(&body)
                .send();
            let resp = tokio::time::timeout(HEADERS_TIMEOUT, send_fut)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "Anthropic request timed out after {}s waiting for response headers (url={url})",
                        HEADERS_TIMEOUT.as_secs()
                    )
                })?
                .with_context(|| format!("Anthropic request failed (url={url})"))?;

            let status = resp.status();
            if !status.is_success() {
                let resp_body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    url = %url,
                    model = %model_for_log,
                    status = %status,
                    response_body = %rsclaw_util::truncate_str(&resp_body, 1000),
                    "Anthropic provider non-2xx response"
                );
                anyhow::bail!(
                    "Anthropic API error {status} at {url} (model={model_for_log}): {resp_body}"
                );
            }

            let byte_stream = tokio_stream::StreamExt::timeout(resp.bytes_stream(), STREAM_IDLE_TIMEOUT)
                .map(|r| match r {
                    Ok(Ok(bytes)) => Ok(bytes),
                    Ok(Err(e)) => Err(anyhow::anyhow!("stream read error: {e}")),
                    Err(_) => Err(anyhow::anyhow!(
                        "Anthropic stream idle for {}s (server stalled mid-generation)",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    )),
                });
            let event_stream = byte_stream
                .scan(SseState::new(req.model.clone()), |state, chunk| {
                    futures::future::ready(Some(parse_sse_chunk(chunk, state)))
                })
                .flat_map(futures::stream::iter);

            let stream: LlmStream = Box::pin(event_stream);
            Ok(stream)
        })
    }
}

// ---------------------------------------------------------------------------
// Model family rules
// ---------------------------------------------------------------------------

/// Request-shape rules that differ across Claude generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeFamily {
    /// Opus 4.7+, Sonnet 5+, Fable, Mythos: thinking is `{type:"adaptive"}`
    /// only (`budget_tokens` returns 400) and sampling parameters
    /// (`temperature` / `top_p` / `top_k`) return 400.
    AdaptiveOnly,
    /// Opus 4.6 / Sonnet 4.6: adaptive thinking recommended (`budget_tokens`
    /// deprecated); sampling allowed but not together with thinking.
    Adaptive,
    /// Older Claude: budget-style thinking (`budget_tokens` >= 1024 and
    /// strictly below `max_tokens`); sampling not allowed with thinking.
    Legacy,
    /// Not a Claude model id — an anthropic-compatible third party (Kimi,
    /// MiniMax, GLM, ...). Keep the historical request shape.
    Other,
}

fn claude_family(model: &str) -> ClaudeFamily {
    let m = model.to_ascii_lowercase().replace('.', "-");
    if m.contains("fable") || m.contains("mythos") {
        return ClaudeFamily::AdaptiveOnly;
    }
    if !m.contains("claude") {
        return ClaudeFamily::Other;
    }
    // Version follows the tier name in current ids (`claude-opus-4-7`,
    // `claude-sonnet-5`). Legacy ids put it before (`claude-3-5-sonnet-
    // 20241022`); a date segment there is not a version, so those resolve
    // to `None` and fall into Legacy.
    let version = ["opus-", "sonnet-", "haiku-"].iter().find_map(|tier| {
        let rest = &m[m.find(tier)? + tier.len()..];
        let mut segs = rest.split('-');
        let major: u32 = segs
            .next()
            .filter(|s| (1..=2).contains(&s.len()))?
            .parse()
            .ok()?;
        let minor: u32 = segs
            .next()
            .filter(|s| (1..=2).contains(&s.len()))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Some((major, minor))
    });
    match version {
        Some((major, _)) if major >= 5 => ClaudeFamily::AdaptiveOnly,
        Some((4, minor)) if minor >= 7 => ClaudeFamily::AdaptiveOnly,
        Some((4, 6)) => ClaudeFamily::Adaptive,
        _ => ClaudeFamily::Legacy,
    }
}

// ---------------------------------------------------------------------------
// Thinking-block replay cache
// ---------------------------------------------------------------------------

/// Bound on remembered assistant turns carrying thinking blocks.
const THINKING_CACHE_CAP: usize = 256;

/// Process-level cache of the thinking / redacted_thinking blocks (with
/// their `signature`) that preceded each `tool_use`, keyed by
/// `(model, tool_use_id)`.
///
/// The Messages API requires the assistant turn of an in-flight tool loop to
/// be replayed with its original thinking blocks unchanged (signature
/// included) when thinking is on. Signed blocks are persisted on the message
/// itself (`ContentPart::Reasoning { signature, redacted, .. }`, fed by
/// `StreamEvent::ReasoningBlock`); this cache is only the fallback for
/// messages without them. Keyed by model because thinking blocks are bound
/// to the model that produced them.
#[derive(Default)]
struct ThinkingCache {
    map: HashMap<(String, String), Vec<Value>>,
    order: VecDeque<(String, String)>,
}

fn thinking_cache() -> &'static Mutex<ThinkingCache> {
    static CACHE: OnceLock<Mutex<ThinkingCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ThinkingCache::default()))
}

fn remember_thinking(model: &str, tool_use_id: &str, blocks: Vec<Value>) {
    if tool_use_id.is_empty() || blocks.is_empty() {
        return;
    }
    let mut cache = match thinking_cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let key = (model.to_owned(), tool_use_id.to_owned());
    if cache.map.insert(key.clone(), blocks).is_none() {
        cache.order.push_back(key);
    }
    while cache.order.len() > THINKING_CACHE_CAP {
        if let Some(old) = cache.order.pop_front() {
            cache.map.remove(&old);
        }
    }
}

fn recall_thinking(model: &str, tool_use_id: &str) -> Option<Vec<Value>> {
    let cache = match thinking_cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    cache
        .map
        .get(&(model.to_owned(), tool_use_id.to_owned()))
        .cloned()
}

// ---------------------------------------------------------------------------
// Request body builder
// ---------------------------------------------------------------------------

fn build_request_body(req: &LlmRequest) -> Result<Value> {
    // Split system messages from conversation messages.
    let (system, mut messages) =
        split_system_messages(&req.messages, req.system.as_deref(), &req.model);
    // Anthropic rejects a tool_result without its tool_use (and vice
    // versa) and requires results to directly follow their call.
    repair_tool_pairing(&mut messages);

    let family = claude_family(&req.model);
    let mut max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let thinking_requested = req.thinking_budget.is_some_and(|b| b > 0);
    // When thinking is on, the assistant turn of the in-flight tool loop
    // must start with its original thinking block. If we cannot replay it
    // (cache miss after a restart / model switch) run this request without
    // thinking instead of taking a guaranteed 400.
    let thinking_on = thinking_requested && trailing_tool_turn_replayable(&messages);
    if thinking_requested && !thinking_on {
        tracing::debug!(
            model = %req.model,
            "anthropic: thinking blocks for the in-flight tool turn are unavailable; sending this request without thinking"
        );
    }

    let mut body = json!({
        "model":      req.model,
        "stream":     true,
        "messages":   messages,
    });

    if let Some(sys) = system {
        body["system"] = json!(sys);
    }

    // Inject prompt caching markers (system_and_3 strategy).
    inject_cache_control(&mut body);

    // Sampling: rejected outright by AdaptiveOnly models, and incompatible
    // with thinking on every Claude model. Third-party compatible endpoints
    // keep the historical behaviour.
    let sampling_allowed = match family {
        ClaudeFamily::AdaptiveOnly => false,
        ClaudeFamily::Adaptive | ClaudeFamily::Legacy => !thinking_on,
        ClaudeFamily::Other => true,
    };
    if let Some(t) = req.temperature
        && sampling_allowed
    {
        body["temperature"] = super::json_f32(t);
    }

    if !req.tools.is_empty() {
        // Anthropic enforces the same `^[a-zA-Z0-9_-]{1,64}$` pattern
        // as OpenAI on tool names, so plugin tools like `wechat.send_text`
        // need the same wire encoding. Restore happens in the agent
        // runtime after accumulating streamed name fragments.
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name":         crate::openai::sanitize_tool_name(&t.name),
                    "description":  t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        body["tools"] = json!(tools);
        // Set tool_choice explicitly. The real Anthropic API defaults this to
        // {"type":"auto"} so it's a no-op for Claude — but anthropic-COMPATIBLE
        // third parties (notably Kimi's api.kimi.com/coding endpoint) ignore the
        // `tools` list entirely unless tool_choice is present, so the model never
        // function-calls and instead tells the user to run commands themselves.
        // The OpenAI-completions path encodes the same Kimi quirk (see
        // `openai::build_request_body`); the anthropic path needs it too. Being
        // explicit is safe for genuine Claude and fixes every compatible endpoint
        // with this behavior.
        body["tool_choice"] = json!({ "type": "auto" });
    }

    // Extended thinking. Newer Claude models only accept adaptive thinking;
    // budget-style thinking needs `1024 <= budget_tokens < max_tokens`.
    if thinking_on {
        match family {
            ClaudeFamily::AdaptiveOnly | ClaudeFamily::Adaptive => {
                body["thinking"] = json!({ "type": "adaptive" });
            }
            ClaudeFamily::Legacy | ClaudeFamily::Other => {
                let budget = req
                    .thinking_budget
                    .unwrap_or(MIN_THINKING_BUDGET)
                    .max(MIN_THINKING_BUDGET);
                if budget >= max_tokens {
                    // max_tokens includes the thinking budget; keep the
                    // original visible-answer room on top of it.
                    max_tokens = budget.saturating_add(max_tokens);
                }
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                });
            }
        }
    }
    body["max_tokens"] = json!(max_tokens);

    Ok(body)
}

/// True unless the last assistant turn carries `tool_use` without leading
/// thinking blocks — the shape the API rejects when thinking is enabled.
fn trailing_tool_turn_replayable(messages: &[Value]) -> bool {
    let Some(last_assistant) = messages
        .iter()
        .rev()
        .find(|m| m["role"].as_str() == Some("assistant"))
    else {
        return true;
    };
    let Some(blocks) = last_assistant["content"].as_array() else {
        return true;
    };
    let has_tool_use = blocks
        .iter()
        .any(|b| b["type"].as_str() == Some("tool_use"));
    if !has_tool_use {
        return true;
    }
    matches!(
        blocks.first().and_then(|b| b["type"].as_str()),
        Some("thinking") | Some("redacted_thinking")
    )
}

/// Anthropic counterpart of the OpenAI path's `fix_tool_call_pairing` +
/// `reorder_tool_messages`:
///   - drops `tool_result` blocks whose `tool_use` is not in the history,
///   - drops `tool_use` blocks that never received a result,
///   - moves every result into a user message directly after its call.
///
/// Messages left empty by the pruning are removed (consecutive same-role
/// turns are merged by the API).
fn repair_tool_pairing(messages: &mut Vec<Value>) {
    let block_type = |b: &Value| b["type"].as_str().map(str::to_owned);
    let mut use_ids: HashSet<String> = HashSet::new();
    let mut result_ids: HashSet<String> = HashSet::new();
    for m in messages.iter() {
        let Some(blocks) = m["content"].as_array() else {
            continue;
        };
        for b in blocks {
            match (m["role"].as_str(), block_type(b).as_deref()) {
                (Some("assistant"), Some("tool_use")) => {
                    if let Some(id) = b["id"].as_str() {
                        use_ids.insert(id.to_owned());
                    }
                }
                (Some("user"), Some("tool_result")) => {
                    if let Some(id) = b["tool_use_id"].as_str() {
                        result_ids.insert(id.to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    let mut results: HashMap<String, Vec<Value>> = HashMap::new();
    let mut rest: Vec<Value> = Vec::with_capacity(messages.len());
    for mut m in messages.drain(..) {
        let role = m["role"].as_str().unwrap_or("").to_owned();
        if let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) {
            if role == "user" {
                let mut kept = Vec::with_capacity(blocks.len());
                for b in blocks.drain(..) {
                    if block_type(&b).as_deref() == Some("tool_result") {
                        match b["tool_use_id"].as_str() {
                            Some(id) if use_ids.contains(id) => {
                                results.entry(id.to_owned()).or_default().push(b);
                            }
                            _ => {
                                tracing::debug!("anthropic: dropping orphaned tool_result");
                            }
                        }
                    } else {
                        kept.push(b);
                    }
                }
                *blocks = kept;
            } else if role == "assistant" {
                blocks.retain(|b| {
                    block_type(b).as_deref() != Some("tool_use")
                        || b["id"].as_str().is_some_and(|id| result_ids.contains(id))
                });
                // Thinking blocks alone are not a valid turn.
                if blocks.iter().all(|b| {
                    matches!(
                        block_type(b).as_deref(),
                        Some("thinking") | Some("redacted_thinking")
                    )
                }) {
                    blocks.clear();
                }
            }
            if blocks.is_empty() {
                continue;
            }
        }
        rest.push(m);
    }

    for m in rest {
        let call_ids: Vec<String> = if m["role"].as_str() == Some("assistant") {
            m["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b["type"].as_str() == Some("tool_use"))
                        .filter_map(|b| b["id"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        messages.push(m);
        let mut result_blocks: Vec<Value> = Vec::new();
        for id in &call_ids {
            if let Some(r) = results.remove(id) {
                result_blocks.extend(r);
            }
        }
        if !result_blocks.is_empty() {
            messages.push(json!({ "role": "user", "content": result_blocks }));
        }
    }
}

fn split_system_messages<'a>(
    messages: &'a [Message],
    extra_system: Option<&'a str>,
    model: &str,
) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> =
        extra_system.map(|s| vec![s.to_owned()]).unwrap_or_default();

    let mut conv: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                if let MessageContent::Text(t) = &msg.content {
                    system_parts.push(t.clone());
                }
            }
            Role::User | Role::Assistant | Role::Tool => {
                conv.push(serialize_message(msg, model));
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, conv)
}

// TODO: Tool role maps to "user" but loses the tool_use_id, which Anthropic
// requires for tool_result blocks. This may cause issues with multi-turn
// tool-use conversations.
fn serialize_message(msg: &Message, model: &str) -> Value {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
        Role::System => "user", // fallback, shouldn't happen
    };

    // Anthropic's API rejects messages with empty `content` ("the
    // message at position N with role 'user' must not be empty").
    // Empty content here is almost always a bug somewhere upstream
    // (compaction, tool-result truncation, an aborted turn that left
    // a husk in history). Rather than 400-out the entire turn, we
    // substitute a single-char placeholder so the conversation can
    // make forward progress; the upstream bug should still be fixed,
    // but a placeholder beats a hard failure on every retry.
    const EMPTY_PLACEHOLDER: &str = "(empty turn)";
    let content = match &msg.content {
        MessageContent::Text(t) => {
            if t.trim().is_empty() {
                tracing::warn!(role, "empty text-content message; substituting placeholder");
                json!(EMPTY_PLACEHOLDER)
            } else {
                json!(t)
            }
        }
        MessageContent::Parts(parts) => {
            let is_tool_turn = msg.role == Role::Assistant
                && parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::ToolUse { .. }));
            // Replay the original thinking blocks (with signature) of an
            // assistant tool turn. Source of truth is the signed reasoning
            // persisted on the message; the process cache is the fallback
            // for turns recorded before signatures were persisted.
            let persisted_thinking = is_tool_turn
                && parts
                    .iter()
                    .any(|p| replayable_thinking_block(p, model).is_some());
            let cached_thinking = if is_tool_turn && !persisted_thinking {
                parts.iter().find_map(|p| match p {
                    ContentPart::ToolUse { id, .. } => recall_thinking(model, id),
                    _ => None,
                })
            } else {
                None
            };
            let mut serialized: Vec<Value> = Vec::with_capacity(parts.len());
            if let Some(blocks) = &cached_thinking {
                serialized.extend(blocks.iter().cloned());
            }
            for part in parts {
                if let ContentPart::Reasoning { text, .. } = part {
                    if persisted_thinking {
                        // Signed blocks replay in their stored position; the
                        // unsigned copies are dropped.
                        if let Some(block) = replayable_thinking_block(part, model) {
                            serialized.push(block);
                        }
                        continue;
                    }
                    // Thinking replaced by cached blocks, or an empty
                    // replay-only part (an empty text block is rejected).
                    if cached_thinking.is_some() || text.trim().is_empty() {
                        continue;
                    }
                }
                serialized.push(serialize_part(part));
            }
            // Reject entirely-empty parts arrays, and arrays where
            // every Text/Reasoning part is whitespace.
            let has_meaningful_content = !serialized.is_empty()
                && serialized.iter().any(|p| {
                    let t = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if t == "text" {
                        p.get("text")
                            .and_then(|v| v.as_str())
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                    } else {
                        // image / tool_use / tool_result are non-empty
                        // by construction — they always carry data.
                        true
                    }
                });
            if has_meaningful_content {
                json!(serialized)
            } else {
                tracing::warn!(
                    role,
                    parts_len = serialized.len(),
                    "all-empty parts array; substituting placeholder"
                );
                json!([{ "type": "text", "text": EMPTY_PLACEHOLDER }])
            }
        }
    };

    json!({ "role": role, "content": content })
}

/// Wire `thinking` / `redacted_thinking` block for a signed reasoning part,
/// when it can be replayed to `model`.
///
/// Signed blocks are replayed unchanged. Across genuine Claude models the
/// API drops blocks the target cannot read, so they are always sent; a block
/// from (or to) an anthropic-compatible third party is only replayed to the
/// exact model that produced it — a foreign signature is a hard 400.
fn replayable_thinking_block(part: &ContentPart, model: &str) -> Option<Value> {
    let ContentPart::Reasoning {
        text,
        signature,
        redacted,
        model: produced_by,
    } = part
    else {
        return None;
    };
    let produced_by = produced_by.as_deref()?;
    let compatible = produced_by == model
        || (claude_family(produced_by) != ClaudeFamily::Other
            && claude_family(model) != ClaudeFamily::Other);
    if !compatible {
        return None;
    }
    if let Some(data) = redacted.as_deref().filter(|d| !d.is_empty()) {
        return Some(json!({ "type": "redacted_thinking", "data": data }));
    }
    let signature = signature.as_deref().filter(|s| !s.is_empty())?;
    Some(json!({
        "type": "thinking",
        "thinking": text,
        "signature": signature,
    }))
}

fn serialize_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({ "type": "text", "text": text }),
        ContentPart::Image { url } => json!({
            "type": "image",
            "source": { "type": "url", "url": url }
        }),
        ContentPart::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id":    id,
            // Echo the wire-encoded name so Anthropic accepts the
            // history block. The runtime stores names in their
            // original (restored) form, so re-sanitize on serialize.
            "name":  crate::openai::sanitize_tool_name(name),
            "input": input,
        }),
        ContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type":        "tool_result",
            "tool_use_id": tool_use_id,
            "content":     content,
            "is_error":    is_error.unwrap_or(false),
        }),
        ContentPart::Reasoning { text, .. } => json!({
            "type": "text",
            "text": text,
        }),
    }
}

// ---------------------------------------------------------------------------
// Prompt caching — "system_and_3" strategy
// ---------------------------------------------------------------------------

/// Inject `cache_control: {"type": "ephemeral"}` markers into the request body.
///
/// Strategy (matches hermes-agent "system_and_3"):
/// - 1 breakpoint on the system prompt (stable prefix, high cache-hit rate)
/// - Up to 3 rolling breakpoints on the most recent non-system messages
///
/// The system prompt is converted from a plain string to a content-block array
/// so that the `cache_control` field can be attached to the last block.
fn inject_cache_control(body: &mut Value) {
    let cache_marker = json!({"type": "ephemeral"});

    // -- System prompt breakpoint --
    if let Some(system_val) = body.get_mut("system") {
        match system_val {
            // Plain string -> convert to content-block array with cache_control.
            Value::String(text) => {
                let block = json!([{
                    "type": "text",
                    "text": text.clone(),
                    "cache_control": cache_marker.clone(),
                }]);
                *system_val = block;
            }
            // Already an array of content blocks -> tag the last one.
            Value::Array(blocks) => {
                if let Some(last) = blocks.last_mut() {
                    last["cache_control"] = cache_marker.clone();
                }
            }
            _ => {}
        }
    }

    // -- Message breakpoints: 2+1 anchor strategy --
    //
    // Anchor 1 (stable): first user message — the task baseline.  Once set it
    // never changes even as the conversation grows or messages are trimmed from
    // the middle, so the prefix before this anchor is always a cache hit.
    //
    // Dynamic (volatile): last message only — the freshest addition.
    //
    // This replaces the old "last 3" approach which caused cache churn whenever
    // middle messages were pruned: the three-message window would shift and
    // invalidate the previously-cached prefix.
    if let Some(Value::Array(messages)) = body.get_mut("messages") {
        let len = messages.len();
        if len == 0 {
            return;
        }

        // Anchor: first user message.
        if messages[0].get("role").and_then(|r| r.as_str()) == Some("user") {
            tag_last_content_block(&mut messages[0], &cache_marker);
        }

        // Dynamic: latest message (only if it is different from the first).
        if len > 1 {
            let last_idx = len - 1;
            tag_last_content_block(&mut messages[last_idx], &cache_marker);
        }
    }
}

/// Add `cache_control` to the last content block of a message.
///
/// If the message content is a plain string, convert it to a content-block
/// array so the marker can be attached.
fn tag_last_content_block(msg: &mut Value, marker: &Value) {
    let content = match msg.get_mut("content") {
        Some(c) => c,
        None => return,
    };

    match content {
        // Plain string -> convert to [{type: "text", text: "...", cache_control: ...}]
        Value::String(text) => {
            let block = json!([{
                "type": "text",
                "text": text.clone(),
                "cache_control": marker.clone(),
            }]);
            *content = block;
        }
        // Array of content blocks -> tag the last one.
        Value::Array(blocks) => {
            if let Some(last) = blocks.last_mut() {
                last["cache_control"] = marker.clone();
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// SSE parser
// ---------------------------------------------------------------------------

/// Per-stream parser state (one per HTTP response).
// TODO: SSE buffered parsing is duplicated across openai.rs, anthropic.rs,
// gemini.rs — extract shared utility
struct SseState {
    /// Model id of the request — keys the thinking replay cache.
    model: String,
    /// Incomplete trailing line carried to the next chunk.
    line_buffer: String,
    /// Incomplete trailing UTF-8 sequence carried to the next chunk.
    utf8_pending: Vec<u8>,
    /// Thinking blocks being streamed, by content-block index:
    /// `(thinking_text, signature)`.
    open_thinking: HashMap<u64, (String, String)>,
    /// Completed thinking / redacted_thinking blocks of this message, in
    /// order — attached to the tool_use blocks that follow them.
    completed_thinking: Vec<Value>,
    /// Input-side usage from `message_start` (the final `message_delta`
    /// only reliably carries `output_tokens`).
    start_usage: Option<TokenUsage>,
}

impl SseState {
    fn new(model: String) -> Self {
        Self {
            model,
            line_buffer: String::new(),
            utf8_pending: Vec::new(),
            open_thinking: HashMap::new(),
            completed_thinking: Vec::new(),
            start_usage: None,
        }
    }
}

/// Buffered SSE parser — handles TCP chunk boundaries that split lines and
/// multi-byte characters.
fn parse_sse_chunk(chunk: Result<bytes::Bytes>, state: &mut SseState) -> Vec<Result<StreamEvent>> {
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => return vec![Err(e)],
    };

    let text = super::decode_utf8_chunk(&mut state.utf8_pending, &bytes);
    state.line_buffer.push_str(&text);

    let last_newline_pos = match state.line_buffer.rfind('\n') {
        Some(pos) => pos,
        None => return vec![],
    };

    let complete_portion = state.line_buffer[..last_newline_pos].to_owned();
    state.line_buffer.drain(..=last_newline_pos);

    let mut events = Vec::new();
    for line in complete_portion.lines() {
        if let Some(data) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
        {
            if data == "[DONE]" {
                continue;
            }
            if let Some(event) = parse_event(data, state) {
                events.push(Ok(event));
            }
        }
    }

    events
}

fn usage_from(u: &serde_json::Map<String, Value>) -> TokenUsage {
    TokenUsage {
        input: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        cache_creation: u
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read: u
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Default::default()
    }
}

fn parse_event(data: &str, state: &mut SseState) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(data).ok()?;
    let event_type = v["type"].as_str()?;

    match event_type {
        "message_start" => {
            state.start_usage = v["message"]["usage"].as_object().map(usage_from);
            None
        }
        "content_block_delta" => {
            let delta_type = v["delta"]["type"].as_str()?;
            match delta_type {
                "text_delta" => {
                    let text = v["delta"]["text"].as_str()?.to_owned();
                    Some(StreamEvent::TextDelta(text))
                }
                "thinking_delta" => {
                    let text = v["delta"]["thinking"].as_str().unwrap_or("").to_owned();
                    if let Some(index) = v["index"].as_u64()
                        && let Some(block) = state.open_thinking.get_mut(&index)
                    {
                        block.0.push_str(&text);
                    }
                    if text.is_empty() {
                        None
                    } else {
                        Some(StreamEvent::ReasoningDelta(text))
                    }
                }
                "signature_delta" => {
                    if let Some(index) = v["index"].as_u64()
                        && let Some(block) = state.open_thinking.get_mut(&index)
                    {
                        block
                            .1
                            .push_str(v["delta"]["signature"].as_str().unwrap_or(""));
                    }
                    None
                }
                "input_json_delta" => {
                    // Tool input streaming — emit as ToolCall so the agent loop
                    // accumulates partial JSON fragments (same pattern as OpenAI).
                    let partial = v["delta"]["partial_json"].as_str().unwrap_or("");
                    if partial.is_empty() {
                        None
                    } else {
                        Some(StreamEvent::ToolCall {
                            id: String::new(),
                            name: String::new(),
                            input: Value::String(partial.to_owned()),
                        })
                    }
                }
                _ => None,
            }
        }
        "content_block_start" => {
            let block = &v["content_block"];
            match block["type"].as_str() {
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or("").to_owned();
                    // Remember the thinking that led to this call so the
                    // next request can replay it unchanged.
                    remember_thinking(&state.model, &id, state.completed_thinking.clone());
                    // Tool call start — emit immediately so the agent loop knows.
                    Some(StreamEvent::ToolCall {
                        id,
                        name: block["name"].as_str().unwrap_or("").to_owned(),
                        input: serde_json::Value::Object(Default::default()),
                    })
                }
                Some("thinking") => {
                    if let Some(index) = v["index"].as_u64() {
                        state.open_thinking.insert(
                            index,
                            (
                                block["thinking"].as_str().unwrap_or("").to_owned(),
                                block["signature"].as_str().unwrap_or("").to_owned(),
                            ),
                        );
                    }
                    None
                }
                Some("redacted_thinking") => {
                    state.completed_thinking.push(block.clone());
                    let data = block["data"].as_str().unwrap_or("");
                    if data.is_empty() {
                        None
                    } else {
                        Some(StreamEvent::ReasoningBlock {
                            text: String::new(),
                            signature: None,
                            redacted: Some(data.to_owned()),
                            model: state.model.clone(),
                        })
                    }
                }
                _ => None,
            }
        }
        "content_block_stop" => {
            if let Some(index) = v["index"].as_u64()
                && let Some((thinking, signature)) = state.open_thinking.remove(&index)
            {
                state.completed_thinking.push(json!({
                    "type": "thinking",
                    "thinking": thinking,
                    "signature": signature,
                }));
                // Hand the signed block to the runtime so it is persisted on
                // the assistant message (the process cache is only a
                // fallback). Unsigned blocks cannot be replayed anyway.
                if !signature.is_empty() {
                    return Some(StreamEvent::ReasoningBlock {
                        text: thinking,
                        signature: Some(signature),
                        redacted: None,
                        model: state.model.clone(),
                    });
                }
            }
            None
        }
        "message_delta" => {
            let delta_usage = v["usage"].as_object().map(usage_from);
            // Merge: `message_start` carries the input side, the final
            // `message_delta` the output side. Prefer non-zero values.
            let usage = match (state.start_usage.clone(), delta_usage) {
                (Some(start), Some(delta)) => Some(TokenUsage {
                    input: if delta.input > 0 { delta.input } else { start.input },
                    output: delta.output.max(start.output),
                    cache_creation: if delta.cache_creation > 0 {
                        delta.cache_creation
                    } else {
                        start.cache_creation
                    },
                    cache_read: if delta.cache_read > 0 {
                        delta.cache_read
                    } else {
                        start.cache_read
                    },
                    ..Default::default()
                }),
                (start, delta) => delta.or(start),
            };
            if v["delta"]["stop_reason"].is_string() {
                Some(StreamEvent::Done { usage })
            } else {
                None
            }
        }
        "error" => {
            let msg = v["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_owned();
            Some(StreamEvent::Error(msg))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        super::{LlmRequest, Message, MessageContent, Role},
        *,
    };

    fn make_request() -> LlmRequest {
        LlmRequest {
            fallback_models: Vec::new(),
            model: "claude-3-5-sonnet-20241022".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn request_serializes_messages() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("hi".to_owned()),
                    rsclaw_hidden: None,
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("hello".to_owned()),
                    rsclaw_hidden: None,
                },
            ],
            ..make_request()
        };
        let body = build_request_body(&req).expect("build request body");
        let msgs = body["messages"].as_array().expect("messages is array");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"].as_str().expect("role is str"), "user");
        assert_eq!(msgs[1]["role"].as_str().expect("role is str"), "assistant");
    }

    #[test]
    fn system_field_present() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            system: Some("hello".to_owned()),
            ..make_request()
        };
        let body = build_request_body(&req).expect("build request body");
        // After cache_control injection, system is an array of content blocks.
        let blocks = body["system"]
            .as_array()
            .expect("system should be content-block array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"].as_str().expect("text is str"), "hello");
        assert_eq!(
            blocks[0]["cache_control"]["type"]
                .as_str()
                .expect("cache_control type is str"),
            "ephemeral"
        );
    }

    /// Regression: when tools are present, the body must carry an explicit
    /// `tool_choice: {"type":"auto"}`. Genuine Claude defaults to auto, but
    /// anthropic-compatible endpoints (Kimi's api.kimi.com/coding) ignore the
    /// tools list without it, so the model never function-calls.
    #[test]
    fn tool_choice_auto_set_when_tools_present() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            tools: vec![crate::ToolDef {
                name: "shell".to_owned(),
                description: "run a shell command".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            ..make_request()
        };
        let body = build_request_body(&req).expect("build request body");
        assert_eq!(
            body["tool_choice"]["type"].as_str(),
            Some("auto"),
            "tool_choice must be set to auto when tools are present"
        );
        assert!(body["tools"].as_array().is_some_and(|t| t.len() == 1));
    }

    /// No tools → no `tool_choice` key (avoid sending it with an empty tools
    /// list, which some endpoints reject).
    #[test]
    fn tool_choice_absent_when_no_tools() {
        let body = build_request_body(&make_request()).expect("build request body");
        assert!(
            body.get("tool_choice").is_none(),
            "tool_choice must be omitted when there are no tools"
        );
    }

    #[test]
    fn cache_control_system_and_anchors() {
        // 2+1 anchor strategy: system + first user message + last message.
        let req = LlmRequest {
            fallback_models: Vec::new(),
            system: Some("system prompt".to_owned()),
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("m1".to_owned()),
                    rsclaw_hidden: None,
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("m2".to_owned()),
                    rsclaw_hidden: None,
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Text("m3".to_owned()),
                    rsclaw_hidden: None,
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("m4".to_owned()),
                    rsclaw_hidden: None,
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Text("m5".to_owned()),
                    rsclaw_hidden: None,
                },
            ],
            ..make_request()
        };
        let body = build_request_body(&req).expect("build request body");

        // System should have cache_control.
        let sys_blocks = body["system"].as_array().expect("system content blocks");
        assert_eq!(
            sys_blocks[0]["cache_control"]["type"]
                .as_str()
                .expect("cache_control type"),
            "ephemeral"
        );

        let msgs = body["messages"].as_array().expect("messages is array");
        assert_eq!(msgs.len(), 5);

        // m1 (first user): cache_control — stable task anchor.
        let m1_content = &msgs[0]["content"];
        assert!(
            m1_content.is_array(),
            "m1 should be converted to content-block array"
        );
        assert_eq!(
            m1_content[0]["cache_control"]["type"]
                .as_str()
                .expect("m1 cache_control type"),
            "ephemeral"
        );

        // m2, m3, m4: no cache_control (middle messages are not anchored).
        for i in 1..4 {
            let content = &msgs[i]["content"];
            if content.is_array() {
                assert!(
                    content[0].get("cache_control").is_none(),
                    "message {i} should not have cache_control"
                );
            }
            // plain string content also means no cache_control — acceptable
        }

        // m5 (last): cache_control — dynamic breakpoint.
        let m5_content = &msgs[4]["content"];
        assert!(m5_content.is_array(), "m5 should be content-block array");
        assert_eq!(
            m5_content[0]["cache_control"]["type"]
                .as_str()
                .expect("m5 cache_control type"),
            "ephemeral"
        );
    }

    #[test]
    fn cache_control_fewer_than_3_messages() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("only one".to_owned()),
                rsclaw_hidden: None,
            }],
            ..make_request()
        };
        let body = build_request_body(&req).expect("build request body");
        let msgs = body["messages"].as_array().expect("messages is array");
        // Single message should still get cache_control.
        let content = &msgs[0]["content"];
        assert!(content.is_array());
        assert_eq!(
            content[0]["cache_control"]["type"]
                .as_str()
                .expect("cache_control type"),
            "ephemeral"
        );
    }

    #[test]
    fn temperature_serializes() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            temperature: Some(0.7),
            ..make_request()
        };
        let body = build_request_body(&req).expect("build request body");
        let t = body["temperature"].as_f64().expect("temperature is f64");
        assert!((t - 0.7).abs() < 1e-4);
    }

    #[test]
    fn messages_url_does_not_double_v1() {
        assert_eq!(
            messages_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url(ANTHROPIC_API_BASE),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn claude_family_detection() {
        assert_eq!(claude_family("claude-opus-5"), ClaudeFamily::AdaptiveOnly);
        assert_eq!(claude_family("claude-opus-4-7"), ClaudeFamily::AdaptiveOnly);
        assert_eq!(claude_family("claude-sonnet-5"), ClaudeFamily::AdaptiveOnly);
        assert_eq!(claude_family("claude-fable-5-1"), ClaudeFamily::AdaptiveOnly);
        assert_eq!(claude_family("claude-sonnet-4.6"), ClaudeFamily::Adaptive);
        assert_eq!(claude_family("claude-opus-4-6"), ClaudeFamily::Adaptive);
        assert_eq!(claude_family("claude-haiku-4-5"), ClaudeFamily::Legacy);
        assert_eq!(
            claude_family("claude-opus-4-20250514"),
            ClaudeFamily::Legacy
        );
        assert_eq!(
            claude_family("claude-3-5-sonnet-20241022"),
            ClaudeFamily::Legacy
        );
        assert_eq!(claude_family("kimi-k2"), ClaudeFamily::Other);
    }

    #[test]
    fn adaptive_only_model_uses_adaptive_thinking_and_no_temperature() {
        let req = LlmRequest {
            model: "claude-opus-4-7".to_owned(),
            temperature: Some(0.7),
            thinking_budget: Some(10240),
            ..Default::default()
        };
        let body = build_request_body(&req).expect("build request body");
        assert_eq!(body["thinking"]["type"].as_str(), Some("adaptive"));
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn legacy_budget_is_kept_below_max_tokens() {
        let req = LlmRequest {
            model: "claude-opus-4-20250514".to_owned(),
            max_tokens: Some(4096),
            temperature: Some(0.5),
            thinking_budget: Some(10240),
            ..Default::default()
        };
        let body = build_request_body(&req).expect("build request body");
        let budget = body["thinking"]["budget_tokens"].as_u64().expect("budget");
        let max = body["max_tokens"].as_u64().expect("max_tokens");
        assert!(budget < max, "budget {budget} must be < max_tokens {max}");
        // Sampling is not allowed together with thinking.
        assert!(body.get("temperature").is_none());
    }

    fn tool_turn_request(model: &str, tool_id: &str) -> LlmRequest {
        LlmRequest {
            model: model.to_owned(),
            thinking_budget: Some(4096),
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("run it".to_owned()),
                    rsclaw_hidden: None,
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::reasoning("plan"),
                        ContentPart::ToolUse {
                            id: tool_id.to_owned(),
                            name: "shell".to_owned(),
                            input: json!({"cmd": "ls"}),
                        },
                    ]),
                    rsclaw_hidden: None,
                },
                Message {
                    role: Role::Tool,
                    content: MessageContent::Parts(vec![ContentPart::ToolResult {
                        tool_use_id: tool_id.to_owned(),
                        content: "ok".to_owned(),
                        is_error: None,
                    }]),
                    rsclaw_hidden: None,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn thinking_dropped_when_tool_turn_has_no_replayable_thinking() {
        let req = tool_turn_request("claude-haiku-4-5", "toolu_no_cache_1");
        let body = build_request_body(&req).expect("build request body");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn cached_thinking_blocks_are_replayed_with_signature() {
        let mut state = SseState::new("claude-haiku-4-5".to_owned());
        for data in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig123"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_cached_1","name":"shell","input":{}}}"#,
        ] {
            parse_event(data, &mut state);
        }
        let req = tool_turn_request("claude-haiku-4-5", "toolu_cached_1");
        let body = build_request_body(&req).expect("build request body");
        assert_eq!(body["thinking"]["type"].as_str(), Some("enabled"));
        let assistant = &body["messages"][1]["content"];
        assert_eq!(assistant[0]["type"].as_str(), Some("thinking"));
        assert_eq!(assistant[0]["signature"].as_str(), Some("sig123"));
        assert_eq!(assistant[1]["type"].as_str(), Some("tool_use"));
    }

    fn signed_tool_turn_request(model: &str, produced_by: &str, tool_id: &str) -> LlmRequest {
        let mut req = tool_turn_request(model, tool_id);
        req.messages[1].content = MessageContent::Parts(vec![
            ContentPart::Reasoning {
                text: "plan".to_owned(),
                signature: Some("sig-persisted".to_owned()),
                redacted: None,
                model: Some(produced_by.to_owned()),
            },
            ContentPart::Reasoning {
                text: String::new(),
                signature: None,
                redacted: Some("opaque".to_owned()),
                model: Some(produced_by.to_owned()),
            },
            ContentPart::ToolUse {
                id: tool_id.to_owned(),
                name: "shell".to_owned(),
                input: json!({"cmd": "ls"}),
            },
        ]);
        req
    }

    #[test]
    fn persisted_signed_thinking_is_replayed_without_cache() {
        let req = signed_tool_turn_request("claude-opus-5", "claude-opus-5", "toolu_persisted_1");
        let body = build_request_body(&req).expect("build request body");
        assert_eq!(body["thinking"]["type"].as_str(), Some("adaptive"));
        let assistant = body["messages"][1]["content"]
            .as_array()
            .expect("assistant blocks");
        assert_eq!(assistant.len(), 3);
        assert_eq!(assistant[0]["type"].as_str(), Some("thinking"));
        assert_eq!(assistant[0]["thinking"].as_str(), Some("plan"));
        assert_eq!(assistant[0]["signature"].as_str(), Some("sig-persisted"));
        assert_eq!(assistant[1]["type"].as_str(), Some("redacted_thinking"));
        assert_eq!(assistant[1]["data"].as_str(), Some("opaque"));
        assert_eq!(assistant[2]["type"].as_str(), Some("tool_use"));
    }

    #[test]
    fn persisted_thinking_crosses_claude_models_but_not_third_parties() {
        // Claude -> Claude model switch: replay, the API drops what it can't read.
        let req = signed_tool_turn_request("claude-fable-5", "claude-opus-5", "toolu_switch_1");
        let body = build_request_body(&req).expect("build request body");
        assert_eq!(
            body["messages"][1]["content"][0]["type"].as_str(),
            Some("thinking")
        );
        // Third-party signature to Claude: never replayed.
        let req = signed_tool_turn_request("claude-haiku-4-5", "kimi-k2", "toolu_switch_2");
        let body = build_request_body(&req).expect("build request body");
        assert!(body.get("thinking").is_none());
        let assistant = body["messages"][1]["content"]
            .as_array()
            .expect("assistant blocks");
        // Falls back to the plain-text reasoning copy; the redacted part is dropped.
        assert_eq!(assistant.len(), 2);
        assert_eq!(assistant[0]["type"].as_str(), Some("text"));
        assert_eq!(assistant[1]["type"].as_str(), Some("tool_use"));
    }

    #[test]
    fn thinking_stop_and_redacted_start_emit_reasoning_blocks() {
        let mut state = SseState::new("claude-opus-5".to_owned());
        parse_event(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            &mut state,
        );
        parse_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}"#,
            &mut state,
        );
        parse_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s1"}}"#,
            &mut state,
        );
        match parse_event(r#"{"type":"content_block_stop","index":0}"#, &mut state) {
            Some(StreamEvent::ReasoningBlock {
                text,
                signature,
                redacted,
                model,
            }) => {
                assert_eq!(text, "why");
                assert_eq!(signature.as_deref(), Some("s1"));
                assert!(redacted.is_none());
                assert_eq!(model, "claude-opus-5");
            }
            other => panic!("expected ReasoningBlock, got {other:?}"),
        }
        match parse_event(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"enc"}}"#,
            &mut state,
        ) {
            Some(StreamEvent::ReasoningBlock { redacted, .. }) => {
                assert_eq!(redacted.as_deref(), Some("enc"));
            }
            other => panic!("expected redacted ReasoningBlock, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_part_serde_stays_backward_compatible() {
        let old: ContentPart =
            serde_json::from_str(r#"{"type":"reasoning","text":"x"}"#).expect("old reasoning");
        assert!(matches!(
            &old,
            ContentPart::Reasoning { signature: None, redacted: None, model: None, .. }
        ));
        let v = serde_json::to_value(ContentPart::reasoning("x")).expect("serialize");
        assert_eq!(v, json!({"type": "reasoning", "text": "x"}));
    }

    #[test]
    fn tool_pairing_drops_orphans_and_reorders_results() {
        let mut msgs = vec![
            json!({"role":"user","content":"go"}),
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"a","name":"x","input":{}},
                {"type":"tool_use","id":"b","name":"y","input":{}}
            ]}),
            json!({"role":"user","content":[{"type":"text","text":"interject"}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"1"}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"zzz","content":"orphan"}]}),
        ];
        repair_tool_pairing(&mut msgs);
        // Unanswered tool_use "b" is removed.
        let calls = msgs[1]["content"].as_array().expect("assistant blocks");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"].as_str(), Some("a"));
        // Result "a" directly follows its call; orphan "zzz" is gone.
        assert_eq!(msgs[2]["content"][0]["tool_use_id"].as_str(), Some("a"));
        assert_eq!(msgs[3]["content"][0]["text"].as_str(), Some("interject"));
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn usage_merges_message_start_and_delta() {
        let mut state = SseState::new("claude-haiku-4-5".to_owned());
        parse_event(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":120,"cache_read_input_tokens":80,"output_tokens":1}}}"#,
            &mut state,
        );
        let ev = parse_event(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
            &mut state,
        );
        match ev {
            Some(StreamEvent::Done { usage: Some(u) }) => {
                assert_eq!(u.input, 120);
                assert_eq!(u.output, 42);
                assert_eq!(u.cache_read, 80);
            }
            other => panic!("expected Done with usage, got {other:?}"),
        }
    }

    #[test]
    fn sse_chunk_keeps_cjk_split_across_chunks() {
        let mut state = SseState::new("claude-haiku-4-5".to_owned());
        let line = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"中文\"}}\n";
        let bytes = line.as_bytes();
        // Split inside the first CJK character.
        let cut = line.find('中').expect("cjk present") + 1;
        let first = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(&bytes[..cut])), &mut state);
        assert!(first.is_empty());
        let second = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(&bytes[cut..])), &mut state);
        match second.first() {
            Some(Ok(StreamEvent::TextDelta(t))) => assert_eq!(t, "中文"),
            other => panic!("expected text delta, got {other:?}"),
        }
    }
}
