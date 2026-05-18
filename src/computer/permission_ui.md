# Computer-use permission gate — UI specification

Backend half lives in `src/computer/permission.rs`. This file specifies
the Tauri / Next.js half. NOT YET IMPLEMENTED — tracked here for the
ui-dev role.

## 1. WS event subscriber

`ui/app/lib/rsclaw-ws.ts`: add a handler for the new `permission_request`
event the backend will broadcast. Payload shape mirrors
`PermissionRequest` in `src/computer/permission.rs`:

```ts
type PermissionRequest = {
  request_id: string;
  agent_id: string;
  app: string;             // may be empty for generic-desktop
  reason: string;          // plain-language summary
  estimated_steps: number; // value of max_loop
};
```

On receipt, push to a global handler / context that mounts
`ComputerUsePermissionDialog`.

## 2. React component

`ui/app/components/ComputerUsePermissionDialog.tsx`. Modal with:

- **Title:** "RsClaw is about to control your computer"
- **Body:**
  - App name (`req.app`) — fall back to "your desktop" if empty
  - Instruction summary (`req.reason`)
  - Step-count estimate (`req.estimated_steps`)
- **Four buttons:**
  - "Allow once" → posts `{ request_id, decision: "allow_once" }`
  - "Allow this session" → `allow_session`
  - `Always allow for {app}` → `allow_always` (label hides the
    `for {app}` suffix when `req.app` is empty)
  - "Deny" → `deny`

Visual style: red accent — this is a security-significant prompt.
Match the existing destructive-action visual in NextChat-derived UI
(see Claude Code's "Bypass permissions" toggle for a reference).

## 3. WS method on backend

`chat.permission_response` (or whatever fits the existing WS dispatch
naming). Receives:

```json
{ "request_id": "...", "decision": "allow_once" | "allow_session" | "allow_always" | "deny" }
```

Handler must:
1. Look up the `request_id` in the gateway's
   `Arc<RedbPermissionStore>`.
2. Call `store.resolve_pending_request(request_id, decision).await`.
3. The driver future awakens, calls `record(...)`, and proceeds.

If `resolve_pending_request` returns `false`, log a warning (the
request likely timed out or was already answered) — do not error.

## 4. Bypass toggle

Add a subtle "Bypass all permissions" toggle in the gateway settings
panel for power users. Backend wiring is already in place — the
`bypass_all` flag is read from runtime config when the
`RedbPermissionStore` is constructed; flipping the toggle triggers a
gateway restart (same path as other config changes).

## Integration handoff

UI dev should mount the dialog at the top level (e.g. inside the
existing `RsClawPanel`) so it overlays whatever screen the user is
on when an agent action arrives. The dialog stays mounted but hidden
until a `permission_request` event fires.
