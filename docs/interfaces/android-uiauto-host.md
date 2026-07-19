# Android visual host interface

**Status:** Implemented in `codex/android-uiauto-host`

**Date:** 2026-07-18

**Consumers:** `wechat-android` v2 and future Android WASM plugins

## Decision

Android plugins use ClsAgent `cls android` primitives for screenshot, coordinate
input, and app activation, plus a narrowly allowlisted `cls android uiauto raw`
sidecar for native protocol operations that do not require an accessibility
tree. The host must not spawn `adb`, open a local ADB forward, or fall back to
legacy `uiautomator dump`.

This is a breaking replacement of `host-android`. The old ADB-shaped functions
and implementation are removed; callers must migrate with the host. Old Android
plugins are outside this contract and are not part of the compatibility gate.

## Configuration

The gateway process reads:

| Variable | Required | Default | Meaning |
|---|---:|---:|---|
| `RSCLAW_ANDROID_NODE` | yes | none | CLS tunnel node, for example `android-dev` |
| `RSCLAW_ANDROID_UIAUTO_PORT` | no | `6790` | Device-local UIAutomator2 HTTP port |
| `RSCLAW_CLS_BIN` | no | `cls` | Explicit CLS executable path/name |

Missing or invalid configuration fails closed with a short host error. It must
not cause an ADB fallback.

The host invokes CLS with direct process arguments, never a shell command. CLS
owns tunnel authentication, pinned host-key verification, session reuse, and
the UIAutomator2 HTTP transport.

## WIT interface

```wit
interface host-android {
/// Run one allowlisted high-level `cls android` agent operation.
/// `args-json` is an object using the CLS command's camelCase option names.
/// Returns the command's JSON object without a host-specific envelope.
android-call: func(command: string, args-json: string)
    -> result<string, string>;

/// Forward one native UIAutomator2/WebDriver JSON request through CLS.
/// Returns the server JSON body in its native shape.
android-uiauto-raw: func(method: string, path: string, json-body: option<string>)
    -> result<string, string>;
}
```

### `android-call`

The initial allowlist is:

```text
status screenshot tap swipe key launch
```

Tree/dump/find/text commands are deliberately absent. WeChat exposes only a
collapsed empty accessibility root, so production observation is screenshot
only. Customer text is sent through the raw request's private file instead of
the process argument list.

The host owns a per-command option allowlist. Unknown commands, unknown fields,
non-object arguments, NUL bytes, oversized values, and non-finite/negative
numbers for unsigned options are rejected before spawning CLS. Boolean `false`
omits a flag; boolean `true` emits the corresponding switch.

Examples:

```json
{"command":"screenshot","args":{}}
{"command":"tap","args":{"x":540,"y":2100}}
{"command":"launch","args":{"package":"com.tencent.mm"}}
```

The screenshot response also carries an additive `contactBadge` object derived
from the same decoded PNG. It reports the Contacts-tab red-badge pixel probe
(`badge`, boolean `count`, red/cluster diagnostics, and Contacts/WeChat
active-tab pixel evidence), so
a plugin can submit that one image to OCR without taking a second screenshot
merely to inspect friend-request state. Consumers must treat it as presence
evidence, not OCR of the badge numeral.

### `android-uiauto-raw`

Allowed methods are `GET` and `POST`. Raw paths are capability-allowlisted for
the following protocol families only:

- session discovery/creation;
- source, screenshot, and legacy-native window size reads (`window/current/size`);
- element lookup, safe attributes, text/rectangle reads, click/clear/value writes;
- W3C actions;
- keycode, bounded UTF-8 clipboard write, and app activation/termination/start activity. Foreground
  package/activity is intentionally not advertised: this direct UIA2 build has
  no working native route for it, and deriving it from `/source` can block when
  WeChat suppresses accessibility.

Script execution, shell execution, logs, files, clipboard reads, arbitrary Appium
extensions, and session deletion are rejected. The sole clipboard capability is
`set_clipboard` with exact `plaintext` type, fixed `rsclaw` label, valid base64,
UTF-8 decoding, and a 64 KiB decoded limit. This preserves native JSON
request/response shapes without turning the WASM import into an unrestricted
UIAutomator2 administration channel.

Every allowed path must also:

- start with exactly one `/`;
- contain no scheme, authority, control characters, backslash, `..`, query, or fragment;
- be at most 512 bytes.

GET requests cannot carry a body. POST requests require valid JSON of at most
1 MiB. Session creation accepts only Android/UiAutomator2 capabilities plus
bounded `noReset` and `newCommandTimeout`; executable paths and other arbitrary
Appium capabilities are rejected. Raw does not delete or guess a WebDriver
session. The caller explicitly uses `/sessions` or `/session`, then addresses
the returned session ID. Session deletion remains operator-owned because
stopping instrumentation can affect every plugin sharing that node.

POST bodies are also endpoint-contract checked before CLS starts: locators have
an allowlisted strategy and matching native aliases; text/value must encode the
same bounded Unicode input; click/clear require `{}`; W3C actions are bounded
touch-pointer sequences only; clipboard, keycode, and app lifecycle calls accept
only their exact typed fields. Adding a raw endpoint therefore requires adding both its
path capability and its body contract plus tests.

## Response and error contract

- Success returns compact JSON with the same fields and value types printed by
  the corresponding CLS command.
- `raw` returns the native UIAutomator2/WebDriver JSON body, including its
  `sessionId` and `value` fields.
- CLS non-zero exit, timeout, invalid JSON, and oversized stdout are host errors.
- Known secure-window, missing-accessibility-root, and unsupported-endpoint
  failures are collapsed to stable short errors instead of returning an Appium
  Java stack trace to the plugin or cron logs.
- stderr control characters are neutralized, text is truncated on UTF-8
  character boundaries, and it must not be copied into normal info logs.
- XML source, compact tree text, screenshots, chat text, and raw bodies are not
  logged by the host.

Default deadlines:

| Operation | Deadline |
|---|---:|
| status/key/tap/swipe/current/launch/terminate | 20 s |
| raw | 30 s |
| source/tree/inspect/wait-current/screenshot | 45 s |

`wait-current` may request a shorter logical timeout in its arguments; the host
still enforces its 45-second process deadline.

## Safety boundaries

- WeChat actions use screenshot-grounded coordinates; its accessibility tree is
  never an authorization signal.
- A write flow must verify the foreground package and independent conversation
  title immediately before input and again before the send action.
- Text input focuses the composer with a verified visual coordinate, writes a
  bounded clipboard payload, and injects `KEYCODE_PASTE`; it never locates a
  focused element through accessibility.
- Raw is a transport escape hatch, not an agent-facing general tool. Production
  agents receive ticket-scoped plugin tools, never method/path/body control.
- The host transport does not make a multi-call UI workflow atomic. The v2
  plugin owns a durable lease for its calls, and a deployment must dedicate one
  CLS node to that Android automation plugin. Before multiple plugins or
  gateways may share a node, rsclaw needs a host-global, token-bound flow lease;
  a per-process mutex would not protect multi-call or multi-gateway workflows.
- File staging, clipboard reads/general management, and unrestricted raw calls
  remain unsupported and must not silently invoke ADB.

## Verification

Host unit tests cover:

- command and option allowlists;
- camelCase-to-CLI argument construction without a shell;
- raw method/path/body and endpoint-capability validation;
- streaming stdout bounds, JSON validation, and bounded error rendering;
- configuration validation failing without ADB fallback.

Integration smoke tests use a dedicated device/node:

```bash
cls android status -n android-dev
cls android screenshot -n android-dev -o /tmp/android-canary.png
cls android uiauto raw -n android-dev /status
```

WeChat on the canary device exposes a one-node empty root even with Android
accessibility enabled. `/source`, `tree`, and element lookup are therefore not
part of the production smoke test or monitor path. Screenshot failure is a hard
observation failure, never permission to report an empty inbox.
