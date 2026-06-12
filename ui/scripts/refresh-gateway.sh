#!/usr/bin/env bash
# Rebuild the `rsclaw` gateway sidecar and bounce any running instance.
#
# Tauri's `app:dev` only watches `src-tauri/` for changes — modifying
# anything under the top-level `src/` (gateway routes, agent
# runtime, memory store, etc.) doesn't trigger a rebuild of the
# sidecar binary the desktop app launches. Stale binaries silently
# 404 newly-added endpoints, which is hard to spot without
# tracing the chain (and we burnt an afternoon on it once).
#
# This script does the three steps that have to happen together:
#
#   1. `cargo build --bin rsclaw` in the workspace root
#   2. Copy the freshly-built binary into BOTH Tauri sidecar paths
#      (`src-tauri/binaries/rsclaw-<triple>` is the canonical one
#      `externalBin` reads from; `src-tauri/target/debug/rsclaw` is
#      the path the dev launcher actually exec's — Tauri's sidecar
#      lookup duplicates here when running `tauri dev`)
#   3. Send SIGTERM to any running `rsclaw gateway run` process so
#      the watchdog respawns it from the fresh binary
#
# Usage:
#   yarn refresh-gateway
#
# Or directly: ./ui/scripts/refresh-gateway.sh
#
# Cross-platform: host triple comes from `rustc -vV`, so it auto-
# resolves on aarch64-apple-darwin / x86_64-apple-darwin / Linux /
# Windows targets without per-machine edits.

set -euo pipefail

# Resolve repo root from this script's location — robust against
# `yarn` running us with cwd = ui/.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cd "$REPO_ROOT"

# Detect the host target triple Tauri uses for the sidecar suffix.
HOST_TRIPLE="$(rustc -vV 2>/dev/null | awk '/^host:/ {print $2}')"
if [ -z "$HOST_TRIPLE" ]; then
  echo "✗ couldn't detect host triple via rustc -vV" >&2
  exit 1
fi

# Embed the workspace version into the sidecar (option_env!"RSCLAW_BUILD_VERSION")
# so the desktop build.rs sidecar-drift check can compare actual ↔ expected.
# Without this, the sidecar reports "dev" and the check skips.
WORKSPACE_VERSION="$(awk -F\" '/^version = / { print $2; exit }' "$REPO_ROOT/Cargo.toml")"
if [ -z "$WORKSPACE_VERSION" ]; then
  echo "✗ couldn't parse workspace version from $REPO_ROOT/Cargo.toml" >&2
  exit 1
fi

echo "→ building rsclaw (triple: $HOST_TRIPLE, version: $WORKSPACE_VERSION)"
RSCLAW_BUILD_VERSION="$WORKSPACE_VERSION" cargo build --bin rsclaw

SRC_BIN="$REPO_ROOT/target/debug/rsclaw"
SIDECAR_BIN="$REPO_ROOT/ui/src-tauri/binaries/rsclaw-$HOST_TRIPLE"
DEV_BIN="$REPO_ROOT/ui/src-tauri/target/debug/rsclaw"

if [ ! -f "$SRC_BIN" ]; then
  echo "✗ build succeeded but $SRC_BIN missing" >&2
  exit 1
fi

mkdir -p "$(dirname "$SIDECAR_BIN")" "$(dirname "$DEV_BIN")"
cp "$SRC_BIN" "$SIDECAR_BIN"
cp "$SRC_BIN" "$DEV_BIN"
echo "→ copied to:"
echo "   $SIDECAR_BIN"
echo "   $DEV_BIN"

# SIGTERM lets the gateway run its shutdown coordinator (flush task
# queue, close channels, persist state). The Tauri-side watchdog
# notices the exit and respawns from the new binary within a couple
# of seconds.
if pgrep -f "rsclaw gateway run" >/dev/null 2>&1; then
  echo "→ stopping running gateway(s) (SIGTERM)"
  pkill -f "rsclaw gateway run" || true
  echo "   watchdog will respawn from the new binary"
else
  echo "→ no gateway running; new binary picks up on next launch"
fi

echo "✓ refresh-gateway done"
