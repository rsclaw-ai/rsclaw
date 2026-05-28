# cap manual smoke checklist

After restarting the gateway in this worktree:

```bash
cd /Users/oopos/dev/github-rsclaw/.worktrees/feat-cap
cargo run -- gateway restart
```

For each coding agent whose CLI is installed locally, fire one prompt
through `tool_cap` and confirm a non-empty reply.

```
curl -X POST http://localhost:8442/api/v1/chat \
  -H 'Content-Type: application/json' \
  -d '{"message": "use tool_cap with agent=<AGENT> and task=\"print hello\""}'
```

Replace `<AGENT>` with each of:

- `claudecode`  (binary: `claude`, override via `CLAUDE_BIN`)
- `openclaude`  (binary: `openclaude`, override via `OPENCLAUDE_BIN`)
- `opencode`    (binary: `opencode`, override via `OPENCODE_BIN`)
- `codex`       (binary: `codex`, must be on `$PATH`)

For each, confirm:

- [ ] Response includes `"agent": "<AGENT>"` and a non-empty `"reply"` string.
- [ ] Gateway log (`tail -f var/log/gateway.log` or wherever your profile points) shows `tool_cap: dispatch agent=<AGENT>` then a `cap permission auto-approve` if the agent requested any (claudecode/openclaude/opencode typically do; codex via `CodexMcpDriver` may not).
- [ ] No `cap actor for <AGENT> closed` errors.

If any agent's CLI isn't installed, skip — note it in the gateway log
will show `cap <AGENT> spawn: …NotFound…` which is the expected
"binary not on PATH" outcome.

## Known caveats

- **Permission**: P1 always approves. If a coding agent is about to do
  something destructive in your workspace, this WILL go through. P2
  (real-time conversation) is where human-in-the-loop lives.
- **Driver respawn**: if a CLI crashes mid-turn the slot empties; the
  next `tool_cap` for that agent respawns it. No backoff — if the CLI
  keeps crashing you get rapid respawn cycles.
- **codex**: transitional `CodexMcpDriver`. Will switch to
  `ClaudeCodeDriver::codex_builder` once cap-rs ships it.
