#!/bin/bash
# tools/parallel-feature.sh
# Orchestrate a full feature development cycle using parallel sub-agents.
#
# Pipeline:
#   1. Architect (serial)        — design interfaces + UI specs
#   2. Backend-dev + UI-dev      — implement in parallel
#   3. Backend-tester + UI-tester — write tests in parallel
#   4. Reviewer + Design-reviewer — review in parallel
#   5. QA Lead (serial)          — final quality gate
#   6. Merge                     — integrate into feature branch
#
# Usage:
#   ./tools/parallel-feature.sh <feature-name>
#   ./tools/parallel-feature.sh <feature-name> --from-step 3   # resume from step 3
#   ./tools/parallel-feature.sh <feature-name> --resume         # resume from last saved step
#
# Example: ./tools/parallel-feature.sh ws-operator-broadcast

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/lib.sh"

MAX_RETRIES=2

# ── Input validation ────────────────────────────────────────────────────────

FEATURE="${1:-}"
FROM_STEP=1

if [ -z "$FEATURE" ]; then
  echo "Error: feature name is required."
  echo ""
  echo "Usage: ./tools/parallel-feature.sh <feature-name> [--from-step N] [--resume]"
  exit 1
fi

if [[ ! "$FEATURE" =~ ^[a-z0-9-]+$ ]]; then
  echo "Error: feature name must be lowercase alphanumeric with hyphens only."
  exit 1
fi

shift
while [ $# -gt 0 ]; do
  case "$1" in
    --from-step) FROM_STEP="$2"; shift 2 ;;
    --resume)    FROM_STEP=$(load_state "feature-$FEATURE"); shift ;;
    *)           echo "Unknown option: $1"; exit 1 ;;
  esac
done

# ── Preflight checks ────────────────────────────────────────────────────────

WORKTREES_DIR=".worktrees"
REQUIRED_WORKTREES=(architect backend-dev ui-dev backend-tester ui-tester reviewer design-reviewer qa-lead)

for role in "${REQUIRED_WORKTREES[@]}"; do
  if [ ! -d "$WORKTREES_DIR/$role" ]; then
    echo "Error: worktree not found for role '$role'."
    echo "Run ./tools/init-worktrees.sh first."
    exit 1
  fi
done

mkdir -p docs/interfaces docs/ui-specs docs/reviews docs/adr

echo "========================================"
echo " RsClaw Sub-Agent Pipeline"
echo " Feature: $FEATURE"
echo " Starting from step: $FROM_STEP"
echo "========================================"
echo ""

# ── Step 1: Architect (serial) ───────────────────────────────────────────────

if should_run_step 1 "$FROM_STEP"; then
  log_step "1/6" "Architect — designing interfaces and UI specs..."

  run_claude "$MAX_RETRIES" "$WORKTREES_DIR/architect" \
    "Design the interfaces and UI specs for feature: $FEATURE.

     Required outputs:
     - docs/interfaces/$FEATURE.md  (Rust traits, TypeScript types, WS event shapes)
     - docs/ui-specs/$FEATURE.md    (layout, component states, data contracts)

     Do not write any implementation code.
     Stop when both documents are written and complete."

  # Validate output exists in architect worktree
  if [ ! -f "$WORKTREES_DIR/architect/docs/interfaces/$FEATURE.md" ]; then
    log_fail "Architect did not produce docs/interfaces/$FEATURE.md"
    exit 1
  fi
  if [ ! -f "$WORKTREES_DIR/architect/docs/ui-specs/$FEATURE.md" ]; then
    log_fail "Architect did not produce docs/ui-specs/$FEATURE.md"
    exit 1
  fi

  # Sync architect output to main repo and dev worktrees
  sync_worktree "$WORKTREES_DIR/architect" "design: $FEATURE interfaces and UI specs" \
    . "$WORKTREES_DIR/backend-dev" "$WORKTREES_DIR/ui-dev" \
    -- docs/interfaces docs/ui-specs

  save_state "feature-$FEATURE" 2
  log_ok "Interface definitions ready"
  echo ""
fi

# ── Step 2: Backend-dev + UI-dev (parallel) ──────────────────────────────────

if should_run_step 2 "$FROM_STEP"; then
  log_step "2/6" "Backend-dev + UI-dev — implementing in parallel..."

  run_parallel "$MAX_RETRIES" \
    "$WORKTREES_DIR/backend-dev" \
    "Implement feature '$FEATURE' in src/.

     Read docs/interfaces/$FEATURE.md for the interface contract.
     Create a skeleton test file at tests/${FEATURE//-/_}.rs when done.
     Follow all rules in CLAUDE.md." \
    "$WORKTREES_DIR/ui-dev" \
    "Implement feature '$FEATURE' in ui/.

     Read docs/ui-specs/$FEATURE.md for the component spec.
     Read docs/interfaces/$FEATURE.md for TypeScript types and WS event shapes.
     Follow all rules in CLAUDE.md."

  # Sync dev output to tester and reviewer worktrees
  sync_worktree "$WORKTREES_DIR/backend-dev" "feat: $FEATURE backend implementation" \
    "$WORKTREES_DIR/backend-tester" "$WORKTREES_DIR/reviewer" \
    -- src tests

  sync_worktree "$WORKTREES_DIR/ui-dev" "feat: $FEATURE UI implementation" \
    "$WORKTREES_DIR/ui-tester" "$WORKTREES_DIR/design-reviewer" \
    -- ui

  save_state "feature-$FEATURE" 3
  log_ok "Backend and UI implementation complete"
  echo ""
fi

# ── Step 3: Backend-tester + UI-tester (parallel) ────────────────────────────

if should_run_step 3 "$FROM_STEP"; then
  log_step "3/6" "Backend-tester + UI-tester — writing tests in parallel..."

  run_parallel "$MAX_RETRIES" \
    "$WORKTREES_DIR/backend-tester" \
    "Write comprehensive tests for backend feature '$FEATURE'.

     Read src/ changes for this feature.
     Write tests to tests/${FEATURE//-/_}.rs.
     Prioritize error paths and boundary conditions.
     Follow all rules in CLAUDE.md." \
    "$WORKTREES_DIR/ui-tester" \
    "Write comprehensive tests for UI feature '$FEATURE'.

     Read ui/ changes for this feature.
     Write tests to ui/test/$FEATURE.test.tsx.
     Cover all five WebSocket states if applicable.
     Follow all rules in CLAUDE.md."

  # Sync test output to reviewer worktrees
  sync_worktree "$WORKTREES_DIR/backend-tester" "test: $FEATURE backend tests" \
    "$WORKTREES_DIR/reviewer" \
    -- tests

  sync_worktree "$WORKTREES_DIR/ui-tester" "test: $FEATURE UI tests" \
    "$WORKTREES_DIR/design-reviewer" \
    -- ui/test

  save_state "feature-$FEATURE" 4
  log_ok "Tests written"
  echo ""
fi

# ── Step 4: Reviewer + Design-reviewer (parallel) ────────────────────────────

if should_run_step 4 "$FROM_STEP"; then
  log_step "4/6" "Reviewer + Design-reviewer — reviewing in parallel..."

  run_parallel "$MAX_RETRIES" \
    "$WORKTREES_DIR/reviewer" \
    "Review Rust code changes for feature '$FEATURE'.

     Check src/ and tests/ for correctness.
     Output your full review to docs/reviews/$FEATURE.md.
     Use [BLOCK], [SUGGEST], [NOTE] tags as defined in CLAUDE.md.
     Include a VERDICT line at the end." \
    "$WORKTREES_DIR/design-reviewer" \
    "Review UI code changes for feature '$FEATURE'.

     Check ui/ changes for correctness.
     Output your full review to docs/reviews/ui-$FEATURE.md.
     Use [VISUAL-BLOCK], [UX-BLOCK], [SUGGEST], [NOTE] tags as defined in CLAUDE.md.
     Include a VERDICT line at the end."

  # Sync review output to main repo and qa-lead
  sync_worktree "$WORKTREES_DIR/reviewer" "review: $FEATURE backend review" \
    . "$WORKTREES_DIR/qa-lead" \
    -- docs/reviews

  sync_worktree "$WORKTREES_DIR/design-reviewer" "review: $FEATURE UI review" \
    . "$WORKTREES_DIR/qa-lead" \
    -- docs/reviews

  # Count blocking issues
  BLOCKS=0
  for f in "docs/reviews/$FEATURE.md" "docs/reviews/ui-$FEATURE.md"; do
    if [ -f "$f" ]; then
      COUNT=$(grep -cE "\[(BLOCK|VISUAL-BLOCK|UX-BLOCK)\]" "$f" || true)
      BLOCKS=$((BLOCKS + COUNT))
    fi
  done

  if [ "$BLOCKS" -gt 0 ]; then
    echo ""
    log_fail "$BLOCKS blocking issue(s) found — pipeline stopped."
    echo ""
    grep -hE "\[(BLOCK|VISUAL-BLOCK|UX-BLOCK)\]" \
      "docs/reviews/$FEATURE.md" "docs/reviews/ui-$FEATURE.md" 2>/dev/null || true
    echo ""
    echo "Resolve [BLOCK] items and resume:"
    echo "  ./tools/parallel-feature.sh $FEATURE --from-step 4"
    save_state "feature-$FEATURE" 4
    exit 1
  fi

  save_state "feature-$FEATURE" 5
  log_ok "No blocking issues"
  echo ""
fi

# ── Step 5: QA Lead (serial) ─────────────────────────────────────────────────

if should_run_step 5 "$FROM_STEP"; then
  log_step "5/6" "QA Lead — final quality gate..."

  run_claude "$MAX_RETRIES" "$WORKTREES_DIR/qa-lead" \
    "Run the QA gate for feature '$FEATURE'.

     Review files:
     - docs/reviews/$FEATURE.md
     - docs/reviews/ui-$FEATURE.md

     Run the full merge checklist from CLAUDE.md.
     Output your sign-off or block reason to the PR description draft.
     If any hard-stop condition is met, output BLOCKED and stop."

  save_state "feature-$FEATURE" 6
  echo ""
fi

# ── Step 6: Merge ────────────────────────────────────────────────────────────

if should_run_step 6 "$FROM_STEP"; then
  log_step "6/6" "Merging all worktree changes into feat/$FEATURE..."

  merge_worktrees "feat/$FEATURE" \
    "$WORKTREES_DIR/architect" \
    "$WORKTREES_DIR/backend-dev" \
    "$WORKTREES_DIR/ui-dev" \
    "$WORKTREES_DIR/backend-tester" \
    "$WORKTREES_DIR/ui-tester"

  clear_state "feature-$FEATURE"
  echo ""
fi

echo "========================================"
echo " Pipeline complete: $FEATURE"
echo " Branch: feat/$FEATURE"
echo "========================================"
