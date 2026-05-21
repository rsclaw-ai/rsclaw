# Fastshot routing: 3-slot LLM dispatch (heavy / fastshot / vision)

> Status: **proposed** (rsclaw-server fastshot protocol pending)
> Branch the work will land on: TBD
> Discovered: 2026-05-15 while debugging feishu session truncation and
> investigating why auxiliary LLM calls clog the heavy-model slot pool

## Problem

The agent runtime currently routes **every** LLM call through one
resolved model (the agent's `model` field, typically Qwen3.6-27B). The
worker has a fixed 4-slot KV-cache pool per node:

```
slots_total: 4
sessions: [
  agent main session A   message_count=117  n_tokens=38766
  agent main session B   message_count=116  n_tokens=41613
  heartbeat              message_count=4    n_tokens=4255
  heartbeat              message_count=4    n_tokens=4225
]
```

Any auxiliary call — `/btw` sidebar question, web search query rewrite,
web tool result compression, personal-info extraction, image description
— competes for those same 4 slots. When the pool is full, LRU evicts a
warm prefix and the next turn on the evicted session pays a 150-200 s
cold prefill (~28k token system+tools at ~150 tok/s).

The user-visible symptom: "main chat feels stuck for minutes" — but the
main chain didn't actually do anything slow; an unrelated auxiliary call
evicted its prefix cache.

The 11 known LLM call-sites that currently share the heavy pool:

| Location | Purpose | max_tok | Input size |
| --- | --- | --- | --- |
| `preparse.rs:904` | /btw sidebar | 500 | ~500 |
| `query_planner.rs:389` | web search rewrite | 400 | ~500 |
| `context_mgr.rs:813` | image description | 300 | image + small prompt |
| `runtime.rs:1095` | side query (read-only) | 500 | small |
| `context_mgr.rs:660` | personal-info extraction | 512 | few k |
| `compaction.rs:886` | extract_key_facts | 1024 | medium |
| `runtime.rs:1214` | extract_answer / web result compression | 1000 | medium |
| `tools_web.rs:1141` | web_fetch summary (opt-in) | 2000 | 10-50k |
| `computer/driver.rs:233` | computer vision grounding | 2048 | screenshot + prompt |
| `crystallizer.rs:335` | memory fact crystallization | 4096 | medium |
| `compaction.rs:818` | compact_single fallback | 4096 | 30-50k |

## Decision

Route by **importance**, not by difficulty. Unimportant or auxiliary
calls go to a separate fastshot slot pool (4B model). Vision calls go
to a third pool (VL-7B). Only the main agent chain and quality-critical
synthesis stay on the heavy 27B pool.

Three routing slots:

| Slot | Model | Pool | Deadline | Notes |
| --- | --- | --- | --- | --- |
| `fastshot` | Qwen3.5-4B | rsclaw-server fastshot pool | 5-10 s | Short-lived stateless calls |
| `vision` | Qwen2.5-VL-7B | dedicated VL pool | 10-30 s | Anything with image input |
| `heavy` | Qwen3.6-27B | main pool (4 slots) | unlimited | Main agent chain + quality-critical synthesis |

Routing budget for fastshot (Qwen3.5-4B Q4, prefill ~600 tok/s, decode
~100 tok/s):

- 5 s tier: input ≤ 2500 tokens, output ≤ 500 tokens
- 10 s tier: input ≤ 5000 tokens, output ≤ 800 tokens

### Call-site assignment

```
fastshot (4B)
├ preparse.rs:904       /btw sidebar question
├ query_planner.rs:389  web search query rewrite
├ runtime.rs:1095       side query (read-only)
├ context_mgr.rs:660    personal-info extraction
├ runtime.rs:1214       extract_answer / web result compression  ⭐
├ tools_web.rs:1141     web_fetch summary (opt-in)
└ compaction.rs:886     extract_key_facts (borderline — A/B verify)

vision (VL-7B)
├ context_mgr.rs:813    image description           (fastshot tier, 10 s)
└ computer/driver.rs    computer vision grounding   (heavy tier, no limit)

heavy (27B)
├ compaction.rs:818     compact_single  (rare; only OpenAI-compat path)
├ crystallizer.rs:335   memory crystallization
└ agent main chain      every run_turn LLM call
```

⭐ `extract_answer` is the highest-leverage migration: it triggers on
**every** web_fetch / web_browser / web_search long-text result, currently
runs on 27B, and routinely evicts the main agent's prefix cache. Moving
it to fastshot eliminates the single largest source of "main chat feels
stuck" reports.

### Web-tool-result compression specifically

`runtime.rs:5574-5587` already pre-filters HTML through
`html_dehydrate_to_text` (lol-html: strips `<script>/<style>/<nav>/
<header>/<footer>`, decodes entities, collapses whitespace) before
calling the LLM compressor. Empirically:

| Site | Raw | Dehydrated | Token (Qwen) |
| --- | --- | --- | --- |
| BBC article | 190 KB | ~20 KB | ~6-7 k |
| HN front page | 35 KB | ~5 KB | ~1.5 k |
| Wikipedia long article | 800 KB | ~100 KB | ~30-40 k |

For unimportant compression (i.e. fastshot path) we accept input
truncation rather than falling back to heavy — losing the tail of a
Wikipedia summary is preferable to evicting the main session's prefix
cache.

## Web tool architecture refactor

The routing decision above only moves the LLM compression call to a
cheaper slot. It does not address how the underlying web tools are
structured. Today `src/agent/tools_web.rs` is a 2000-line file with
all eight search backends (`duckduckgo`, `google`, `bing`, `brave`,
`bing-free`, `baidu`, `sogou`, `serper`) inlined into one giant
`match provider { ... }` block, plus a browser-based fallback that
opens a chromium tab when every text-mode backend fails.

This layout has two problems:

1. **Adding a new backend means editing the same hot file** — every
   new provider becomes another arm of the same match, with its own
   inline HTTP code and result-parsing logic. There is no boundary at
   which a contributor can drop in a new provider without touching
   the dispatcher.
2. **Capability is hardcoded to "search"** — the file calls the
   active backend for search results, then a separate code path
   (`runtime.rs:1140`) re-fetches and LLM-compresses the page bodies.
   `web_extract` (single URL → clean markdown) and `web_crawl` (seed
   URL → multi-page bundle) don't exist as first-class tools today
   even though the work is being done piecemeal through the search +
   compressor pipeline.

### Provider trait

Introduce a single trait at `src/agent/web_provider.rs`:

```rust
#[async_trait]
pub trait WebProvider: Send + Sync {
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;

    fn supports_search(&self)  -> bool { false }
    fn supports_extract(&self) -> bool { false }
    fn supports_crawl(&self)   -> bool { false }

    async fn search(&self, query: &str, limit: usize)
        -> Result<SearchResults> { bail!("not supported") }
    async fn extract(&self, urls: &[&str], opts: ExtractOpts)
        -> Result<Vec<ExtractResult>> { bail!("not supported") }
    async fn crawl(&self, seed_url: &str, opts: CrawlOpts)
        -> Result<CrawlResult> { bail!("not supported") }
}
```

Result types match what the tool wrappers already produce so the
refactor is invisible to the agent:

```rust
pub struct SearchResults {
    pub web: Vec<SearchHit>,  // {title, url, description, position}
}
pub struct ExtractResult {
    pub url: String,
    pub title: String,
    pub content: String,       // markdown preferred, html fallback
    pub metadata: Value,
}
```

### Provider registry + per-capability routing

`src/agent/web_registry.rs` resolves `(capability, config)` to a
provider:

```rust
pub struct WebRegistry {
    providers: HashMap<String, Arc<dyn WebProvider>>,
}

impl WebRegistry {
    pub fn resolve(&self, cap: Capability, configured: Option<&str>)
        -> Option<Arc<dyn WebProvider>> {
        // 1. configured-and-capable wins (even if !is_available, so
        //    the user sees a precise "set X_API_KEY" error, not a
        //    silent switch to a different backend).
        // 2. single capable+available provider shortcut.
        // 3. legacy preference walk filtered by availability:
        //    firecrawl > brave > google > bing > serper > duckduckgo
        //    > bing-free > baidu > sogou
    }
}
```

Config gets per-capability slots so search and extract can use
different backends (the typical pattern: free HTML scraper for
search, AI-optimized SaaS for extract):

```json5
web: {
  search_backend:  "bing-free",     // 中文场景 HTML scrape 优于 SaaS
  extract_backend: "firecrawl",     // 干净 markdown
  crawl_backend:   "firecrawl",
  backend:         "firecrawl",     // shared fallback
}
```

### New providers to add

The existing eight backends already cover search across global and
Chinese engines. The gap is **AI-optimized providers** that return
LLM-friendly content directly, eliminating most of the per-fetch
compression overhead.

| Provider | Adds | Why it matters |
| --- | --- | --- |
| **Firecrawl** | extract + crawl + interact | Server-side HTML → clean markdown via paid SDK. Gives us `web_extract` (single URL, replaces the compress_tool_result path for known URLs) and `web_crawl` (seed URL, no built-in equivalent today). Replaces the LLM compressor for most pages because the markdown is already token-budgeted-ish. |
| **Exa** | search + extract | Neural search with `contextMaxCharacters` server-side cap (default 10k chars total across all results). For English/global queries the curated content is more LLM-friendly than HTML-scraped snippets. Chinese site coverage is limited so it does not replace the free scrapers for `web.search_backend = bing-free` users. |
| **Tavily** | search + extract + crawl | Agent-friendly search with built-in summarization. Cheaper than Firecrawl, less clean markdown but acceptable for follow-up reading. |

All three are added as new modules under `src/agent/web_providers/`
and registered at startup based on env-var presence (no eager init —
the SDK / HTTP client only constructs when a tool call actually
dispatches to that provider).

### Tool wrapper behavior

Three tool surfaces stay agent-visible:

```
web_search  (existing)        → search backend
web_extract (new)             → extract backend, hard output cap 5000 chars
web_crawl   (new, optional)   → crawl backend
```

`web_extract`'s post-fetch compression policy:

```
content_len  action
─────────────────────────────────────────────────────────────────
> 2 MB       reject with "use web_crawl for large sites"
> 500 KB     chunked: split 100 KB chunks, parallel fastshot 4B
             compress each, then synthesize → cap 5000 chars
> 5 KB       single fastshot 4B compress → cap 5000 chars
< 5 KB       skip compression, return as-is
```

Compression failures fall back to truncating the first 5000 chars
of the dehydrated content with a "[truncated, raise auxiliary
timeout]" footer — never return an error to the agent, because
agents handle truncated content sensibly but loop on errors.

This makes `runtime.rs:1140 compress_tool_result_for_session`
redundant: every web tool already returns content within the 5000
char budget. The dispatch path in `runtime.rs:5574-5587` reduces to
"pass through raw" because the compression already happened in the
extract provider. That call site can be deleted once the new
`web_extract` tool is wired up.

### What stays unchanged

- The eight existing search providers stay — just refactored into
  per-provider modules behind the trait. No behavior change for
  users who never set `web.extract_backend`.
- Browser fallback (`browser_navigate` opening chromium when all
  text-mode searches fail) stays. It is the only provider that
  bypasses bot-detection on captcha-walled sites.
- Website-policy gating (`tools/website_policy.rs`) and SSRF checks
  stay at the wrapper layer, applied uniformly to every provider's
  extracted URLs after redirect resolution.

## Out of scope

- **Splitting heavy across multiple worker nodes**: orthogonal — solves
  capacity, not the auxiliary-call interference described here.
- **Per-channel slot affinity**: bigger refactor; revisit if fastshot
  isolation alone doesn't recover main-chat latency.
- **Removing `compact_single` fallback for kv_cache_mode=2**: already
  done in a separate change; not part of this routing decision.

## Implementation outline

1. Config schema: add `routing` block with `fastshot` / `vision` / `heavy`
   model slots (each is just a model string the existing provider/model
   resolver understands).

   ```json5
   routing: {
     fastshot: "rsclaw/qwen3.5-4b",
     vision:   "rsclaw/qwen2.5-vl-7b",
     heavy:    "rsclaw/qwen3.6-27b"
   }
   ```

2. Provider builder: `LlmRequest::fastshot()` / `::vision()` helpers that
   pre-fill `tools: vec![]`, `thinking_budget: None`, `kv_cache_mode: 0`,
   tight `max_tokens`, low `temperature`.

3. Deadline: wrap the fastshot call sites in
   `tokio::time::timeout(Duration::from_secs(N), ...)`. On timeout,
   fastshot calls **fail-fast** rather than fall back to heavy — falling
   back to heavy under load defeats the isolation we just bought.

4. Site-by-site migration: do `extract_answer` first (biggest win), then
   `/btw`, then the rest. Each migration is one call-site swap, no logic
   change.

5. Vision path needs to confirm rsclaw-server / rsclaw-llm route
   multimodal `content[].type=image` correctly. `/v1/messages` already
   accepts Anthropic-style image blocks; the rsclaw `/sessions/<id>/turn`
   path needs validation.

6. Web tool refactor — separate task, can ship after the routing
   work lands:

   - Extract `WebProvider` trait from `tools_web.rs`.
   - Move each existing backend (`duckduckgo`, `google`, `bing`,
     `brave`, `bing-free`, `baidu`, `sogou`, `serper`, browser
     fallback) into its own module under `src/agent/web_providers/`.
   - Add `WebRegistry` with per-capability resolution.
   - Add `firecrawl` provider (extract + crawl). Optional: `exa`,
     `tavily`. All three are env-var-gated; absent keys → not
     registered → legacy preference walk falls through to existing
     HTML scrapers.
   - Wire new `web_extract` tool with 5000 char output cap,
     compression on fastshot slot.
   - Delete `runtime.rs:1140 compress_tool_result_for_session` once
     `web_extract` is in production.

## Open questions

- Does `extract_key_facts` (60k char input cap) tolerate fastshot
  quality? Needs A/B on actual stored memory diff.
- Should `image_description` and `computer/driver` truly share the
  VL pool, or do they need separate slots? Image description is
  short-lived; computer/driver holds a session across many steps.
- `tools_web.rs:1141` summary_model is opt-in today. Make it default
  to `routing.fastshot` when unset?
- Default `web.extract_backend` when both Firecrawl key and the
  built-in HTML scraper are available? Firecrawl is paid but gives
  cleaner markdown; built-in is free and works for most pages after
  `html_dehydrate_to_text`. Suggest: built-in scraper default,
  Firecrawl opt-in via explicit `web.extract_backend = firecrawl`.
- Should `web_crawl` be agent-visible at all, or kept behind a flag?
  Multi-page crawl can rack up cost and tokens fast; safer to keep
  manual until we have a budget enforcement layer.

## References

- Worker truncation root cause (UTF-8 byte boundary in SSE
  text_delta) — debugged 2026-05-15, fixed worker-side.
- Worker pool size: 4 slots (`/sessions` endpoint at
  `http://rsclaw-worker:8001/sessions`).
- Cold prefill cost at ~28k prefix tokens: ~150-200 s.
