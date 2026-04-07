#!/bin/bash
# switch-role.sh — Activate a Claude Code sub-agent role
# Usage: ./switch-role.sh <role>
# Place this file in the project root.

ROLES_DIR=".claude/roles"
ROLES="architect backend-dev ui-dev backend-tester ui-tester reviewer design-reviewer qa-lead"

if [ -z "$1" ]; then
  echo "Usage: ./switch-role.sh <role>"
  echo ""
  echo "Available roles:"
  for r in $ROLES; do
    echo "  $r"
  done
  exit 1
fi

ROLE=$1
ROLE_FILE="$ROLES_DIR/$ROLE.md"

if [ ! -f "$ROLE_FILE" ]; then
  echo "Error: role file not found at $ROLE_FILE"
  echo ""
  echo "Available roles:"
  for r in $ROLES; do
    echo "  $r"
  done
  exit 1
fi

cp "$ROLE_FILE" CLAUDE.md
echo "✓ Switched to role: $ROLE"
echo "  CLAUDE.md updated — restart Claude Code to apply."
