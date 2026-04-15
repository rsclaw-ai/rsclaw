#!/bin/bash
# tools/lib.sh — Shared functions for RsClaw sub-agent pipelines.
# Source this file at the top of each pipeline script.

set -euo pipefail

# ── Colors ────────────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

# ── Logging ───────────────────────────────────────────────────────────────────

log_step()  { echo -e "${CYAN}>> [${1}] ${2}${NC}"; }
log_ok()    { echo -e "  ${GREEN}OK${NC} ${1}"; }
log_warn()  { echo -e "  ${YELLOW}WARN${NC} ${1}"; }
log_fail()  { echo -e "  ${RED}FAIL${NC} ${1}"; }

# ── Retry wrapper ─────────────────────────────────────────────────────────────
# Usage: run_claude <max_retries> <worktree_dir> <prompt>
# Returns 0 on success, 1 on exhausted retries.

run_claude() {
  local max_retries="${1}"
  local dir="${2}"
  local prompt="${3}"
  local attempt=0

  while [ "$attempt" -lt "$max_retries" ]; do
    attempt=$((attempt + 1))
    if claude --dir "$dir" "$prompt" 2>&1; then
      return 0
    fi
    if [ "$attempt" -lt "$max_retries" ]; then
      log_warn "Attempt $attempt/$max_retries failed in $dir — retrying..."
      sleep 2
    fi
  done

  log_fail "All $max_retries attempts failed in $dir"
  return 1
}

# ── Parallel runner with retry ────────────────────────────────────────────────
# Usage: run_parallel <max_retries> <dir1> <prompt1> <dir2> <prompt2> ...
# Arguments must be in (dir, prompt) pairs.
# Returns 0 if all succeed, 1 if any fail.

run_parallel() {
  local max_retries="${1}"
  shift

  local dirs=()
  local prompts=()
  while [ $# -ge 2 ]; do
    dirs+=("$1")
    prompts+=("$2")
    shift 2
  done

  local pids=()
  local statuses=()
  local tmpdir
  tmpdir=$(mktemp -d)

  for i in "${!dirs[@]}"; do
    (
      if run_claude "$max_retries" "${dirs[$i]}" "${prompts[$i]}"; then
        echo "0" > "$tmpdir/$i"
      else
        echo "1" > "$tmpdir/$i"
      fi
    ) &
    pids+=($!)
  done

  # Wait for all
  for pid in "${pids[@]}"; do
    wait "$pid" || true
  done

  # Check results
  local failed=0
  for i in "${!dirs[@]}"; do
    local status
    status=$(cat "$tmpdir/$i" 2>/dev/null || echo "1")
    if [ "$status" != "0" ]; then
      log_fail "${dirs[$i]}"
      failed=1
    fi
  done

  rm -rf "$tmpdir"
  return "$failed"
}

# ── Worktree sync ────────────────────────────────────────────────────────────
# Commit changes in source worktree, then copy specified paths to targets.
#
# Usage: sync_worktree <source_worktree> <commit_msg> <target1> [target2...] -- <path1> [path2...]
#
# Example:
#   sync_worktree .worktrees/architect "architect output" \
#     .worktrees/backend-dev .worktrees/ui-dev \
#     -- docs/interfaces docs/ui-specs

sync_worktree() {
  local source="$1"
  local msg="$2"
  shift 2

  local targets=()
  while [ $# -gt 0 ] && [ "$1" != "--" ]; do
    targets+=("$1")
    shift
  done

  # Skip the "--" separator
  [ "${1:-}" = "--" ] && shift

  local paths=("$@")

  # Commit in source worktree
  if git -C "$source" diff --quiet && git -C "$source" diff --cached --quiet; then
    # Check for untracked files in the paths we care about
    local has_untracked=false
    for p in "${paths[@]}"; do
      if [ -n "$(git -C "$source" ls-files --others --exclude-standard "$p" 2>/dev/null)" ]; then
        has_untracked=true
        break
      fi
    done
    if [ "$has_untracked" = false ]; then
      log_warn "No changes to sync from $source"
      return 0
    fi
  fi

  for p in "${paths[@]}"; do
    git -C "$source" add "$p" 2>/dev/null || true
  done
  git -C "$source" commit -m "$msg" --allow-empty 2>/dev/null || true

  # Copy files to each target
  for target in "${targets[@]}"; do
    for p in "${paths[@]}"; do
      if [ -e "$source/$p" ]; then
        # Ensure parent dir exists in target
        if [ -d "$source/$p" ]; then
          mkdir -p "$target/$p"
          cp -r "$source/$p/." "$target/$p/"
        elif [ -f "$source/$p" ]; then
          mkdir -p "$target/$(dirname "$p")"
          cp "$source/$p" "$target/$p"
        fi
      fi
    done
    log_ok "Synced to $target"
  done
}

# ── Merge feature branches ───────────────────────────────────────────────────
# Collect commits from worktrees into a single integration branch.
#
# Usage: merge_worktrees <integration_branch> <worktree1> [worktree2...]

merge_worktrees() {
  local integration_branch="$1"
  shift
  local worktrees=("$@")

  # Create integration branch from current HEAD
  git checkout -B "$integration_branch" HEAD

  for wt in "${worktrees[@]}"; do
    if [ ! -d "$wt" ]; then
      log_warn "Worktree $wt not found — skipping"
      continue
    fi

    # Get the HEAD commit of the worktree
    local wt_head
    wt_head=$(git -C "$wt" rev-parse HEAD 2>/dev/null || echo "")
    local main_head
    main_head=$(git rev-parse HEAD 2>/dev/null || echo "")

    if [ -z "$wt_head" ] || [ "$wt_head" = "$main_head" ]; then
      log_warn "No new commits in $wt — skipping"
      continue
    fi

    # Cherry-pick all commits from worktree that are not in integration branch
    local commits
    commits=$(git -C "$wt" log --reverse --format="%H" "$main_head..$wt_head" 2>/dev/null || echo "")
    if [ -z "$commits" ]; then
      log_warn "No new commits in $wt — skipping"
      continue
    fi

    for commit in $commits; do
      if git cherry-pick "$commit" 2>/dev/null; then
        log_ok "Cherry-picked $(git log -1 --format='%s' "$commit") from $wt"
      else
        log_warn "Conflict cherry-picking from $wt — skipping commit"
        git cherry-pick --abort 2>/dev/null || true
      fi
    done
  done

  log_ok "Integration branch ready: $integration_branch"
}

# ── Step resume support ──────────────────────────────────────────────────────
# Usage: should_run_step <current_step> <from_step>
# Returns 0 (true) if current_step >= from_step

should_run_step() {
  [ "$1" -ge "$2" ]
}

# ── State file for resume ────────────────────────────────────────────────────

PIPELINE_STATE_DIR=".pipeline-state"

save_state() {
  local pipeline="$1"
  local step="$2"
  mkdir -p "$PIPELINE_STATE_DIR"
  echo "$step" > "$PIPELINE_STATE_DIR/$pipeline.step"
}

load_state() {
  local pipeline="$1"
  local state_file="$PIPELINE_STATE_DIR/$pipeline.step"
  if [ -f "$state_file" ]; then
    cat "$state_file"
  else
    echo "1"
  fi
}

clear_state() {
  local pipeline="$1"
  rm -f "$PIPELINE_STATE_DIR/$pipeline.step"
}
