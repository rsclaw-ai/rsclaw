//! Tool handler for `read_artifact` — LLM-side companion to the artifact
//! store. When the runtime backstop replaces a large tool_result with a
//! preview + `tool_result_id`, the LLM uses this tool to fetch the full
//! content (or a slice of it).
//!
//! Modes:
//! - `full` (default) — return entire artifact text
//! - `head:N` — first N lines
//! - `tail:N` — last N lines
//! - `lines:A-B` — line range (1-indexed, inclusive)
//! - `grep:PATTERN` — lines matching regex (case-insensitive)

use anyhow::{Result, anyhow};
use regex::RegexBuilder;
use rsclaw_artifact::{ArtifactId, default_store};
use serde_json::{Value, json};

use super::runtime::{AgentRuntime, RunContext};

/// Apply `mode` to `full` text and return the selected slice.
///
/// Factored out from the tool handler so unit tests can hit the parser
/// without standing up a `RunContext`. Modes:
/// - `full`         — entire text (returns `full` clone)
/// - `stat`         — size summary only, no content (kept as `Ok("")` here; the
///   handler attaches structured fields to the response Value)
/// - `head:N`       — first N lines (N=0 → empty)
/// - `tail:N`       — last N lines (N=0 → empty)
/// - `lines:A-B`    — 1-indexed inclusive range, clamped to `[1, total]`
/// - `grep:PATTERN` — case-insensitive regex over lines
pub(crate) fn apply_mode(full: &str, mode: &str) -> Result<String> {
    // Defensive trim against the v1 tool-call protocol's trailing-newline
    // leak (see read_session_archive::apply_archive_mode for the same guard).
    let mode = mode.trim();
    let lines: Vec<&str> = full.lines().collect();
    let total = lines.len();
    if mode == "full" {
        return Ok(full.to_owned());
    }
    if mode == "stat" {
        // Stat mode returns no content; the handler decorates the JSON
        // response with line/char/byte counts instead.
        return Ok(String::new());
    }
    if let Some(rest) = mode.strip_prefix("head:") {
        let n: usize = rest
            .parse()
            .map_err(|_| anyhow!("read_artifact: bad head count `{rest}`"))?;
        return Ok(lines.iter().take(n).copied().collect::<Vec<_>>().join("\n"));
    }
    if let Some(rest) = mode.strip_prefix("tail:") {
        let n: usize = rest
            .parse()
            .map_err(|_| anyhow!("read_artifact: bad tail count `{rest}`"))?;
        let start = total.saturating_sub(n);
        return Ok(lines[start..].join("\n"));
    }
    if let Some(range) = mode.strip_prefix("lines:") {
        let (a, b) = range
            .split_once('-')
            .ok_or_else(|| anyhow!("read_artifact: `lines:A-B` malformed: `{range}`"))?;
        let a: usize = a
            .parse()
            .map_err(|_| anyhow!("read_artifact: bad start line `{a}`"))?;
        let b: usize = b
            .parse()
            .map_err(|_| anyhow!("read_artifact: bad end line `{b}`"))?;
        if a == 0 || b < a {
            return Err(anyhow!(
                "read_artifact: lines:A-B must satisfy 1 ≤ A ≤ B, got {a}-{b}"
            ));
        }
        // Clamp both endpoints so an LLM asking for lines:100-200 on a
        // 5-line file gets an empty slice instead of a panic.
        let lo = a.saturating_sub(1).min(total);
        let hi = b.min(total).max(lo);
        return Ok(lines[lo..hi].join("\n"));
    }
    if let Some(pattern) = mode.strip_prefix("grep:") {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| anyhow!("read_artifact: grep pattern invalid: {e}"))?;
        return Ok(lines
            .iter()
            .filter(|l| re.is_match(l))
            .copied()
            .collect::<Vec<_>>()
            .join("\n"));
    }
    Err(anyhow!(
        "read_artifact: unknown mode `{mode}`. Use full | head:N | tail:N | lines:A-B | grep:PATTERN"
    ))
}

/// Truncate `text` to the largest prefix of WHOLE lines whose estimated
/// token count stays within `budget`. Returns
/// `(page_text, lines_in_page, total_lines_in_text)`. When `text` already
/// fits, `page_text == text` and `lines_in_page == total`.
///
/// This is the per-turn pagination floor for `read_artifact`: a `mode=full`
/// (or any mode) result that would blow `max_per_turn_input_tokens` is
/// served one page at a time instead of dumped whole — lossless (the full
/// artifact stays on disk) and bounded (each page ≤ budget). The model
/// pages on via `lines:A-B` / `grep:`.
pub(crate) fn paginate_to_budget(text: &str, budget_tokens: usize) -> (String, usize, usize) {
    use crate::context_mgr::estimate_tokens;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if estimate_tokens(text) <= budget_tokens {
        return (text.to_owned(), total, total);
    }
    let Some(&first) = lines.first() else {
        // Empty text already fits above; this is just defensive.
        return (String::new(), 0, 0);
    };
    // Pathological: the FIRST line alone busts the budget. Hard
    // char-truncate it into a single page so we never return an empty
    // or over-budget page. ~4 chars/token is the ASCII upper bound; CJK
    // comes in well under budget.
    if estimate_tokens(first) > budget_tokens {
        let cap_chars = budget_tokens.saturating_mul(4).max(1);
        let truncated: String = first.chars().take(cap_chars).collect();
        return (truncated, 1, total);
    }
    // First line fits — always include it, then greedily accumulate whole
    // lines until the next would bust the budget. Guarantees n ≥ 1.
    let mut acc = String::from(first);
    let mut used = estimate_tokens(first);
    let mut n = 1usize;
    for line in &lines[1..] {
        let line_tokens = estimate_tokens(line) + 1; // +1 for the rejoin '\n'
        if used + line_tokens > budget_tokens {
            break;
        }
        acc.push('\n');
        acc.push_str(line);
        used += line_tokens;
        n += 1;
    }
    (acc, n, total)
}

/// Split a tool-result `content` into `(body, trailing_handle_marker)`.
///
/// The runtime backstop appends a recovery marker to truncated tool
/// results, e.g. `"\n\n[truncated — call read_artifact(tool_result_id=
/// \"tr_…\") for full output]"`. The per-turn aggregate guard needs to
/// re-trim such results WITHOUT dropping that marker (dropping it would
/// turn lossless pagination into lossy truncation). Returns the body
/// before the marker plus the marker slice when one is present; otherwise
/// `(content, None)`.
pub(crate) fn split_artifact_marker(content: &str) -> (&str, Option<&str>) {
    if let Some(pos) = content.rfind("\n\n[") {
        let tail = &content[pos..];
        if tail.contains("read_artifact") {
            return (&content[..pos], Some(tail));
        }
    }
    (content, None)
}

/// 1-indexed artifact line where the `mode`-selected slice begins, when
/// the slice maps to a contiguous range (so a precise `lines:A-B` next-page
/// hint is possible). `None` for `grep` (non-contiguous) and `stat`.
fn selected_start_line(mode: &str, total_lines: usize) -> Option<usize> {
    let mode = mode.trim();
    if mode == "full" || mode.starts_with("head:") {
        return Some(1);
    }
    if let Some(rest) = mode.strip_prefix("lines:")
        && let Some((a, _)) = rest.split_once('-')
        && let Ok(a) = a.parse::<usize>()
    {
        return Some(a.max(1));
    }
    if let Some(rest) = mode.strip_prefix("tail:")
        && let Ok(n) = rest.parse::<usize>()
    {
        return Some(total_lines.saturating_sub(n) + 1);
    }
    None
}

/// A line-windowed slice of an artifact, used by ad-hoc semantic search.
struct SearchChunk {
    text: String,
    /// 0-indexed first line of the chunk.
    start_line: usize,
    /// 0-indexed line one past the chunk's last line.
    end_line: usize,
}

/// Split text into ~line-windowed chunks for transient semantic search.
/// Deliberately simpler than the KB markdown chunker — no LogicalSourceId /
/// locator machinery, just fixed windows with line tracking, which is all a
/// one-blob top-k retrieval needs.
fn chunk_for_search(full: &str) -> Vec<SearchChunk> {
    const WINDOW: usize = 40;
    let lines: Vec<&str> = full.lines().collect();
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let end = (i + WINDOW).min(lines.len());
        let text = lines[i..end].join("\n");
        if !text.trim().is_empty() {
            chunks.push(SearchChunk {
                text,
                start_line: i,
                end_line: end,
            });
        }
        i = end;
    }
    chunks
}

/// Cosine similarity of two equal-length embedding vectors.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na < 1e-12 || nb < 1e-12 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Fallback chunk scorer when no embedder is available: term-overlap count.
fn substring_score(chunks: &[SearchChunk], query: &str) -> Vec<(usize, f32)> {
    let ql = query.to_lowercase();
    let terms: Vec<&str> = ql.split_whitespace().collect();
    let mut scored: Vec<(usize, f32)> = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let cl = c.text.to_lowercase();
            let hits = terms.iter().filter(|t| cl.contains(*t)).count();
            (i, hits as f32)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

impl AgentRuntime {
    /// Per-call read budget for `read_artifact` — much wider than the generic
    /// per-turn floor (`max_per_turn_input_tokens`), so a deliberate read of a
    /// SKILL.md / web page comes back whole in one shot. Still BOUNDED
    /// (`max_artifact_read_tokens`, default 16000): content beyond this is
    /// served chunk-by-chunk via the server-side cursor, so a single read can
    /// never blow the session context.
    async fn artifact_read_budget(&self) -> usize {
        self.live
            .agents
            .read()
            .await
            .defaults
            .max_artifact_read_tokens
            .unwrap_or(16_000) as usize
    }

    pub(crate) async fn tool_read_artifact(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        // Trim string args: the rsclaw v1 tool-call protocol leaks a trailing
        // newline into them (same root cause as read_session_archive `mode`
        // and the computer `action` arg). Untrimmed, `tool_result_id="tr_x\n"`
        // fails to resolve and `mode="grep:x\n"` matches nothing.
        let id_str = args["tool_result_id"]
            .as_str()
            .or_else(|| args["id"].as_str())
            .map(str::trim)
            .ok_or_else(|| {
                anyhow!(
                    "read_artifact: `tool_result_id` must be a JSON string (missing or \
                     non-string value given). Pass the id exactly as it appears in the \
                     truncation marker, e.g. tool_result_id=\"tr_abc123\"."
                )
            })?;
        let id = ArtifactId::parse(id_str)?;

        // No mode = cursor mode (return the next unread chunk). The model
        // never has to compute line ranges; calling again always advances.
        let mode = args["mode"].as_str().unwrap_or("").trim();
        let store = default_store();
        let full = store.read(&ctx.session_key, &id).map_err(|e| {
            anyhow!(
                "artifact `{}` not found in session `{}` ({e}). \
                 Sessions are independent — an id from another session won't resolve here.",
                id.as_str(),
                ctx.session_key
            )
        })?;

        let total_lines = full.lines().count();
        let budget = self.artifact_read_budget().await;

        // stat — size summary only, no content.
        if mode == "stat" {
            return Ok(json!({
                "tool_result_id": id.as_str(),
                "mode": "stat",
                "total_lines": total_lines,
                "returned_chars": 0,
                "content": "",
                "byte_size": full.len(),
                "char_count": full.chars().count(),
            }));
        }

        // query:QUESTION — semantic search over the artifact (no cursor/paging).
        if let Some(q) = mode.strip_prefix("query:") {
            return self
                .artifact_semantic_search(&id, &full, q.trim(), budget)
                .await;
        }

        // Random-access modes (head/tail/lines/grep): explicit, bypass the
        // cursor, bounded to the (now wider) budget.
        let is_random_access = mode.starts_with("head:")
            || mode.starts_with("tail:")
            || mode.starts_with("lines:")
            || mode.starts_with("grep:");
        if is_random_access {
            let selected = apply_mode(&full, mode)?;
            let (page, page_lines, selected_lines) = paginate_to_budget(&selected, budget);
            let truncated = page_lines < selected_lines;
            let mut out = json!({
                "tool_result_id": id.as_str(),
                "mode": mode,
                "total_lines": total_lines,
                "returned_chars": page.chars().count(),
                "content": page,
            });
            if truncated {
                out["truncated"] = json!(true);
                out["returned_lines"] = json!(page_lines);
                out["selected_lines"] = json!(selected_lines);
                let next = match selected_start_line(mode, total_lines) {
                    Some(start) => {
                        let next_start = start + page_lines;
                        format!(
                            "Returned lines {start}-{} of this slice. Call read_artifact with \
                             mode=\"lines:{next_start}-{total_lines}\" for the next page, or \
                             grep:PATTERN / query:QUESTION to jump straight to what you need.",
                            start + page_lines - 1
                        )
                    }
                    None => format!(
                        "Returned {page_lines} of {selected_lines} matching lines. Narrow the \
                         grep:PATTERN or use query:QUESTION."
                    ),
                };
                out["next"] = json!(next);
            }
            return Ok(out);
        }

        // Cursor modes: "" / "next" advance from the saved cursor; "full" /
        // "reset" start over from the top. Re-reading the same page is
        // impossible — each call moves the cursor forward — which is the whole
        // point: it removes the stateful "compute the next lines:A-B" burden
        // that even strong models fail at on long content.
        let from_top = mode == "full" || mode == "reset";
        let cursor_key = format!("{}\u{0}{}", ctx.session_key, id.as_str());
        let lines: Vec<&str> = full.lines().collect();

        let start_line = if from_top {
            0
        } else {
            self.artifact_cursors
                .lock()
                .map(|m| m.get(&cursor_key).copied().unwrap_or(0))
                .unwrap_or(0)
        };

        // Already past the end (a bare "next" after everything was read):
        // say so clearly instead of returning an empty page the model loops on.
        if start_line >= total_lines && !from_top {
            return Ok(json!({
                "tool_result_id": id.as_str(),
                "mode": "next",
                "total_lines": total_lines,
                "at_end": true,
                "content": "[END of artifact — all content has already been read]",
                "next": "Nothing left to read. Use mode=\"reset\" to start over, or \
                         grep:PATTERN / query:QUESTION to search specific content.",
            }));
        }

        // On the first chunk of content that won't fit in one page, prepend a
        // best-effort flash summary so the model can orient (and choose to
        // grep/query) instead of blindly paging.
        let summary = if start_line == 0 && crate::context_mgr::estimate_tokens(&full) > budget {
            self.artifact_summary(&ctx.session_key, &id, &full).await
        } else {
            None
        };
        let reserve = if summary.is_some() { 700 } else { 0 };
        let page_budget = budget.saturating_sub(reserve).max(256);

        let remaining_text = lines[start_line.min(lines.len())..].join("\n");
        let (page, page_lines, _sel) = paginate_to_budget(&remaining_text, page_budget);
        let next_line = start_line + page_lines;
        let at_end = next_line >= total_lines;

        if let Ok(mut m) = self.artifact_cursors.lock() {
            m.insert(cursor_key, if at_end { total_lines } else { next_line });
        }

        let content = if let Some(ref s) = summary {
            format!(
                "[AI summary of the full {total_lines}-line artifact — use it to decide whether \
                 to keep reading, grep:PATTERN, or query:QUESTION]\n{s}\n\n\
                 [--- full content, lines {}-{} ---]\n{page}",
                start_line + 1,
                next_line
            )
        } else {
            page
        };

        let mut out = json!({
            "tool_result_id": id.as_str(),
            "mode": "next",
            "total_lines": total_lines,
            "returned_lines": page_lines,
            "from_line": start_line + 1,
            "to_line": next_line,
            "content": content,
        });
        if at_end {
            out["at_end"] = json!(true);
        } else {
            out["next"] = json!(format!(
                "Showed lines {}-{} of {total_lines}. Call read_artifact again (no mode) to \
                 continue from line {}; or grep:PATTERN / query:QUESTION to jump to specifics.",
                start_line + 1,
                next_line,
                next_line + 1
            ));
        }
        Ok(out)
    }

    /// Best-effort flash summary of an artifact, cached in a `<id>.summary.txt`
    /// sidecar so it is generated at most once. Returns `None` on any failure —
    /// the summary is a nicety, never required for correctness.
    async fn artifact_summary(
        &self,
        session_key: &str,
        id: &ArtifactId,
        full: &str,
    ) -> Option<String> {
        let store = default_store();
        if let Some(cached) = store.read_summary(session_key, id) {
            return Some(cached);
        }
        // Cap the input fed to flash so a giant artifact doesn't bust its own
        // context — a gist of the head is enough to orient the reader.
        let input: String = full.chars().take(48_000).collect();
        let summary = self.flash_summarize(&input).await?;
        if let Err(e) = store.write_summary(session_key, id, &summary) {
            tracing::warn!(error = %e, "artifact: failed to cache summary sidecar");
        }
        Some(summary)
    }

    /// One-shot, stateless flash completion that condenses `text` into a short
    /// gist + section outline. Calls the provider registry directly (like
    /// `query_planner`) so it works behind `&self` — the FailoverManager path
    /// needs `&mut`, which the tool dispatch can't give.
    async fn flash_summarize(&self, text: &str) -> Option<String> {
        use futures::StreamExt;
        use rsclaw_provider::{
            AgentEndpoint, LlmRequest, Message, MessageContent, Role, StreamEvent,
        };

        let flash_model = self.resolve_flash_model_name();
        let (provider_name, model_id) = self.providers.resolve_model(&flash_model);
        let provider = match self.providers.get(provider_name) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("artifact summary: flash provider unavailable: {e:#}");
                return None;
            }
        };

        let req = LlmRequest {
            fallback_models: Vec::new(),
            model: model_id.to_owned(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(format!(
                    "Summarize the following document for another AI that will then read \
                     specific parts of it. Give a 2-4 sentence gist, then a short bullet \
                     outline of the main sections/topics. Be faithful and concise; do not \
                     invent.\n\n{text}"
                )),
                rsclaw_hidden: None,
            }],
            tools: vec![],
            system: Some(
                "You are a precise document summarizer. Output only the gist and outline."
                    .to_owned(),
            ),
            max_tokens: Some(400),
            temperature: Some(0.0),
            frequency_penalty: None,
            thinking_budget: None,
            endpoint: AgentEndpoint::Flash,
            kv_cache_mode: 0,
            session_key: None,
            system_shared: None,
            user_system: None,
            recall: None,
        };

        let mut stream = match provider.stream(req).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("artifact summary flash call failed: {e:#}");
                return None;
            }
        };
        let mut buf = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(StreamEvent::TextDelta(d)) => buf.push_str(&d),
                Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("artifact summary stream error: {e:#}");
                    break;
                }
            }
        }
        let buf = buf.trim().to_owned();
        if buf.is_empty() { None } else { Some(buf) }
    }

    /// Semantic search over a single artifact, reusing the hot KB embedder
    /// (no persistent index): chunk → embed chunks + query → cosine top-k.
    /// Degrades to term-overlap scoring when the embedder is unavailable.
    async fn artifact_semantic_search(
        &self,
        id: &ArtifactId,
        full: &str,
        query: &str,
        budget: usize,
    ) -> Result<Value> {
        if query.is_empty() {
            return Err(anyhow!("read_artifact: query:QUESTION must be non-empty"));
        }
        let chunks = chunk_for_search(full);
        if chunks.is_empty() {
            return Ok(json!({
                "tool_result_id": id.as_str(),
                "mode": format!("query:{query}"),
                "matches": [],
                "note": "Artifact is empty.",
            }));
        }

        let ranked: Vec<(usize, f32)> = if let Some(kb) = rsclaw_kb::global_service() {
            let embedder = kb.embedder();
            let mut inputs: Vec<String> = Vec::with_capacity(chunks.len() + 1);
            inputs.push(query.to_owned());
            inputs.extend(chunks.iter().map(|c| c.text.clone()));
            let n = chunks.len();
            match tokio::task::spawn_blocking(move || embedder.embed_batch(&inputs)).await {
                Ok(Ok(vecs)) if vecs.len() == n + 1 => {
                    let q = vecs[0].clone();
                    let mut scored: Vec<(usize, f32)> = vecs[1..]
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (i, cosine(&q, v)))
                        .collect();
                    scored
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                    scored
                }
                _ => substring_score(&chunks, query),
            }
        } else {
            substring_score(&chunks, query)
        };

        let mut used = 0usize;
        let mut matches = Vec::new();
        for (i, score) in ranked.into_iter().take(12) {
            if score <= 0.0 && !matches.is_empty() {
                break;
            }
            let c = &chunks[i];
            let t = crate::context_mgr::estimate_tokens(&c.text);
            if used + t > budget && !matches.is_empty() {
                break;
            }
            used += t;
            matches.push(json!({
                "from_line": c.start_line + 1,
                "to_line": c.end_line,
                "score": (score * 1000.0).round() / 1000.0,
                "text": c.text,
            }));
            if used >= budget {
                break;
            }
        }

        Ok(json!({
            "tool_result_id": id.as_str(),
            "mode": format!("query:{query}"),
            "total_lines": full.lines().count(),
            "matches": matches,
            "note": "Top semantically-relevant sections. Read surrounding context with \
                     lines:A-B, or refine with another query:QUESTION.",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        (1..=5)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn full_returns_everything() {
        assert_eq!(apply_mode(&sample(), "full").unwrap(), sample());
    }

    // Regression: v1 tool-call protocol leaks a trailing newline into the
    // mode arg; untrimmed it broke exact-match and grep regexes.
    #[test]
    fn mode_tolerates_trailing_newline() {
        assert_eq!(apply_mode(&sample(), "full\n").unwrap(), sample());
        assert_eq!(apply_mode(&sample(), "head:2\n").unwrap(), "line1\nline2");
        assert_eq!(apply_mode(&sample(), "grep:line3\n").unwrap(), "line3");
    }

    #[test]
    fn head_n_takes_first_n_lines() {
        assert_eq!(apply_mode(&sample(), "head:2").unwrap(), "line1\nline2");
    }

    #[test]
    fn head_zero_returns_empty() {
        assert_eq!(apply_mode(&sample(), "head:0").unwrap(), "");
    }

    #[test]
    fn tail_n_takes_last_n_lines() {
        assert_eq!(apply_mode(&sample(), "tail:2").unwrap(), "line4\nline5");
    }

    #[test]
    fn tail_over_total_returns_all() {
        assert_eq!(apply_mode(&sample(), "tail:99").unwrap(), sample());
    }

    #[test]
    fn lines_range_inclusive_one_indexed() {
        assert_eq!(
            apply_mode(&sample(), "lines:2-4").unwrap(),
            "line2\nline3\nline4"
        );
    }

    #[test]
    fn lines_out_of_range_clamps_no_panic() {
        // Regression: a=100, total=5 used to panic on `lines[99..5]` (start > end).
        let out = apply_mode(&sample(), "lines:100-200").unwrap();
        assert_eq!(out, "");
        let out = apply_mode(&sample(), "lines:3-200").unwrap();
        assert_eq!(out, "line3\nline4\nline5");
    }

    #[test]
    fn lines_invalid_ranges_rejected() {
        assert!(apply_mode(&sample(), "lines:0-3").is_err());
        assert!(apply_mode(&sample(), "lines:5-3").is_err());
        assert!(apply_mode(&sample(), "lines:abc").is_err());
    }

    #[test]
    fn grep_filters_case_insensitive() {
        let body = "INFO ok\nERROR bad\ninfo also ok\nWARN meh";
        let out = apply_mode(body, "grep:error").unwrap();
        assert_eq!(out, "ERROR bad");
        let out = apply_mode(body, "grep:^info").unwrap();
        assert_eq!(out, "INFO ok\ninfo also ok");
    }

    #[test]
    fn unknown_mode_rejected() {
        let err = apply_mode("x", "weirdo").unwrap_err().to_string();
        assert!(err.contains("unknown mode"), "got: {err}");
    }

    // -------- pagination (max_per_turn_input_tokens floor) --------

    #[test]
    fn paginate_under_budget_returns_whole() {
        let text = sample(); // 5 short lines, well under any real budget
        let (page, n, total) = paginate_to_budget(&text, 5_000);
        assert_eq!(page, text);
        assert_eq!(n, 5);
        assert_eq!(total, 5);
    }

    #[test]
    fn paginate_over_budget_returns_whole_lines_only() {
        // 200 lines of ~10 ASCII tokens each (~40 chars) ≈ 2000 tokens total.
        // A 100-token budget should return the first handful of WHOLE lines.
        let text = (1..=200)
            .map(|i| format!("line{i} aaaa bbbb cccc dddd eeee ffff gggg"))
            .collect::<Vec<_>>()
            .join("\n");
        let total_lines = text.lines().count();
        let (page, n, total) = paginate_to_budget(&text, 100);
        assert_eq!(total, total_lines);
        assert!(
            n > 0 && n < total_lines,
            "expected a partial page, got {n}/{total_lines}"
        );
        // Page must be a whole-line prefix (no mid-line cut) and within budget.
        assert!(text.starts_with(&page));
        assert!(page.ends_with(|c: char| c != '\n'));
        assert!(
            crate::context_mgr::estimate_tokens(&page) <= 100 + 20,
            "page should be ~within budget"
        );
    }

    #[test]
    fn paginate_single_giant_line_hard_truncates() {
        // One line that alone busts the budget — must still return exactly
        // one (char-truncated) line rather than an empty or over-budget page.
        let giant = "x".repeat(100_000); // ~25k tokens on one line
        let (page, n, _total) = paginate_to_budget(&giant, 50);
        assert_eq!(n, 1);
        assert!(page.len() < giant.len(), "giant line must be truncated");
        assert!(!page.is_empty());
    }

    #[test]
    fn split_artifact_marker_extracts_trailing_handle() {
        let content = "some preview body\nline 2\n\n[truncated — call read_artifact(tool_result_id=\"tr_abc\") for full output]";
        let (body, marker) = split_artifact_marker(content);
        assert_eq!(body, "some preview body\nline 2");
        assert!(marker.unwrap().contains("read_artifact"));
        assert!(marker.unwrap().contains("tr_abc"));
    }

    #[test]
    fn split_artifact_marker_none_when_no_handle() {
        let content = "plain tool result with no artifact handle\n\n[just a note]";
        let (body, marker) = split_artifact_marker(content);
        // The trailing bracket block doesn't mention read_artifact → not a handle.
        assert_eq!(body, content);
        assert!(marker.is_none());
    }

    #[test]
    fn split_artifact_marker_plain_text() {
        let (body, marker) = split_artifact_marker("just some output");
        assert_eq!(body, "just some output");
        assert!(marker.is_none());
    }

    #[test]
    fn selected_start_line_maps_contiguous_modes() {
        assert_eq!(selected_start_line("full", 100), Some(1));
        assert_eq!(selected_start_line("head:20", 100), Some(1));
        assert_eq!(selected_start_line("lines:30-90", 100), Some(30));
        assert_eq!(selected_start_line("tail:10", 100), Some(91));
        // grep / stat are non-contiguous → no precise next-range.
        assert_eq!(selected_start_line("grep:foo", 100), None);
        assert_eq!(selected_start_line("stat", 100), None);
    }

    #[test]
    fn chunk_for_search_windows_with_line_tracking() {
        // 95 lines → windows of 40 → [0..40, 40..80, 80..95].
        let text = (1..=95)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_for_search(&text);
        assert_eq!(chunks.len(), 3);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (0, 40));
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (40, 80));
        assert_eq!((chunks[2].start_line, chunks[2].end_line), (80, 95));
        assert!(chunks[0].text.starts_with("L1\n"));
        assert!(chunks[2].text.ends_with("L95"));
    }

    #[test]
    fn chunk_for_search_empty_input() {
        assert!(chunk_for_search("").is_empty());
    }

    #[test]
    fn cosine_identical_orthogonal_and_mismatch() {
        let a = [1.0_f32, 0.0, 0.0];
        let b = [1.0_f32, 0.0, 0.0];
        let c = [0.0_f32, 1.0, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &c).abs() < 1e-6);
        // Length mismatch → 0.0, never a panic.
        assert_eq!(cosine(&a, &[1.0_f32, 0.0]), 0.0);
        // Zero vector → 0.0 (no NaN).
        assert_eq!(cosine(&a, &[0.0_f32, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn substring_score_ranks_by_term_overlap() {
        let chunks = vec![
            SearchChunk {
                text: "the quick brown fox".to_owned(),
                start_line: 0,
                end_line: 1,
            },
            SearchChunk {
                text: "lazy dog sleeps".to_owned(),
                start_line: 1,
                end_line: 2,
            },
            SearchChunk {
                text: "quick lazy fox jumps".to_owned(),
                start_line: 2,
                end_line: 3,
            },
        ];
        let ranked = substring_score(&chunks, "quick fox");
        // Chunk 0 and 2 both contain both terms; chunk 1 contains neither.
        assert_eq!(ranked[0].1, 2.0);
        assert_eq!(ranked.last().unwrap().1, 0.0);
        assert_eq!(ranked.last().unwrap().0, 1);
    }
}
