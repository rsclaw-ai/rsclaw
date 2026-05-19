# Env Sync — auto-managed `.env`

The gateway reads secrets and any `${VAR}` placeholder in
`rsclaw.json5` from a single auto-managed file:

```
$RSCLAW_BASE_DIR/.env       (default: ~/.rsclaw/.env, mode 0600)
```

You don't need to populate it by hand. The runtime keeps it in sync
with your shell on every load.

## What happens on `rsclaw gateway run`

1. **Snapshot** — current process env is frozen (this is "the shell").
2. **Load `.env`** — additive: vars already set by the shell are not
   overwritten.
3. **Scan `rsclaw.json5`** — every `${VAR}` and every
   `{ "source": "env", "id": "VAR" }` reference is collected.
4. **Reconcile** — for each referenced var:
   - shell has it + `.env` has same value → no-op
   - shell has it + `.env` has different value → **shell wins**, `.env`
     updated (rotation case)
   - shell has it + `.env` missing → `.env` adds it (first sync)
   - shell missing + `.env` has it → keep `.env` (service-manager case)
   - both missing → see "Shell rc fallback" below
5. **Write `.env`** atomically if anything changed.

## Shell rc fallback

When a var is missing from both the shell snapshot AND `.env` —
typical when `launchd` / `systemd` starts the gateway with an empty
env and you've only ever `export`-ed the var in `~/.zshrc` — the
gateway invokes:

```
$SHELL -lic 'env'
```

(`-l` for login, `-i` for interactive — sources the full rc/profile
chain). Output is filtered to just the still-missing vars, written
to `.env`, and the `_RSCLAW_ENV_INHERITED=1` marker is set so
self-restart children skip the source.

`$SHELL` is resolved from the env var first; falls back to
`dscl . -read /Users/<user>` (macOS) or `getent passwd <user>`
(Linux) when launchd / systemd doesn't pass `$SHELL` through.

Skip the fallback with `RSCLAW_NO_SHELL_SOURCE=1`. Disabled on
Windows (no POSIX rc-file convention).

## When a var is still unresolved

If a provider's `apiKey` resolves to an unresolved `${VAR}`
placeholder AND there's no `<NAME>_API_KEY` env fallback, the
provider is **disabled at boot** rather than registered with an
empty key. The disable reason includes the var name and a fix hint.

The provider's slot in `/api/v1/status` shows:

```json
{
  "providers": {
    "active": ["anthropic", "deepseek"],
    "disabled": [
      {
        "name": "rsclaw",
        "reason": "apiKey unresolved — set RSCLAW_API_KEY in your shell (then `rsclaw env sync`) or edit ~/.rsclaw/.env directly"
      }
    ]
  }
}
```

Failover skips disabled providers automatically.

## Operator commands

```bash
rsclaw env list
```

Show every var referenced in `rsclaw.json5` and its resolution
status (set in shell / set in `.env` / drift / missing).

```bash
rsclaw env sync [--dry-run] [--force]
```

Apply the same reconcile pipeline manually. `--dry-run` previews
changes; `--force` also blanks `.env` entries the shell no longer
exports.

```bash
rsclaw doctor
```

Includes an `Env:` section summarising referenced/ok/missing/drift
counts and emitting warn-level issues for each missing or drifting
var.

## Rotating a key

1. Edit `~/.zshrc` (or wherever you export it).
2. `source ~/.zshrc` in your terminal.
3. `rsclaw gateway restart` from that same terminal.

The restart inherits your updated shell env. The reconcile pipeline
detects shell != `.env` and rewrites `.env` with the new value. The
next service-managed launch reads the new value from `.env`.

To rotate **without** touching the shell:

```bash
$EDITOR ~/.rsclaw/.env
rsclaw gateway restart
```

`.env` is the source of truth as long as the shell doesn't
contradict it.

## Opt-outs

| Env var | Effect |
| --- | --- |
| `RSCLAW_NO_ENV_SYNC=1` | Skip the full reconcile pipeline. `${VAR}` resolves from process env only (legacy behaviour). |
| `RSCLAW_NO_SHELL_SOURCE=1` | Skip the shell-rc fallback. Still reads `.env`. |
| `_RSCLAW_ENV_INHERITED=1` | Already-tried marker set internally; honour it if you're wrapping the gateway in your own supervisor. |

## File format

```
# Auto-managed by rsclaw. Edits are preserved unless overwritten by
# `rsclaw env sync` or auto-sync at startup (shell wins on diff).
RSCLAW_API_KEY=sk-...
DEEPSEEK_API_KEY=sk-...
```

One `KEY=VAL` per line. `#` starts a comment. Blank lines ignored.
No quoting / no escapes — values with `\n` are skipped on write
with a `# SKIPPED:` marker so they round-trip and are obvious.
Keys are sorted alphabetically for stable diffs.

## Implementation pointers

| File | Role |
| --- | --- |
| `src/config/env_file.rs` | Read/write `.env` (atomic, mode 0600). |
| `src/config/env_resolution.rs` | Shell snapshot, reconcile, shell-rc fallback. |
| `src/config/loader.rs` | Calls `env_resolution::reconcile()` before `expand_env_vars`. |
| `src/gateway/providers.rs` | Detects unresolved `apiKey` → `registry.disable()`. |
| `src/provider/registry.rs` | `disable()` / `disabled_list()` API. |
| `src/cmd/env.rs` | `env list` / `env sync` CLI handlers. |
| `src/cmd/doctor.rs` | `Env:` section in `rsclaw doctor`. |
