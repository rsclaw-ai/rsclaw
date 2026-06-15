# `rsclaw cap` — multi coding-agent CLI orchestration

Status: DRAFT spec (decisions captured from design discussion 2026-06-10)

## Goal

Drive **multiple CLI coding agents** (claudecode / openclaude / opencode / codex)
from the terminal, each isolated in its own git worktree, with two interaction
models:

1. **PTY/tmux interactive** (primary) — real terminals you attach to, drive by
   hand, and iterate on ("持续改进").
2. **cap_rs batch** (secondary) — headless stream-json fan-out, optional reviewer
   verdict, auto-merge the winner.

Acceptance/验收:
- PTY mode → the human is in the loop (attach, iterate, decide). Optional reviewer
  agent runs build/tests.
- Batch mode → a reviewer agent (one of the four, in review mode) `cd`s into each
  candidate worktree, runs build/tests, picks a winner.

## Key constraint

`cap_rs` drivers are **stream-json**, not PTY. So the two modes use different
mechanisms:
- PTY mode: spawn the real `claude` / `codex` / `opencode` CLIs in a PTY, managed
  by tmux directly from rsclaw (`tmux new-session -d` / `send-keys` / `attach`).
  Reuse the existing tmux/PTY session-management code (ported in as a module) —
  no runtime dependency on any external manager binary. tmux gives
  detach-survives + attach-by-name + send-keys for free.
- Batch mode: reuse `cap::runtime::spawn_driver` + a new synchronous
  run-to-completion helper (consume `AgentEvent` until `Done`, auto-answer
  PermissionRequest/AskUser since `dangerously_skip_permissions(true)` is already
  set).

## Shared: worktree lifecycle

- Per run: `.cap-runs/<ts>/<agent>/` git worktree off the current HEAD (one per
  agent), so parallel agents never collide.
- Track {agent, worktree path, branch} in a small run manifest
  (`.cap-runs/<ts>/run.json`).
- Cleanup: keep worktrees until the run is resolved; `rsclaw cap clean` prunes.

## CLI surface (proposed)

```
rsclaw cap run   --agents claudecode,codex,opencode --task "..." [--cwd .] [--mode pty|batch]
rsclaw cap ls                          # list active runs + per-agent status
rsclaw cap attach <run> <agent>        # tmux attach to a PTY session (pty mode)
rsclaw cap say    <run> <agent> "..."  # send a follow-up turn (iterate)
rsclaw cap review <run> [--reviewer codex]   # reviewer runs build/tests, picks winner
rsclaw cap apply  <run> <agent>        # merge that worktree's diff into current branch
rsclaw cap clean  <run>                # remove worktrees + tmux sessions
```

- `--mode pty` (default): launches tmux PTY sessions; `attach`/`say` work.
- `--mode batch`: headless cap_rs; runs to completion; prints summary+diff; with
  `--apply`/`--review` does the auto pipeline.
- `--json` on `run`/`ls`/`review` for scripting.

## Auto-merge winner (chosen behavior)

`review` selects a winner (reviewer verdict or `--winner <agent>`), then `apply`
merges that worktree's diff into the current branch:
- prefer `git -C <worktree> diff <base>..HEAD` → `git apply --3way` on the main
  tree; fall back to `git merge`/cherry-pick if the agent committed.
- conflict → stop, report, leave worktrees intact for manual resolution.
- never auto-apply without an explicit winner (reviewer or flag).

## Build stages

1. **Skeleton + worktree mgr**: `Cap(CapCommand)` in cli, `cmd/cap.rs`, run
   manifest, worktree create/list/clean. No agents yet.
2. **PTY mode**: tmux session per agent running the real CLI in the worktree;
   `attach` / `say` / `ls` status.
3. **Batch mode**: sync `run_to_completion` over `cap::runtime::spawn_driver`;
   parallel fan-out; summary+diff collection.
4. **Reviewer + apply**: reviewer agent over candidate worktrees; winner
   selection; `apply` merge with conflict handling.
5. **Polish**: `--json`, `clean`, docs.

## Notes / open

- Coding-agent CLIs must be installed to test e2e (claude, codex, opencode).
- Should live on its own branch, NOT `feat/football-api` (unrelated to football).
- Port in the existing tmux/PTY session-management code as a rsclaw module;
  manage tmux directly (no external manager binary at runtime).
