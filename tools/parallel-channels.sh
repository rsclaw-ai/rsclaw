#!/bin/bash
# tools/parallel-channels.sh
# Implement multiple channels in parallel, each in its own worktree.
# Each channel gets its own backend-dev + backend-tester pair.
#
# Usage:
#   ./tools/parallel-channels.sh <channel1> <channel2> ...
#   ./tools/parallel-channels.sh --from-step 3 <channel1> <channel2> ...
#   ./tools/parallel-channels.sh --resume <channel1> <channel2> ...
#
# Example: ./tools/parallel-channels.sh signal zalo matrix

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/lib.sh"

MAX_RETRIES=2

# ── Input parsing ────────────────────────────────────────────────────────────

FROM_STEP=1
CHANNELS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --from-step) FROM_STEP="$2"; shift 2 ;;
    --resume)    FROM_STEP=0; shift ;;  # resolved after channels are known
    *)           CHANNELS+=("$1"); shift ;;
  esac
done

if [ ${#CHANNELS[@]} -eq 0 ]; then
  echo "Error: at least one channel name is required."
  echo ""
  echo "Usage: ./tools/parallel-channels.sh [--from-step N] <channel1> [channel2] ..."
  echo "Example: ./tools/parallel-channels.sh signal zalo matrix"
  exit 1
fi

for ch in "${CHANNELS[@]}"; do
  if [[ ! "$ch" =~ ^[a-z0-9_]+$ ]]; then
    echo "Error: channel name '$ch' must be lowercase alphanumeric with underscores only."
    exit 1
  fi
done

# Build a stable pipeline ID from sorted channel names
PIPELINE_ID="channels-$(IFS=-; echo "${CHANNELS[*]}")"

if [ "$FROM_STEP" -eq 0 ]; then
  FROM_STEP=$(load_state "$PIPELINE_ID")
fi

# ── Preflight checks ────────────────────────────────────────────────────────

ROLES_DIR=".claude/roles"
WORKTREES_DIR=".worktrees"

if [ ! -f "$ROLES_DIR/backend-dev.md" ]; then
  echo "Error: $ROLES_DIR/backend-dev.md not found."
  echo "Run ./tools/init-worktrees.sh first."
  exit 1
fi

if [ ! -f "$ROLES_DIR/backend-tester.md" ]; then
  echo "Error: $ROLES_DIR/backend-tester.md not found."
  exit 1
fi

mkdir -p "$WORKTREES_DIR" docs/reviews

echo "========================================"
echo " RsClaw Parallel Channel Build"
echo " Channels: ${CHANNELS[*]}"
echo " Starting from step: $FROM_STEP"
echo "========================================"
echo ""

# ── Step 1: Provision per-channel worktrees ──────────────────────────────────

if should_run_step 1 "$FROM_STEP"; then
  log_step "1/4" "Provisioning worktrees..."

  for ch in "${CHANNELS[@]}"; do
    DEV_PATH="$WORKTREES_DIR/dev-$ch"
    TEST_PATH="$WORKTREES_DIR/test-$ch"

    if [ ! -d "$DEV_PATH" ]; then
      git worktree add "$DEV_PATH" HEAD --detach
      cp "$ROLES_DIR/backend-dev.md" "$DEV_PATH/CLAUDE.md"
      log_ok "dev-$ch"
    else
      log_warn "dev-$ch (already exists)"
    fi

    if [ ! -d "$TEST_PATH" ]; then
      git worktree add "$TEST_PATH" HEAD --detach
      cp "$ROLES_DIR/backend-tester.md" "$TEST_PATH/CLAUDE.md"
      log_ok "test-$ch"
    else
      log_warn "test-$ch (already exists)"
    fi
  done

  save_state "$PIPELINE_ID" 2
  echo ""
fi

# ── Step 2: Implement all channels in parallel ───────────────────────────────

if should_run_step 2 "$FROM_STEP"; then
  log_step "2/4" "Implementing channels in parallel..."

  ARGS=()
  for ch in "${CHANNELS[@]}"; do
    ARGS+=("$WORKTREES_DIR/dev-$ch")
    ARGS+=("Implement the '$ch' channel adapter for RsClaw.

     Steps:
     1. Create src/channel/$ch.rs implementing the Channel trait
     2. Add config struct in src/config/schema.rs with #[serde(flatten)] pub base: ChannelBase
     3. Add start_${ch}_if_configured() in src/gateway/startup.rs
     4. Wire DM policy enforcer (pairing / allowlist / open / disabled)
     5. Create a skeleton test file at tests/channel_$ch.rs
     6. Add channel to the UI list in ui/app/components/rsclaw-panel.tsx

     Follow all rules in CLAUDE.md.")
  done

  if ! run_parallel "$MAX_RETRIES" "${ARGS[@]}"; then
    log_fail "Some channel implementations failed. Fix and resume:"
    echo "  ./tools/parallel-channels.sh --from-step 2 ${CHANNELS[*]}"
    save_state "$PIPELINE_ID" 2
    exit 1
  fi

  # Sync dev output to test worktrees
  for ch in "${CHANNELS[@]}"; do
    sync_worktree "$WORKTREES_DIR/dev-$ch" "feat: channel $ch implementation" \
      "$WORKTREES_DIR/test-$ch" \
      -- src tests
  done

  save_state "$PIPELINE_ID" 3
  log_ok "All channels implemented"
  echo ""
fi

# ── Step 3: Test all channels in parallel ────────────────────────────────────

if should_run_step 3 "$FROM_STEP"; then
  log_step "3/4" "Writing tests in parallel..."

  ARGS=()
  for ch in "${CHANNELS[@]}"; do
    ARGS+=("$WORKTREES_DIR/test-$ch")
    ARGS+=("Write comprehensive tests for the '$ch' channel.

     Fill in tests/channel_$ch.rs with:
     - Successful message send
     - Send failure -> retry -> eventual success
     - Send failure -> retry exhausted -> error propagated
     - DM policy: pairing enforced
     - DM policy: allowlist enforced

     Follow all rules in CLAUDE.md.")
  done

  if ! run_parallel "$MAX_RETRIES" "${ARGS[@]}"; then
    log_fail "Some channel tests failed. Fix and resume:"
    echo "  ./tools/parallel-channels.sh --from-step 3 ${CHANNELS[*]}"
    save_state "$PIPELINE_ID" 3
    exit 1
  fi

  save_state "$PIPELINE_ID" 4
  log_ok "All channel tests written"
  echo ""
fi

# ── Step 4: Review all channels in parallel ──────────────────────────────────

if should_run_step 4 "$FROM_STEP"; then
  log_step "4/4" "Reviewing all channels in parallel..."

  if [ ! -f "$ROLES_DIR/reviewer.md" ]; then
    log_warn "reviewer.md not found — skipping review step."
  else
    # Provision per-channel reviewer worktrees
    ARGS=()
    for ch in "${CHANNELS[@]}"; do
      REVIEW_PATH="$WORKTREES_DIR/reviewer-$ch"
      if [ ! -d "$REVIEW_PATH" ]; then
        git worktree add "$REVIEW_PATH" HEAD --detach
        cp "$ROLES_DIR/reviewer.md" "$REVIEW_PATH/CLAUDE.md"
      fi

      # Sync dev+test output to reviewer
      sync_worktree "$WORKTREES_DIR/dev-$ch" "sync: dev-$ch to reviewer" \
        "$REVIEW_PATH" -- src tests
      sync_worktree "$WORKTREES_DIR/test-$ch" "sync: test-$ch to reviewer" \
        "$REVIEW_PATH" -- tests

      ARGS+=("$REVIEW_PATH")
      ARGS+=("Review the '$ch' channel implementation.
       Output to docs/reviews/channel-$ch.md.
       Use [BLOCK], [SUGGEST], [NOTE] tags.
       Include a VERDICT line.")
    done

    run_parallel "$MAX_RETRIES" "${ARGS[@]}" || true

    # Sync review output and count blocks
    TOTAL_BLOCKS=0
    for ch in "${CHANNELS[@]}"; do
      REVIEW_PATH="$WORKTREES_DIR/reviewer-$ch"
      sync_worktree "$REVIEW_PATH" "review: channel $ch" \
        . -- docs/reviews

      REVIEW_FILE="docs/reviews/channel-$ch.md"
      if [ -f "$REVIEW_FILE" ]; then
        COUNT=$(grep -cE "\[BLOCK\]" "$REVIEW_FILE" || true)
        if [ "$COUNT" -gt 0 ]; then
          log_fail "$ch — $COUNT blocking issue(s)"
          TOTAL_BLOCKS=$((TOTAL_BLOCKS + COUNT))
        else
          log_ok "$ch — clean"
        fi
      fi
    done

    if [ "$TOTAL_BLOCKS" -gt 0 ]; then
      echo ""
      log_fail "$TOTAL_BLOCKS total blocking issue(s) across channels."
      echo "Fix all [BLOCK] items and resume:"
      echo "  ./tools/parallel-channels.sh --from-step 4 ${CHANNELS[*]}"
      save_state "$PIPELINE_ID" 4
      exit 1
    fi
  fi

  clear_state "$PIPELINE_ID"
  echo ""
fi

echo "========================================"
echo " All channels complete: ${CHANNELS[*]}"
echo "========================================"
