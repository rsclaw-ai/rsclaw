# CLAUDE.md — RsClaw

You are working on RsClaw, a high-performance Rust-native AI agent gateway.
This is the root context. Read it fully before taking any action.

## What is RsClaw

RsClaw is a ground-up Rust rewrite of OpenClaw (TypeScript/Node.js) with full protocol
compatibility. It runs as a local gateway, a server daemon, or a Tauri desktop app.

Core responsibilities:
- Route messages across 13 messaging channels to AI agent runtimes
- Speak OpenAI-compatible, A2A v0.3, ACP, and WebSocket v3 protocols
- Manage LLM providers with failover
- Store conversation history (redb KV + tantivy + hnsw_rs vector)

## Repository Layout

```
src/          Rust backend (single crate, modular)
ui/           Next.js 15 frontend + Tauri v1 desktop shell
tests/        Integration tests (one file per module)
scripts/      build scripts, orchestration pipelines, install scripts
docs/         interfaces/ · ui-specs/ · adr/ · reviews/
.claude/      roles/  ← sub-agent role definitions
```

## Active Role System

This project uses role-based sub-agents. Each role has a strict write scope.
The active role for this session is determined by which role file was copied here.

If no specific role is active, you are the **general assistant** for this project.
As general assistant you may read anything but should not write implementation code
without first checking the relevant role file in `.claude/roles/`.

To activate a role:
```bash
./scripts/switch-role.sh <role>
# roles: architect · backend-dev · ui-dev · backend-tester
#        ui-tester · reviewer · design-reviewer · qa-lead
```

To run a full pipeline:
```bash
./scripts/parallel-feature.sh <feature-name>   # full dev cycle
./scripts/review-pipeline.sh <branch-name>     # review + QA gate only
./scripts/parallel-channels.sh <ch1> <ch2>     # multiple channels at once
```

## Working Principles

Behavioral guidelines that apply before any code is written. These bias toward
caution over speed — for trivial tasks, use judgment.

### Think Before Coding
Don't assume. Don't hide confusion. Surface tradeoffs.
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them — don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### Simplicity First
Minimum code that solves the problem. Nothing speculative.
- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.
- Test: "Would a senior engineer call this overcomplicated?" If yes, simplify.

### Surgical Changes
Touch only what you must. Clean up only your own mess.
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it — don't delete it.
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.
- Test: Every changed line should trace directly to the user's request.

### Goal-Driven Execution
Define success criteria. Loop until verified.
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work")
require constant clarification.

## Non-Negotiable Rules (apply to every role)

```
Rust
  - async fn in traits: native Rust 2024. Never use async-trait macro.
  - No unwrap(). Use ? or .expect("reason").
  - No emojis in code, comments, logs, or commit messages.
  - All user-facing strings through src/i18n.rs.
  - All pub fn must have a doc comment.
  - New WebSocket events must be registered in events.rs before use.
  - No silent error discard (let _ = ...).

Frontend
  - Tauri invoke: window.__TAURI__?.invoke  (v1 — never core?.invoke)
  - All hooks declared before any early return.
  - Never fetch() inside components.
  - All WebSocket logic through ui/src/hooks/useRsClawSocket.ts.
  - No hardcoded color values. No inline style attributes.
  - Every async component handles loading / error / empty.

General
  - Never commit secrets or tokens.
  - Tag before build, not after.
  - Never modify Cargo.lock unless explicitly upgrading a dependency.
  - Never delete files in tests/ without explicit instruction.
```

## Key Entry Points

| What you want to find | Where to look |
|-----------------------|---------------|
| Agent tool dispatch | `src/agent/runtime.rs` |
| Channel handler pattern | `src/channel/telegram.rs` (reference impl) |
| WebSocket protocol | `src/ws/` |
| All HTTP endpoints | `src/server/` |
| Config schema | `src/config/schema.rs` |
| Global event bus | `src/events.rs` |
| UI control panel | `ui/app/components/rsclaw-panel.tsx` |
| Tauri commands | `ui/src-tauri/src/main.rs` |

## Browser Automation

Use `agent-browser` for web automation. Run `agent-browser --help` for all commands.

Core workflow:

1. `agent-browser open <url>` - Navigate to page
2. `agent-browser snapshot -i` - Get interactive elements with refs (@e1, @e2)
3. `agent-browser click @e1` / `fill @e2 "text"` - Interact using refs
4. Re-snapshot after page changes

## Dev Commands

```bash
# Backend
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo build
cargo run -- --dev                          # port 18889
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test

# Frontend
cd ui && yarn dev
cd ui && yarn tsc --noEmit
cd ui && yarn test

# Lint
cargo clippy -- -D warnings
cd ui && yarn lint
```

## When You Are Unsure

1. Check `docs/interfaces/` for the relevant module — the contract is there.
2. Check `docs/adr/` for decisions that explain *why* something works the way it does.
3. Check `AGENTS.md` for team-wide coding standards and key patterns.
4. If still unclear, write a question to `docs/interfaces/open-questions.md` and stop.
   Do not guess on architecture or protocol decisions.

## License

MIT OR Apache-2.0 dual license. Contributions are dual-licensed under the same terms (see README).
