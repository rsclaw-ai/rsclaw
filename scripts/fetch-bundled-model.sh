#!/usr/bin/env bash
# Fetch the BGE-small-zh embedding model into the Tauri bundle resources
# directory so `tauri build` can include it in the app installer. The
# model is too large for git but small enough to ship in the desktop
# bundle. CI release workflows must run this before building the app.
#
# Idempotent: skips when the file is already present and non-empty.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="$REPO_ROOT/ui/src-tauri/resources/bge-small-zh"
TARGET_FILE="$TARGET_DIR/model.safetensors"
URL="${BGE_MODEL_URL:-https://gitfast.org/tools/models/bge-small-zh-v1.5.zip}"

if [[ -s "$TARGET_FILE" ]]; then
  echo "[fetch-bundled-model] $TARGET_FILE already present ($(du -h "$TARGET_FILE" | cut -f1)), skipping"
  exit 0
fi

mkdir -p "$TARGET_DIR"

# Local fallback: copy from the user's existing model dir if present.
LOCAL_SRC="$HOME/.rsclaw/models/bge-small-zh"
if [[ -s "$LOCAL_SRC/model.safetensors" ]]; then
  echo "[fetch-bundled-model] copying from $LOCAL_SRC"
  cp "$LOCAL_SRC/model.safetensors" "$TARGET_FILE"
  [[ -s "$TARGET_DIR/config.json" ]]    || cp "$LOCAL_SRC/config.json"    "$TARGET_DIR/"
  [[ -s "$TARGET_DIR/tokenizer.json" ]] || cp "$LOCAL_SRC/tokenizer.json" "$TARGET_DIR/"
  echo "[fetch-bundled-model] done"
  exit 0
fi

# Download fresh. Use a single temp dir for both archive and extraction so
# cleanup is one rm and the script stays portable across BSD/GNU mktemp
# (git-bash on Windows runners is GNU; macOS is BSD).
echo "[fetch-bundled-model] downloading $URL"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
TMP_ZIP="$WORK_DIR/bge-model.zip"
EXTRACT_DIR="$WORK_DIR/extract"
mkdir -p "$EXTRACT_DIR"

if command -v curl >/dev/null; then
  curl -fL --retry 3 -o "$TMP_ZIP" "$URL"
elif command -v wget >/dev/null; then
  wget -O "$TMP_ZIP" "$URL"
else
  echo "[fetch-bundled-model] need curl or wget" >&2
  exit 1
fi

unzip -q "$TMP_ZIP" -d "$EXTRACT_DIR"

# The zip ships either flat or under a subdirectory; locate the safetensors.
WEIGHTS="$(find "$EXTRACT_DIR" -name model.safetensors | head -n1)"
if [[ -z "$WEIGHTS" ]]; then
  echo "[fetch-bundled-model] zip did not contain model.safetensors" >&2
  exit 1
fi
SRC_DIR="$(dirname "$WEIGHTS")"
cp "$SRC_DIR/model.safetensors" "$TARGET_FILE"
[[ -s "$TARGET_DIR/config.json" ]]    || cp "$SRC_DIR/config.json"    "$TARGET_DIR/"
[[ -s "$TARGET_DIR/tokenizer.json" ]] || cp "$SRC_DIR/tokenizer.json" "$TARGET_DIR/"
echo "[fetch-bundled-model] installed -> $TARGET_FILE ($(du -h "$TARGET_FILE" | cut -f1))"
