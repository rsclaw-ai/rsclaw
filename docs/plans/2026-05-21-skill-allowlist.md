# Skill / Plugin Auto-Install Allowlist

> **For agentic workers:** REQUIRED SUB-SKILL: `superpowers:executing-plans`. This
> gates the `skill_install` agent tool (shipped in `feat(agent): self-service skill
> tools`, commit 80811dc) so the model can only AUTO-install audited, content-pinned
> skills/plugins. `cargo check` + `cargo test` green at each checkbox.

**Status:** Proposed · **Date:** 2026-05-21 · **Branch:** dev · **Sequence:** before exposing skill_install in production (it currently installs ungated).

---

## Problem

`skill_install` (agent-initiated) currently installs any slug from any registry.
Skills carry executable scripts the agent then runs — so an autonomous install
from an uncurated source is a supply-chain / prompt-injection → RCE risk. We need
a security gate: the agent may AUTO-install only audited, content-verified skills;
everything else requires the user (explicit CLI install or confirmation).

## Trust model — two paths

| Path | Initiator | Gate |
|---|---|---|
| `skill_install` tool | agent (autonomous) | **allowlist only**, + content sha256 must match the audited entry |
| `rsclaw skills install <slug>` (CLI) | the user | unrestricted (explicit human intent); may warn if off-allowlist |

The gate is enforced structurally in the tool — the agent cannot self-certify
"the user asked me to," because a prompt-injected agent would claim that. Off-list
installs only happen via the human-initiated CLI path the agent can't invoke.

## Remote source

`https://api.rsclaw.ai/v1/hub/allowlist/` (first-party, HTTPS):

```
allowlist/meta.json      # version + integrity entry (fetch first)
allowlist/skills.json    # audited skills
allowlist/plugins.json   # audited plugins
```

**meta.json** — lightweight; client fetches it first to decide whether to re-pull:
```json
{
  "schema": 1,
  "version": "2026-05-21.1",
  "updated_at": "2026-05-21T12:00:00Z",
  "sha256": { "skills": "<sha256 of skills.json>", "plugins": "<sha256 of plugins.json>" },
  "signature": "<optional: rsclaw-key signature over the fields above>"
}
```

**skills.json / plugins.json** — each entry is an AUDIT RECORD, not just a name:
```json
{ "skills": [
  { "slug": "hithink-market-query",
    "registry": "iwencai",
    "version": "1.0.0",
    "sha256": "<sha256 of the audited skill content>",
    "publisher": "同花顺",
    "audited_at": "2026-05-20" }
] }
```

## Security invariants

1. **slug + content hash double-lock.** Allowlisting a slug is NOT enough — a
   registry could swap the content under an audited slug after audit. On install,
   after download, compute the content sha256 and require it to equal the entry's
   `sha256`; mismatch → refuse. This is the core property of "audited".
2. **Integrity of the list itself.** Verify `skills.json`/`plugins.json` against
   `meta.json.sha256`; if `signature` present, verify it (rsclaw public key) to
   stop a MITM injecting entries. HTTPS + hash-pin is the floor; signature is the
   gold standard.
3. **Fail-closed.** Unreachable AND no local cache → treat allowlist as EMPTY →
   block ALL agent auto-installs (CLI still works). Cache present → use cache
   (stale-but-safe). NEVER "fetch failed → allow."

## Client: fetch / cache / refresh

- On gateway startup + every N hours: GET `meta.json`; if `version` changed,
  GET `skills.json`/`plugins.json`, verify sha256 (+ signature), then mirror to
  the local cache `~/.rsclaw/allowlist/{meta.json,skills.json,plugins.json}`.
- Parse into an in-memory `HashMap<slug, AllowEntry>` for O(1) lookup.
- New module e.g. `src/skill/allowlist.rs`: `Allowlist::load()` (cache→remote),
  `Allowlist::lookup_skill(slug) -> Option<AllowEntry>`, `verify_content(path, &entry)`.

## Gate wiring (skill_install)

```text
tool_skill_install(slug):
    entry = allowlist.lookup_skill(slug)
    if entry is None:
        return refuse: "<slug> is not on the audited allowlist. Ask the user to
                        confirm, or they can install it: rsclaw skills install <slug>"
    download into a temp/staging dir
    if sha256(content) != entry.sha256:
        return refuse: "audited-hash mismatch — registry content changed since audit"
    move into ~/.rsclaw/skills/   (then existing skill_use / reload picks it up)
```

CLI `rsclaw skills install` stays unrestricted (human intent); optionally print a
"not on the audited allowlist" note when off-list.

## Plugins

Same `plugins.json` shape + `lookup_plugin`, wired into a future `plugin_install`
(plugin hot-add is a separate backlog item; the allowlist is ready for it).

## Task list

- [ ] `src/skill/allowlist.rs`: fetch (meta→lists) + sha256/signature verify +
      local cache mirror + in-memory map + fail-closed.
- [ ] Startup: kick off the initial load (background; don't block boot).
- [ ] `tool_skill_install`: gate on `lookup_skill` + post-download content-hash
      verify; refuse with actionable guidance off-list.
- [ ] CLI install: optional off-list warning (no block).
- [ ] Tests: lookup hit/miss, hash-match/mismatch refusal, fail-closed (empty on
      fetch failure), cache reuse.
- [ ] (server, separate) publish the three JSON files at the hub URL.

## Out of scope
- The audited-list CONTENT + the hub server endpoints (rsclaw-server / hub side).
- Signature key distribution (decide: pin a public key in the binary vs config).
- Plugin hot-add runtime (separate backlog).
