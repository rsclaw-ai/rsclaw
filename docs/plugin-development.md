# Plugin Development

rsclaw loads two plugin runtimes:

- **Shell-bridge plugins** — Node / Bun / Deno subprocesses that speak JSON-RPC over stdin/stdout. Easier to author (npm, no Rust toolchain), good for plugins that need to spawn external CLIs or use npm packages.
- **WASM plugins** — Rust crates compiled to `wasm32-wasip2`. Sandboxed, fast, suited for performance-critical or security-sensitive code.

Both runtimes share the same host capability surface (`notify`, `log`, `browser_*`, `sleep`, `storage_allocate_artifact`) and are exposed to the LLM under the same `<plugin>.<tool>` namespace. **Wasm wins on collision** — if a wasm and a shell plugin both claim the same name, the wasm one is dispatched.

This guide focuses on **shell-bridge (Node) plugins**. For wasm plugins see the rsclaw-plugins repo's `examples/default.plugin.json5`.

## Layout

A plugin lives in a directory under `~/.rsclaw/plugins/<name>/`:

```
~/.rsclaw/plugins/myplugin/
├── plugin.json5      # manifest — single source of truth for tools
├── index.mjs         # entry script (default; configurable via manifest.entry)
└── …                 # any other files your plugin needs
```

## Manifest

`plugin.json5`:

```json5
{
  name: "myplugin",
  version: "1.0.0",
  description: "What the plugin does (this string is shown to the LLM)",
  runtime: "node",            // or "bun" / "deno"
  entry: "./index.mjs",       // entry script, relative to plugin dir
  tools: [
    {
      name: "do_thing",
      description: "Does the thing — also shown to the LLM",
      inputSchema: {
        type: "object",
        properties: {
          x: { type: "string", description: "..." }
        },
        required: ["x"]
      }
    }
  ]
}
```

The manifest's `tools` array is the single source of truth for what the LLM sees. `description` and `inputSchema` go into the tool definition the model gets at request time, so write them for the LLM.

## Wire protocol

rsclaw spawns your script as a subprocess and communicates via stdin/stdout JSON-RPC, **one JSON object per line** (newline-delimited JSON).

### Inbound (rsclaw → your plugin), positive id

The id is a positive integer; reply with the same id.

**Tool call:**
```json
{"id": 1, "method": "tool_call",
 "params": {"tool": "do_thing", "args": {...}, "_ctx": {...}}}
```

Reply with one of:
```json
{"id": 1, "result": <any JSON value — your tool's output>}
{"id": 1, "error": "human-readable error message"}
```

**Hooks** (e.g., `before_message`, `after_message`) — same shape, `method` is the hook name.

### Outbound (your plugin → rsclaw), negative id

If you want to call a host method, write a request with a **negative** id and wait for the matching response.

**`notify`** — push an IM message to the conversation that invoked the tool. Pass through the `_ctx` you received from the inbound `tool_call`:
```json
{"id": -1, "method": "notify",
 "params": {"text": "your message", "_ctx": {...}}}
```
Response: `{"id": -1, "result": {"status": "dispatched" | "logged_only" | "no_receivers"}}`

- `dispatched` — sent to the IM channel.
- `logged_only` — no IM channel was wired (e.g., plugin invoked outside a chat session); the text was logged.
- `no_receivers` — the broadcast channel had no subscribers.

**`log`** — write to rsclaw's gateway log:
```json
{"id": -2, "method": "log",
 "params": {"level": "info" | "warn" | "error" | "debug", "text": "..."}}
```

**`browser_*`** — drive the rsclaw-managed CDP browser session (shared with wasm plugins, so login state persists across runtimes):
- `browser_open` — `{"url": "https://..."}`
- `browser_eval` — `{"script": "JS code returning a value"}`
- `browser_eval_with_args` — `{"fn": "(args) => {...}", "args": {...}}`
- `browser_click` — `{"ref": "<element ref from snapshot>"}`
- `browser_click_at` — `{"x": 100, "y": 200}` — native CDP click at viewport coords (use this for React handlers that ignore synthetic clicks)
- `browser_fill` — `{"ref": "...", "text": "..."}`
- `browser_snapshot` — `{}` (returns an accessibility-tree text representation)
- `browser_download` — `{"url": "https://...", "dest_path": "filename.ext", "referer": "https://..." }` (referer optional)

**`sleep`** — yield to the host scheduler:
```json
{"id": -9, "method": "sleep", "params": {"ms": 1500}}
```

**`storage_allocate_artifact`** — allocate a canonical download path (the host owns the on-disk shape; pass a hint filename whose extension drives the category):
```json
{"id": -10, "method": "storage_allocate_artifact", "params": {"filename": "out.mp4"}}
```
Response: `{"id": -10, "result": {"path": "/Users/.../Downloads/rsclaw/video/dl_video_<ts><abc>.mp4"}}`

For multi-file outputs (e.g., a frame sequence):
```json
{"id": -11, "method": "storage_allocate_artifact",
 "params": {"filename": "frame.png", "count": 8}}
```
Response: `{"id": -11, "result": {"paths": ["...", "...", ...]}}`

### Id rule

rsclaw assigns **positive** ids; your plugin assigns **negative** ids. The two never overlap; `id == 0` is reserved (sending it is a protocol error).

## `_ctx` field

Every `tool_call` includes `params._ctx` with three fields:
```json
{"target_id": "...", "channel": "...", "session_key": "..."}
```

Pass it back when calling host methods that need to know the conversation target — currently only `notify`. The other host methods ignore it, but it's harmless to forward.

## Recommended SDK

Hand-rolling the JSON-RPC dispatch is ~30 lines of code (see `tests/fixtures/shell_plugin_echo/index.mjs`), but for production plugins use `@rsclaw/plugin-sdk` (npm) — it provides typed wrappers (`host.notify(text, ctx)`, `host.browser.open(url)`, etc.) and handles id correlation and stdio dispatching for you.

## Choosing a runtime

| Choose Node when… | Choose wasm when… |
|---|---|
| You need npm packages that don't compile to wasm | Sandboxing matters (memory isolation, no filesystem by default) |
| You spawn subprocess CLIs (e.g. `flyai-cli`, `ffmpeg`) | Performance-critical hot paths |
| You want fast iteration without recompiling Rust | You're publishing to a multi-tenant deployment where you can't trust plugin code |

The host method catalog is identical between runtimes, so plugins are portable in principle — most of the porting cost is rewriting business logic between languages, not adapting the host API surface.

## Reference

- Source for the host method catalog: [`src/plugin/host_methods.rs`](../src/plugin/host_methods.rs)
- Wire protocol implementation: [`src/plugin/shell_bridge.rs`](../src/plugin/shell_bridge.rs)
- Test fixture demonstrating both directions: [`tests/fixtures/shell_plugin_echo/`](../tests/fixtures/shell_plugin_echo/)
