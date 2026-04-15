#!/bin/bash
# tools/init-worktrees.sh
# Initialize git worktrees for all sub-agent roles.
# Run once after cloning the repository.
#
# Usage: ./tools/init-worktrees.sh

set -euo pipefail

ROLES=(
  architect
  backend-dev
  ui-dev
  backend-tester
  ui-tester
  reviewer
  design-reviewer
  qa-lead
)

ROLES_DIR=".claude/roles"
WORKTREES_DIR=".worktrees"

# Validate roles directory exists
if [ ! -d "$ROLES_DIR" ]; then
  echo "Error: $ROLES_DIR not found."
  echo "Make sure you have the .claude/roles/ directory with role definition files."
  exit 1
fi

# Create worktrees root
mkdir -p "$WORKTREES_DIR"

echo "Initializing worktrees..."

for role in "${ROLES[@]}"; do
  ROLE_FILE="$ROLES_DIR/$role.md"
  WORKTREE_PATH="$WORKTREES_DIR/$role"

  if [ ! -f "$ROLE_FILE" ]; then
    echo "  Warning: role file not found: $ROLE_FILE — skipping $role"
    continue
  fi

  if [ -d "$WORKTREE_PATH" ]; then
    echo "  Skipping $role (worktree already exists)"
    continue
  fi

  git worktree add "$WORKTREE_PATH" HEAD --detach
  cp "$ROLE_FILE" "$WORKTREE_PATH/CLAUDE.md"
  echo "  ✓ $role"
done

echo ""
echo "All worktrees ready at $WORKTREES_DIR/"
echo ""
echo "Next steps:"
echo "  Run a feature:  ./tools/parallel-feature.sh <feature-name>"
echo "  Run a review:   ./tools/review-pipeline.sh <branch-name>"
echo "  Run channels:   ./tools/parallel-channels.sh <ch1> <ch2> ..."
