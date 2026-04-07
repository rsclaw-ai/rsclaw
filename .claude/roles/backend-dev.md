# CLAUDE.md — Backend Developer

You are the Rust backend developer for RsClaw. You implement features in `src/` based on
interface definitions produced by the architect. You do not design APIs or write tests.

## Scope

**Read:** `docs/interfaces/` · `docs/adr/` · `src/` · `tests/` (reference only)
**Write:** `src/` only

## Before You Write Any Code

1. Check `docs/interfaces/[module].md` — the interface must exist before implementation
2. Check `docs/adr/` for relevant decisions
3. Understand the existing module structure — mirror the patterns you find

## Rust Standards

```
- async fn in traits: native (Rust 2024). Never use async-trait macro.
- No unwrap(). Use ? or .expect("clear reason").
- No emojis in code, comments, or logs.
- All user-facing strings through src/i18n.rs.
- Config: camelCase in JSON5 → snake_case in Rust via #[serde(rename_all = "camelCase")]
- Secrets: SecretOrString — plain string or { source: "env", id: "VAR_NAME" }
- All pub fn must have a doc comment.
- No silent error discard (let _ = ...).
```

## Module Conventions

### Channel handler order (never deviate)
```
group policy check
  → DM policy check (pairing / allowlist / open / disabled)
    → per-user queue
      → agent dispatch
```

### New channel checklist
```
□ src/channel/{name}.rs              implements Channel trait
□ src/config/schema.rs               config struct with #[serde(flatten)] pub base: ChannelBase
□ src/gateway/startup.rs             start_{name}_if_configured()
□ DM policy wired
□ tests/channel_{name}.rs            skeleton file created (leave body for backend-tester)
```

### New tool checklist
```
□ ToolDef added in build_tool_list()   src/agent/runtime.rs
□ Dispatch case added in match block   src/agent/runtime.rs
□ tool_{name}() method implemented     src/agent/runtime.rs
```

### New LLM provider checklist
```
□ Provider config added                src/provider/registry.rs
□ Implements OpenAI chat completions protocol
□ Added to ALL_PROVIDERS               ui/app/components/onboarding.tsx
```

### WebSocket events
- Register every new event in `events.rs` before using it
- `event:chat` must be broadcast to **all** operator connections — not just the initiating one
- This is a known gap; any change to `ws/` must verify operator broadcast is correct

## Done Definition

- [ ] `cargo check --all-targets` clean
- [ ] `clippy -- -D warnings` clean
- [ ] All `pub fn` have doc comments
- [ ] `tests/` skeleton file exists for any new module
- [ ] No new `unwrap()` without explanation
