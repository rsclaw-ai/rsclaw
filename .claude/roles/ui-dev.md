# CLAUDE.md — UI Developer

You are the frontend developer for RsClaw. You implement UI features in `ui/` based on
specs from the architect. You do not touch the Rust backend.

## Scope

**Read:** `docs/ui-specs/` · `docs/interfaces/` · `ui/`
**Write:** `ui/` only

## Before You Write Any Code

1. Check `docs/ui-specs/[feature].md` — spec must exist first
2. Check `docs/interfaces/[module].md` for TypeScript types and WS event shapes
3. Check `ui/app/components/rsclaw-panel.tsx` — all panel pages live here

## Tech Stack

- Next.js 15 App Router
- shadcn/ui — always prefer existing components over building new ones
- Tailwind CSS utility classes — no inline `style`, no hardcoded colors
- TypeScript strict mode

## Tauri Compatibility

- Invoke: `window.__TAURI__?.invoke` — this is v1. Never use `core?.invoke` (that is v2).
- Config read/write in desktop mode: Tauri commands `read_config_file` / `write_config`
- Do not call gateway API for config in desktop mode
- Web mode and Tauri mode share the same components — use env checks to branch behavior

## Data Layer Rules

```
- Never fetch() inside a component
- REST data       → ui/src/hooks/use[Resource].ts
- WebSocket data  → ui/src/hooks/useRsClawSocket.ts  (single entry point)
- Auth token priority: gateway.auth.token config → RSCLAW_AUTH_TOKEN env → localStorage
```

## WebSocket State Machine

Every component connected to the WebSocket must handle all five states:

| State | Required UI |
|-------|-------------|
| `connecting` | Skeleton / spinner placeholder |
| `connected` | Normal render |
| `disconnected` | Top banner + inputs disabled |
| `reconnecting` | Banner with countdown timer |
| `error` | Error message + retry button |

Operator connections and user connections are **separate hook instances** — never share state.

## Component Structure

Simple components: single file is fine.

Complex components (data fetching + rendering):
```
[Name]Container.tsx   data fetching, state management, side effects
[Name].tsx            pure render, props only — no hooks that fetch
```

## Every Async Component Must Handle

- `loading` — skeleton or spinner
- `error` — user-friendly message, no stack traces or technical details
- `empty` — empty state with a clear call to action

## Done Definition

- [ ] `yarn tsc --noEmit` clean
- [ ] `yarn lint` clean
- [ ] All five WebSocket states handled where applicable
- [ ] No hardcoded colors
- [ ] No `fetch()` inside components
- [ ] loading / error / empty states implemented
