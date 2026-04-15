#!/bin/bash

# Switch the active sub-agent role for this working directory.
# In the main repo, the root CLAUDE.md is the project-level context and
# should NOT be overwritten. This script is intended for git worktrees
# created by init-worktrees.sh / parallel-*.sh where each worktree gets
# its own CLAUDE.md with the role definition.

ROLE_DIR=".claude/roles"
TARGET="CLAUDE.md"

AVAILABLE_ROLES=$(ls "$ROLE_DIR"/*.md 2>/dev/null | xargs -n 1 basename | sed 's/\.md//g')

ROLE=$1

if [ -z "$ROLE" ]; then
    echo "Usage: switch-role <role>"
    echo "Available roles: $AVAILABLE_ROLES"
    exit 1
fi

# Safety: refuse to overwrite root CLAUDE.md in the main repo checkout.
if git rev-parse --is-inside-work-tree &>/dev/null; then
    TOPLEVEL=$(git rev-parse --show-toplevel)
    if [ "$(pwd)" = "$TOPLEVEL" ] && [ -z "$(git rev-parse --show-superproject-working-tree 2>/dev/null)" ]; then
        # We are at the repo root and NOT inside a worktree.
        if git worktree list --porcelain | grep -q "worktree $(pwd)$"; then
            : # this is the main worktree — block it
            echo "Error: refusing to overwrite root CLAUDE.md in the main checkout."
            echo "This script is intended for git worktrees only."
            echo "Hint: use ./tools/init-worktrees.sh first, then cd into a worktree."
            exit 1
        fi
    fi
fi

if [ -f "$ROLE_DIR/$ROLE.md" ]; then
    rm -f "$TARGET"
    cp "$ROLE_DIR/$ROLE.md" "$TARGET"
    echo "Role switched to: $ROLE"
else
    echo "Error: Role '$ROLE' not found."
    echo "Available roles: $AVAILABLE_ROLES"
    exit 1
fi
