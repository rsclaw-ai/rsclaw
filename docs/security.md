# Security: sender trust, owners and hardening settings

This page describes who may make an RsClaw agent do what, the config keys that
control it, and the environment variables that relax specific safety checks.
All config keys live in `rsclaw.json5` (camelCase).

## Sender trust model

Channel membership (`dmPolicy`, `allowFrom`, pairing, `groupPolicy`) decides
who may **talk** to an agent. Sender trust decides what a sender may make the
agent **do**. There are two levels:

| Level | Who |
|-------|-----|
| **Owner** | Local entry points (desktop app, WebSocket operator, CLI, loopback HTTP, cron/heartbeat jobs created by owners); DM senders listed literally (not via `*`) in a channel's static `allowFrom` (top level or any `accounts.<name>.allowFrom`); identities listed in `gateway.owners`. |
| **User** | Everyone else: paired users, group members, A2A peers, webhook callers. |

**Paired users are not owners.** Approving a pairing code lets a person chat
with the agent; it does not give them shell or file access. If you pair your
own Telegram / Feishu / WeChat account and want full access from it, add it
to `gateway.owners`.

### `gateway.owners`

```json5
{
  gateway: {
    // "<channel>:<peer_id>" — base channel name, no account suffix
    owners: ["telegram:123456789", "feishu:ou_abc123"],
  },
}
```

- Entries in `gateway.owners` count in group chats too (static `allowFrom`
  entries only make a sender an owner in DMs).
- Changes are hot-reloaded; no gateway restart is needed.
- If no owner is configured for any channel, the gateway logs a hint at
  startup: messaging-channel senders then cannot use owner-only capabilities.

Ways to edit it without hand-editing the file:

```bash
rsclaw channels pair 45GA-KP42 --owner        # approve a code and make that peer an owner
rsclaw channels owner add telegram 123456789
rsclaw channels owner remove telegram 123456789
rsclaw channels owner list [telegram]
```

In the desktop app, open **Control panel -> Pairing**: pending codes have an
**Approve as owner** button, every approved peer has an **Owner** toggle, and
the **Owners** section lists and removes current `gateway.owners` entries.

### Owner-only tools

Non-owners cannot call these tools (aliases included): host command
execution (`exec`, `shell`, `execute_command`, `anycli`, `opencli`, `cap*`,
`install_tool`), host file mutation and exfiltration (`write_file`,
`edit_file`, `web_download`, `send_file`), desktop and logged-in browser
control (`computer_use`, `web_browser`, `browser`), scheduling and
cross-session / cross-agent control (`cron`, `session`, `sessions_*`,
`agent`, `subagents`, `agent_create`, `send_message`, `message`), gateway and
channel administration (`gateway`, `pairing`, `channel`, `*_actions`), and
skill management (`skill_install`, `skill_remove`).

To let non-owners use some of them on a specific agent, list them in
`agents.list[].nonOwnerTools`:

```json5
{
  agents: {
    list: [
      // Public-facing agent: paired users may send files but nothing else risky.
      { id: "helpdesk", nonOwnerTools: ["send_file"] },
      // Lift the restriction entirely for this agent (not recommended).
      { id: "lab", nonOwnerTools: ["*"] },
    ],
  },
}
```

### Owner-only slash commands

Local commands that act on the host are refused for non-owners before they
run: `! <cmd>`, `$ <cmd>`, `/run`, `/sh`, `/exec`, `/ls`, `/cat`, `/ss`,
`/screenshot`, `/webshot`, `/cap`, `/cap-exit`, `/cap-resume`, `/cron`,
`/loop`, `/watch`, `/skill`, `/model`, `/remember`, `/recall`.

Session commands act on exactly the session the message belongs to: `/clear`,
`/new`, `/abort` and the `/cap` sticky binding (`/cap`, `/cap-resume`,
`/cap-exit`) use the same session key the message would be dispatched under
(group and `dmScope` aware). Other sessions of the same sender, and
other groups, are not touched.

### What owners get beyond the tool list

- File writes are confined to the agent workspace. Owners may additionally
  write inside the directories listed in `RSCLAW_WRITE_ROOTS` (see below).
  Owners with `tools.exec.safety` turned off keep unrestricted writes.
- `web_fetch` and the other web tools never reach private or loopback
  addresses unless the sender is an owner **and**
  `RSCLAW_WEB_FETCH_ALLOW_PRIVATE=1`.
- Only owners on local entry points see and change every cron job; owners
  on messaging channels only see the jobs they created.

## Channel policy changes

### Webhook secrets are now required

Inbound webhooks verify signatures. Without the secret, the webhook endpoint
is disabled (outbound sending keeps working) instead of accepting forged
events:

| Channel | Required key | Endpoint disabled without it |
|---------|--------------|------------------------------|
| WhatsApp Cloud API | `channels.whatsapp.accounts.<name>.appSecret` | `/hooks/whatsapp` |
| LINE | `channels.line.channelSecret` (or `accounts.<name>.channelSecret`) | `/hooks/line` |
| Feishu / Lark (HTTP webhook mode) | `channels.feishu.verificationToken` and/or `encryptKey` (top level or per account) | `/hooks/feishu` |

Feishu in WebSocket mode needs neither key. Zalo still accepts
unsigned webhooks without `oaSecret` but logs a warning; set
`channels.zalo.oaSecret` to enable verification.

WhatsApp's subscription handshake (`GET /hooks/whatsapp`, used by Meta when
you register the webhook) also needs a verify token:
`channels.whatsapp.accounts.<name>.verifyToken`, or the `WHATSAPP_VERIFY_TOKEN`
environment variable. Without one the handshake is refused with 403 and Meta
cannot register the webhook.

The generic `hooks` endpoint (`/hooks/<path>` via `hooks.mappings`) and custom
webhook channels (`channels.custom[]` with `type: "webhook"`) refuse every
request (HTTP 503) when `hooks.token` is not configured or resolves to an empty
value. Callers must send the token in `X-Hook-Token` or
`Authorization: Bearer <token>`.

DingTalk access tokens are fetched from the v1.0 endpoint
(`POST /v1.0/oauth2/accessToken`): the app secret travels in the request body
and never appears in a URL or in transport-error logs.

### `groupPolicy` defaults

Custom channels (`channels.custom[]`) and WeCom now default to
`groupPolicy: "allowlist"`. Group messages are dropped unless the group id is
listed in `groupAllowFrom` (or `groupAllowFrom` contains `"*"`). To keep the
old behaviour, set it explicitly:

```json5
{ channels: { wecom: { groupPolicy: "open" } } }
```

### Background command limit

`tools.exec.maxBackground` caps how many background `exec` commands may run at
once across the gateway (default `4`, `0` = unlimited; read at startup). When
the cap is reached, a new background command is refused with an error telling
the agent to wait for a running one to finish or to run the command in the
foreground.

```json5
{ tools: { exec: { maxBackground: 2 } } }
```

## Environment variables

| Variable | Effect |
|----------|--------|
| `RSCLAW_WRITE_ROOTS` | Extra absolute directories owners may write to, in OS path-list syntax (`:`-separated on macOS/Linux, `;` on Windows). Relative entries are ignored. |
| `RSCLAW_WEB_FETCH_ALLOW_PRIVATE` | `1` or `true` lets owners fetch private / loopback URLs (local dev servers). Never applies to non-owners. |
| `RSCLAW_ALLOWED_HOSTS` | Comma-separated extra `Host` header names the gateway accepts while bound to loopback (DNS-rebinding guard). `localhost`, loopback IPs and `tauri.localhost` are always accepted. |
| `RSCLAW_EXEC_PASS_ENV` | Comma-separated env var names that commands run by the agent may inherit even though they look secret (`*_TOKEN`, `*_KEY`, `DATABASE_URL`, ...). Such variables are stripped by default; names set in the config `env` block are kept too. |
| `RSCLAW_WATCH_*` | `/watch` SSE URLs and headers only expand `${VAR}` references whose name starts with `RSCLAW_WATCH_` (e.g. `${RSCLAW_WATCH_EVENTS_TOKEN}`); any other `${VAR}` is left verbatim, so `/watch` cannot leak gateway secrets such as provider API keys. |

## Plugins

### Capability declarations

WASM plugins must declare sensitive host capabilities in `plugin.json5`;
calling a gated host function without the declaration returns an error
instead of performing the action:

```json5
{
  name: "myplugin",
  capabilities: ["http", "cron"],
}
```

| Capability | Gates |
|------------|-------|
| `http` | Outbound HTTP (public addresses only) |
| `device` | Per-plugin device identity key and signatures |
| `cron` | Registering background cron jobs |
| `sse` | Server-sent-event subscriptions |
| `pushOutbound` | Sending messages to arbitrary peers |
| `submitAgentTurn` | Injecting prompts into agent sessions |
| `desktop` | Screen capture, clipboard, mouse / keyboard synthesis |
| `vlmDrive` | Desktop / Android VLM drive loops, limited to the app the plugin names |
| `vlmDriveBypass` | Lets a VLM drive loop act on any app (grant explicitly) |

### Env references in plugin manifests

A manifest `config` block may read environment variables with
`{ source: "env", id: "VAR" }`, but only inside the plugin's own namespace:

- `RSCLAW_PLUGIN_<NAME>_*` (always allowed), and
- `<NAME>_*` (for example `ASTOCK_*` for a plugin named `astock`), unless
  `<NAME>`, or its first `_`-separated segment, is a reserved namespace.

`<NAME>` is the plugin name upper-cased with non-alphanumerics replaced by
`_`. The reserved namespaces are the host's own and well-known provider /
tooling prefixes: `RSCLAW`, `OPENAI`, `ANTHROPIC`, `CLAUDE`, `AWS`, `AZURE`,
`GOOGLE`, `GEMINI`, `GCP`, `GITHUB`, `GH`, `GITLAB`, `DEEPSEEK`, `DOUBAO`,
`ARK`, `VOLC`, `QWEN`, `DASHSCOPE`, `MOONSHOT`, `KIMI`, `ZHIPU`, `MINIMAX`,
`GROQ`, `MISTRAL`, `XAI`, `OPENROUTER`, `HF`, `HUGGINGFACE`, `SSH`, `GPG`,
`NPM`, `CARGO`, `DOCKER`, `KUBE`, `DATABASE`, `PG`, `MYSQL`, `REDIS`, `HOME`,
`PATH`, `USER`, `LD`, `DYLD`. So a plugin named `openai` or `github-tools`
(`GITHUB_TOOLS`) only gets `RSCLAW_PLUGIN_<NAME>_*`. A denied reference
resolves to `null` and is logged. To pass any other
secret to a plugin, set it under `plugins.entries.<name>.config` in
`rsclaw.json5`.

## Device tokens

WebSocket device tokens are now bound to the gateway auth token, expire after
30 days, and are only minted when gateway auth is configured. Tokens issued by
earlier versions (and all tokens after you rotate or remove
`gateway.auth.token`) are rejected: paired WebSocket / mobile clients must log
in again once after upgrading.

## CLI secrets

Flags that take a gateway token or password (`--token`, `--password`,
`--remote-token`, `--gateway-token` on `gateway`, `qr`, `security audit`,
`setup` / `onboard`, `logs`, `tui`) also read their environment fallback
(`RSCLAW_AUTH_TOKEN`, `RSCLAW_GATEWAY_PASSWORD`, `RSCLAW_REMOTE_TOKEN`).
Pass `-` as the value to read the secret from stdin instead, so it never
appears in argv or shell history:

```bash
rsclaw qr --token - < ~/.config/rsclaw-token
```
