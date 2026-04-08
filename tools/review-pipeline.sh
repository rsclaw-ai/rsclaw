#!/bin/bash
# tools/review-pipeline.sh
# Run the review and QA gate for an existing branch.
# Use this to re-run reviews after fixing [BLOCK] items,
# or to review branches developed outside the parallel-feature pipeline.
#
# Usage:
#   ./tools/review-pipeline.sh <branch-name>
#   ./tools/review-pipeline.sh <branch-name> --from-step 2   # skip to QA gate
#
# Example: ./tools/review-pipeline.sh feat/ws-operator-broadcast

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/lib.sh"

MAX_RETRIES=2

# ── Input validation ────────────────────────────────────────────────────────

BRANCH="${1:-}"
FROM_STEP=1

if [ -z "$BRANCH" ]; then
  echo "Error: branch name is required."
  echo ""
  echo "Usage: ./tools/review-pipeline.sh <branch-name> [--from-step N]"
  echo "Example: ./tools/review-pipeline.sh feat/ws-operator-broadcast"
  exit 1
fi

shift
while [ $# -gt 0 ]; do
  case "$1" in
    --from-step) FROM_STEP="$2"; shift 2 ;;
    --resume)    FROM_STEP=$(load_state "review-$BRANCH"); shift ;;
    *)           echo "Unknown option: $1"; exit 1 ;;
  esac
done

# Derive a safe slug for file names
SLUG="${BRANCH//\//-}"
SLUG="${SLUG//[^a-zA-Z0-9-]/-}"

# ── Preflight checks ────────────────────────────────────────────────────────

WORKTREES_DIR=".worktrees"

for role in reviewer design-reviewer qa-lead; do
  if [ ! -d "$WORKTREES_DIR/$role" ]; then
    echo "Error: worktree not found for role '$role'."
    echo "Run ./tools/init-worktrees.sh first."
    exit 1
  fi
done

# Verify the branch exists
if ! git rev-parse --verify "$BRANCH" > /dev/null 2>&1; then
  echo "Error: branch '$BRANCH' does not exist in this repository."
  exit 1
fi

# Sync branch content to reviewer worktrees
for role in reviewer design-reviewer qa-lead; do
  git -C "$WORKTREES_DIR/$role" checkout "$BRANCH" -- . 2>/dev/null || true
done

mkdir -p docs/reviews

echo "========================================"
echo " RsClaw Review Pipeline"
echo " Branch: $BRANCH"
echo " Starting from step: $FROM_STEP"
echo "========================================"
echo ""

# ── Step 1: Rust reviewer + Design reviewer (parallel) ──────────────────────

if should_run_step 1 "$FROM_STEP"; then
  log_step "1/2" "Running Rust and UI reviews in parallel..."

  run_parallel "$MAX_RETRIES" \
    "$WORKTREES_DIR/reviewer" \
    "Review the Rust code changes on branch: $BRANCH

     Focus on src/ changes only.
     Output your complete review to docs/reviews/$SLUG.md
     Use [BLOCK], [SUGGEST], [NOTE] tags as defined in CLAUDE.md.
     End with a VERDICT line: APPROVED or BLOCKED." \
    "$WORKTREES_DIR/design-reviewer" \
    "Review the UI code changes on branch: $BRANCH

     Focus on ui/ changes only.
     Output your complete review to docs/reviews/ui-$SLUG.md
     Use [VISUAL-BLOCK], [UX-BLOCK], [SUGGEST], [NOTE] tags as defined in CLAUDE.md.
     End with a VERDICT line: APPROVED or BLOCKED."

  # Sync review output to main repo and qa-lead
  sync_worktree "$WORKTREES_DIR/reviewer" "review: $BRANCH backend" \
    . "$WORKTREES_DIR/qa-lead" \
    -- docs/reviews

  sync_worktree "$WORKTREES_DIR/design-reviewer" "review: $BRANCH UI" \
    . "$WORKTREES_DIR/qa-lead" \
    -- docs/reviews

  echo ""

  # ── Collect and display blocking issues ──────────────────────────────────

  RUST_REVIEW="docs/reviews/$SLUG.md"
  UI_REVIEW="docs/reviews/ui-$SLUG.md"

  RUST_BLOCKS=0
  UI_BLOCKS=0

  if [ -f "$RUST_REVIEW" ]; then
    RUST_BLOCKS=$(grep -cE "\[BLOCK\]" "$RUST_REVIEW" || true)
  fi

  if [ -f "$UI_REVIEW" ]; then
    UI_BLOCKS=$(grep -cE "\[(VISUAL-BLOCK|UX-BLOCK)\]" "$UI_REVIEW" || true)
  fi

  TOTAL_BLOCKS=$((RUST_BLOCKS + UI_BLOCKS))

  echo "  Review summary:"
  echo "    Rust  — $RUST_BLOCKS blocking issue(s)  ($RUST_REVIEW)"
  echo "    UI    — $UI_BLOCKS blocking issue(s)  ($UI_REVIEW)"
  echo ""

  if [ "$TOTAL_BLOCKS" -gt 0 ]; then
    echo "  Blocking issues found:"
    echo ""
    grep -hE "\[(BLOCK|VISUAL-BLOCK|UX-BLOCK)\]" "$RUST_REVIEW" "$UI_REVIEW" 2>/dev/null | \
      sed 's/^/    /' || true
    echo ""
    log_fail "Pipeline stopped — fix all [BLOCK] items and re-run:"
    echo "    ./tools/review-pipeline.sh $BRANCH"
    save_state "review-$BRANCH" 1
    exit 1
  fi

  log_ok "No blocking issues"
  save_state "review-$BRANCH" 2
  echo ""
fi

# ── Step 2: QA Lead (serial) ─────────────────────────────────────────────────

if should_run_step 2 "$FROM_STEP"; then
  log_step "2/2" "QA Lead — final gate..."

  run_claude "$MAX_RETRIES" "$WORKTREES_DIR/qa-lead" \
    "Run the final QA gate for branch: $BRANCH

     Review reports to check:
     - docs/reviews/$SLUG.md      (Rust review)
     - docs/reviews/ui-$SLUG.md   (UI review)

     Run the full merge checklist from CLAUDE.md.
     If all checks pass, output the sign-off block.
     If any hard-stop condition is met, output BLOCKED with a clear reason and stop."

  clear_state "review-$BRANCH"
  echo ""
fi

echo "========================================"
echo " Review pipeline complete: $BRANCH"
echo "========================================"
