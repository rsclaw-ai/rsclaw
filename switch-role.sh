#!/bin/bash

# Configuration
ROLE_DIR=".claude/roles"
TARGET="CLAUDE.md"

# Get available roles for the help message
AVAILABLE_ROLES=$(ls "$ROLE_DIR"/*.md 2>/dev/null | xargs -n 1 basename | sed 's/\.md//g')

ROLE=$1

# 1. Validate Input
if [ -z "$ROLE" ]; then
    echo "Usage: switch-role <role>"
    echo "Available roles: $AVAILABLE_ROLES"
    exit 1
fi

# 2. Check if role exists
if [ -f "$ROLE_DIR/$ROLE.md" ]; then
    # Force remove current CLAUDE.md to avoid permission/symlink issues
    rm -f "$TARGET"
    
    # Copy the new role template
    cp "$ROLE_DIR/$ROLE.md" "$TARGET"
    
    # Optional: If you want to sync this with Git status
    # git add "$TARGET" 
    
    echo "✓ Role successfully switched to: $ROLE"
else
    echo "Error: Role '$ROLE' not found."
    echo "Available roles: $AVAILABLE_ROLES"
    exit 1
fi
