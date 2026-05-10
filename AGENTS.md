# AGENTS.md — RsClaw

AI coding-assistant reference for Claude Code, Cursor, Copilot, and any agent-driven session.
**Read this file completely before touching any code or proposing architecture.**

---

## Identity (the part that prevents amnesia)

**RsClaw is an AI agent product** — a ground-up Rust rewrite of
[OpenClaw](https://github.com/openclaw/openclaw) (TypeScript/Node.js) with
full protocol compatibility. It runs as a local gateway, a server daemon, or
a Tauri desktop app. The user-visible identity is **"Crab AI Assistant"**.

**Core responsibilities:**

- Receive user input from 13 messaging channels and dispatch to agent runtimes
- Speak OpenAI-compatible, A2A v0.3, ACP, and WebSocket v3 protocols on the **inbound** side
- Drive an agent loop with heartbeat / meditation / evolution / tool dispatch
- Manage LLM providers with cross-provider failover (internal plumbing, not a customer product)
- Store conversation history (redb KV + tantivy full-text + hnsw_rs vector)
- Expose a Tauri desktop UI (Next.js frontend, NextChat-derived)

**It is NOT:**

- ❌ NOT a multi-protocol AI gateway. NOT LiteLLM. NOT Helicone. NOT Portkey.
- ❌ NOT a customer-facing protocol bridge. The provider abstraction in
  `src/provider/` is RsClaw's **internal** plumbing for talking to LLMs;
  it is not a product surface that customers configure.

**Why this matters:** A repeated AI-assistant failure mode is to see
`ProviderRegistry` + `FailoverManager` + 30+ provider integrations and
conclude "this is a LiteLLM-shaped gateway." It is not. Every agent product
needs internal LLM abstraction; that doesn't make it a gateway. **If a
feature feels customer-gateway-shaped (vault, tier-based pricing,
cross-provider routing exposed to clients), pause and ask: does this
serve the agent's purpose, or am I drifting toward LiteLLM-think?**

```

**Data flow (inbound):**

```
User ─→ messaging channel (Telegram / Feishu / WeChat / Discord / ...)
     └─→ inbound protocol (OAI / A2A / ACP / WebSocket v3 / REST API)
            ▼
        github-rsclaw (THIS REPO) ── Crab AI agent
            │
            ▼ (when agent calls an LLM)
        ProviderRegistry → FailoverManager → LlmProvider (internal)
            │
            └─ rsclaw provider → rsclaw-server → rsclaw-llm GPU fleet
            └─ openai / anthropic / gemini / 30+ external providers
```

---

## Repository layout

```
src/
  agent/      Agent runtime, memory, tool dispatch, loop detection,
              compaction, evolution, meditation, prompt builder
  channel/    13 channels: Telegram WeChat Feishu DingTalk QQ Discord Slack
                            WhatsApp Signal LINE Zalo Matrix WeCom
  config/     JSON5 loader, schema, runtime resolution
  gateway/    Startup orchestration, hot-reload, channel wiring,
              ProviderRegistry construction
  server/     Axum HTTP + REST + OpenAI-compatible endpoints +
              A2A + ACP + WebSocket v3
  provider/   LLM providers (internal): OpenAI Anthropic Gemini DeepSeek
              Qwen Ollama + 25+ OAI-compat + rsclaw (kvCacheMode=2) + failover
  store/      redb KV + tantivy full-text + hnsw_rs vector
  ws/         WebSocket protocol v3 (OpenClaw-compatible)
  acp/        Agent Client Protocol
  a2a/        Google A2A v0.3 (cross-network agent collaboration)
  mcp/        MCP client (stdin/stdout JSON-RPC)
  plugin/     Shell bridge, hook registration
  skill/      Skill loader, ClawHub/SkillHub client
  cron/       Cron scheduler and delivery
  browser/    CDP headless Chrome automation
  computer/   computer_use driver (enigo input synthesis)
  cmd/        CLI commands
  cli/        CLI argument parsing
  events.rs   Global event bus
  i18n.rs     10 languages: cn en ja ko th vi fr de es ru
  hooks/      Hook registration
  heartbeat/  Heartbeat engine
  migrate/    Schema migrations

ui/
  app/        Next.js 15 chat UI + control panel (NextChat-derived)
  src-tauri/  Tauri v1 desktop shell + Rust commands

tests/        Integration tests (one file per module)
scripts/      Build, orchestration pipelines, install scripts
docs/         interfaces/ · ui-specs/ · adr/ · reviews/ · ROADMAP.md
.claude/      roles/  ← sub-agent role definitions
```

**Stack:** Rust 2024 · MSRV 1.91 · Tokio · Axum · Tauri v1 · Next.js 15 · redb · tantivy · JSON5

---

## Active role system

This project uses role-based AI sub-agents. Each role has a strict scope.
Role files live in `.claude/roles/`. Activate with:

```bash
./scripts/switch-role.sh <role>
# Or manually:  cp .claude/roles/<role>.md CLAUDE.md
```

| Role | Scope | Can Write |
|------|-------|-----------|
| `architect` | Requirements → interface definitions | `docs/` only |
| `backend-dev` | Rust implementation | `src/` |
| `ui-dev` | Frontend implementation | `ui/` |
| `backend-tester` | Rust test coverage | `tests/` |
| `ui-tester` | Frontend test coverage | `ui/test/` |
| `reviewer` | Rust code review | `docs/reviews/` |
| `design-reviewer` | UI/UX review | `docs/reviews/` |
| `qa-lead` | Quality gate, merge approval | PR description |

**Cross-role rule:** Never write outside your assigned scope. If a gap
requires touching another scope, file a handoff note in `docs/interfaces/`
and stop.

If no specific role is active, you are the **general assistant** — may
read anything but should not write implementation code without first
checking the relevant role file in `.claude/roles/`.

**Pipelines:**

```bash
./scripts/parallel-feature.sh <feature-name>   # full dev cycle
./scripts/review-pipeline.sh <branch-name>     # review + QA gate only
./scripts/parallel-channels.sh <ch1> <ch2>     # multiple channels at once
```

---

## Working principles

These apply before any code is written. **For trivial tasks, use judgment;
for non-trivial work, follow them strictly.**

### Think before coding

Don't assume. Don't hide confusion. Surface tradeoffs.
- State assumptions explicitly. If uncertain, ask.
- Multiple interpretations exist? Present them — don't pick silently.
- Simpler approach exists? Say so. Push back when warranted.
- Something unclear? Stop. Name what's confusing. Ask.

### Read before proposing

The most expensive failure mode on this codebase is "I'll propose
architecture based on what I remember." **Don't.** Open the file, grep
the codebase, read recent commits, then speak. Pattern-matching from
memory has burned multiple sessions of work.

### Simplicity first

Minimum code that solves the problem. Nothing speculative.
- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- 200 lines that could be 50? Rewrite.
- Test: "Would a senior engineer call this overcomplicated?" If yes, simplify.

### Surgical changes

Touch only what you must. Clean up only your own mess.
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style even if you'd do it differently.
- Notice unrelated dead code? Mention it — don't delete it.
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.
- Test: every changed line should trace directly to the user's request.

### Goal-driven execution

Define success criteria. Loop until verified.
- "Add validation" → "write tests for invalid inputs, then make them pass"
- "Fix the bug" → "write a test that reproduces it, then make it pass"
- "Refactor X" → "ensure tests pass before AND after"

For multi-step tasks state a brief plan with verifications:

```
1. [step] → verify: [check]
2. [step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make
it work") require constant clarification.

---

## Coding standards

### Rust

```
- Edition: Rust 2024. async fn in traits is native — never use the async-trait macro.
- BoxFuture is required only for `dyn Trait` dispatch.
- No .unwrap() in production paths. Use ? or .expect("explanation"). Tests are exempt.
- No silent error discard (`let _ = ...`). If best-effort, log with warn!.
- All `pub fn` must have a doc comment.
- No emojis in code, comments, logs, or commit messages.
- All user-facing strings through src/i18n.rs (channels, UI). NOT for LLM-facing prompts.
- LLM-facing prompts (system messages, tool descriptions, summarize/analyze prompts,
  agent instructions) are ALWAYS English. Hardcoded English literals in source.
  English instruction-following is consistently strongest across providers.
- Config fields: camelCase in JSON5, snake_case in Rust via #[serde(rename_all = "camelCase")].
- Secrets: SecretOrString — plain string or { source: "env", id: "VAR_NAME" }.
- Channel handler order: group policy → DM policy (pairing/allowlist) → per-user queue → agent dispatch.
- New WebSocket events MUST be registered in events.rs before use.
- Never modify Cargo.lock unless explicitly upgrading a dependency.
- NEVER add `Co-Authored-By:` lines to commits (memory rule).
```

### Frontend (ui/)

```
- Tauri invoke: window.__TAURI__?.invoke (v1 API, NOT core?.invoke).
- Hooks: all declared before any early return.
- Data fetching: never fetch() inside components — use hooks or store.
- WebSocket: all WS logic through ui/src/hooks/useRsClawSocket.ts.
- Config (desktop): Tauri commands read_config_file / write_config, not gateway API.
- Auth token priority: gateway.auth.token config > RSCLAW_AUTH_TOKEN env > localStorage.
- Components: Container (data) + Presenter (render) separation for complex views.
- Styling: Tailwind utility classes only, no inline style, no hardcoded color values.
- State: every async component handles loading / error / empty.
```

### WebSocket (ws/)

```
- event:chat must be broadcast to ALL operator connections, not just initiating connection.
- Connection states: connecting → connected → disconnected → reconnecting → error.
- UI must reflect all 5 states with distinct feedback.
- Operator connections are separate from user connections — different hook instances.
```

### Build

```
- Env vars required: RSCLAW_BUILD_VERSION, RSCLAW_BUILD_DATE.
- Tag before build, not after.
- Dev gateway flag: --dev (port 18889).
- Test command: RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test.
- Cross-compile targets:
    aarch64-apple-darwin       x86_64-apple-darwin
    x86_64-unknown-linux-musl  aarch64-unknown-linux-musl
    x86_64-pc-windows-msvc     aarch64-pc-windows-msvc
- Default branch: dev. main is for releases only. NEVER push without explicit user approval.
- Every release needs BOTH `vX.Y.Z` (CLI) and `app-vX.Y.Z` (desktop) tags pushed together.
```

---

## Key entry points

| What you want to find | Where to look |
|---|---|
| Agent tool dispatch | `src/agent/runtime.rs` |
| Agent loop / heartbeat / meditation | `src/agent/`, `src/heartbeat/` |
| Compaction (KV-cache-aware) | `src/agent/compaction.rs` |
| Channel handler pattern | `src/channel/telegram.rs` (reference impl) |
| WebSocket protocol v3 | `src/ws/` |
| All HTTP endpoints | `src/server/` |
| OpenAI-compat inbound | `src/server/openai.rs` (or similar) |
| A2A / ACP inbound | `src/a2a/`, `src/acp/` |
| LLM provider registry | `src/gateway/providers.rs` (entry), `src/provider/registry.rs` |
| LLM provider trait | `src/provider/mod.rs` (LlmProvider, LlmRequest, kv_cache_mode) |
| rsclaw-server provider (kvCacheMode=2) | `src/provider/rsclaw.rs` |
| Provider failover orchestrator | `src/provider/failover.rs` |
| Config schema | `src/config/schema.rs` |
| Global event bus | `src/events.rs` |
| UI control panel | `ui/app/components/rsclaw-panel.tsx` |
| Tauri commands | `ui/src-tauri/src/main.rs` |

---

## Key patterns

### i18n (Internationalisation)

All user-facing strings sent through channels (Telegram, Discord, WeChat,
etc.) **must** go through `src/i18n.rs`. CLI output, log messages, and
internal errors stay in English.

**Critical rule:** strings the LLM reads (system prompts, tool
descriptions, summarize/analyze prompts, agent instructions) are NOT
i18n'd. They live as English-only literals in source. Different language
per LLM call would drift over time and English instruction-following is
consistently strongest across providers.

**Adding a new message key:**

```
1. Open src/i18n.rs
2. Add a msg!() block inside the MESSAGES LazyLock initialiser (before `m`):

   msg!("my_key",
       "en" => "English text",        // required
       "zh" => "中文文本",              // required
       "th" => "ข้อความภาษาไทย",
       "vi" => "Van ban tieng Viet",
       "ja" => "日本語テキスト",
       "es" => "Texto en español",
       "ko" => "한국어 텍스트",
       "ru" => "Русский текст",
       "fr" => "Texte français",
       "de" => "Deutscher Text",
   );

3. Use in Rust:
   - Static:      crate::i18n::t("my_key", lang)
   - With params: crate::i18n::t_fmt("my_key", lang, &[("param", value)])
   - Params in template strings use {param} syntax.

4. Get `lang` from config inside run_turn (already set as i18n_lang),
   or derive inline for tool handlers:
       let lang = self.config.raw.gateway.as_ref()
           .and_then(|g| g.language.as_deref())
           .map(crate::i18n::resolve_lang)
           .unwrap_or("en");
   For notification-only code outside a turn (e.g. background spawns),
   capture lang_bg = lang before tokio::spawn and move it into the closure.
```

**Rules:**
- Supported languages: `en`, `zh`, `th`, `vi`, `ja`, `es`, `ko`, `ru`, `fr`, `de` (10 total; `json` is debug-only).
- Minimum required per key: `en` + `zh`. Add the rest as available.
- Keys are snake_case, grouped by feature (prefix: `acp_`, `cli_`, etc.).
- Desktop/system Notification strings use `crate::i18n::default_lang()`.
- Never hardcode Chinese (or any non-English) text outside i18n.rs.
- Review gate: `[BLOCK]` if user-facing string is hardcoded in channel code.

### Adding a new channel

```
1. src/channel/{name}.rs              — implement Channel trait
2. src/config/schema.rs               — add config struct with #[serde(flatten)] pub base: ChannelBase
3. src/gateway/startup.rs             — add start_{name}_if_configured()
4. Wire DM policy enforcer            — pairing / allowlist / open / disabled
5. ui/app/components/rsclaw-panel.tsx — add to channel list
6. tests/channel_{name}.rs            — create test file (even if skeleton)
```

### Adding a new tool

```
1. src/agent/runtime.rs               — add ToolDef in build_tool_list()
2. src/agent/runtime.rs               — add dispatch case in tool match block
3. src/agent/runtime.rs               — implement tool_{name}() method
```

### Adding a new LLM provider

```
1. src/provider/{name}.rs             — implement LlmProvider trait
2. src/provider/mod.rs                — export it
3. src/gateway/providers.rs           — add registration block (config-driven + env-var fallback)
4. ui/app/components/onboarding.tsx   — add to ALL_PROVIDERS for UI
```

---

## Browser automation

Use `rsclaw browser` for web automation. Run `rsclaw browser --help` for
all commands.

**Core workflow:**

```
1. rsclaw browser open <url>      # navigate to page
2. rsclaw browser snapshot -i     # get interactive elements with refs (@e1, @e2)
3. rsclaw browser click @e1       # interact using refs
   rsclaw browser fill @e2 "text"
4. Re-snapshot after page changes
```

---

## Dev commands

```bash
# Backend
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo build
cargo run -- --dev                          # port 18889
cargo run -- gateway restart                # restart gateway (NOT `rsclaw gateway restart`)
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test

# macOS only — wraps `cargo build` and re-signs the binary with a stable
# codesign identifier so Accessibility / Input Monitoring grants survive
# rebuilds. Otherwise computer_use's enigo input synthesis fails with
# "the application does not have the permission to simulate input"
# after every rebuild. Use this instead of `cargo build` for
# computer_use development.
bash scripts/dev-build.sh
bash scripts/dev-build.sh --release

# Frontend
cd ui && yarn dev
cd ui && yarn tsc --noEmit
cd ui && yarn test

# Lint
cargo clippy -- -D warnings
cd ui && yarn lint
```

**Use debug builds during development.** Release builds are reserved for
publishing. Don't waste time on release builds when iterating.

---

## Review standards

Reviewers output to `docs/reviews/[branch].md` using these tags:

| Tag | Meaning |
|---|---|
| `[BLOCK]` | Must fix before merge |
| `[SUGGEST]` | Recommended improvement |
| `[NOTE]` | Non-blocking observation |

**Auto-BLOCK triggers (Rust):**
- `unwrap()` without explanation
- silent error discard (`let _ = ...`)
- new WS event not registered in `events.rs`
- `pub fn` missing doc comment
- channel change with no corresponding test file

**Auto-BLOCK triggers (UI):**
- hardcoded color values
- operation with no loading feedback
- WS disconnect not disabling input
- breaking change without confirm dialog

---

## QA gate (qa-lead only)

Merge only when ALL of the following pass:

```
Backend
  □ docs/reviews/[branch].md — zero [BLOCK] items
  □ cargo test --all passes
  □ New features have tests/ coverage
  □ events.rs has no orphaned events

Frontend
  □ docs/reviews/ui-[branch].md — zero [BLOCK] items
  □ yarn test passes
  □ yarn tsc --noEmit passes
  □ WS state machine: all 5 states tested

Docs
  □ API changes reflected in docs/interfaces/
  □ Architecture decisions recorded in docs/adr/
  □ README / AGENTS.md updated if needed
```

If any QA check is ambiguous or a breaking change touches `ws/` or
`provider/`, **stop and wait for human decision**.

---

## Inbound API reference

| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/message` | POST | Send message to agent |
| `/api/v1/agents` | GET / POST | List / create agents |
| `/api/v1/agents/{id}` | PATCH / DELETE | Update / delete agent |
| `/api/v1/sessions` | GET | List sessions |
| `/api/v1/channels/pair` | POST | Approve pairing code |
| `/api/v1/channels/pairings` | GET | List pairings |
| `/api/v1/config` | GET / PUT | Read / write config |
| `/api/v1/health` | GET | Health check |
| `/v1/chat/completions` | POST | OpenAI-compatible inbound |
| `/v1/models` | GET | OpenAI model list |
| `/.well-known/agent.json` | GET | A2A Agent Card |
| `/api/v1/a2a` | POST | A2A JSON-RPC tasks |

(WebSocket v3 + ACP endpoints are separate, see `src/ws/` and `src/acp/`.)

---

## Three-repo handoff rules

When designing a feature, decide which repo it belongs in **before writing
code**:

| Concern | Belongs in |
|---|---|
| User-visible inbound protocol (Anthropic-shape API, etc.) | **github-rsclaw** `src/server/` (THIS repo) |
| New external LLM provider | **github-rsclaw** `src/provider/` (THIS repo) |
| Agent loop behavior (heartbeat, meditation, tools, compaction) | **github-rsclaw** `src/agent/` (THIS repo) |
| Channels (Telegram, Feishu, etc.) | **github-rsclaw** `src/channel/` (THIS repo) |
| GPU fleet health (auto-drain, retry, version drift, session_diverged) | **rsclaw-server** |
| Worker engine (kvCacheMode=2 protocol, prefill, KV slot mgmt) | **rsclaw-llm** |
| Multi-tenant credential vault / cross-provider failover orchestrator (CUSTOMER-FACING) | **NONE of these repos** — flag as gateway product, requires separate decision |

If a feature feels like it belongs to a "LLM gateway product" rather than
to "the agent" or "the GPU fleet," it probably doesn't belong in any of
these three repos. Pause, name it, and decide before coding.

---

## When you are unsure

1. Check `docs/interfaces/` for the relevant module — the contract is there.
2. Check `docs/adr/` for decisions that explain *why* something works
   the way it does.
3. Check recent commits with `git log --oneline -50` — non-obvious
   behaviors are usually explained in commit bodies.
4. Check the user's auto-memory in
   `~/.claude/projects/-Users-oopos-dev-github-rsclaw/memory/` —
   `project_three_repo_topology.md` is the anti-amnesia anchor.
5. If still unclear, write a question to
   `docs/interfaces/open-questions.md` and stop. **Do not guess** on
   architecture or protocol decisions.

---

## License

MIT OR Apache-2.0 dual license. Contributions are dual-licensed under the
same terms (see README).
