#!/usr/bin/env bash
# Fetch the BGE-small-zh embedding model into the Tauri bundle resources
# directory so `tauri build` can include it in the app installer. The
# model is too large for git but small enough to ship in the desktop
# bundle. CI release workflows must run this before building the app.
#
# Idempotent: skips when the file is already present and non-empty.
#
# Source order (each falls back to the next on failure):
#   1. `$BGE_MODEL_URL` env (single-zip path; manual override)
#   2. Default gitfast.org mirror zip
#   3. Hugging Face direct (three separate files: model + config + tokenizer)
#
# History: the gitfast.org mirror went 404 on 2026-06-11 during the
# 2026.6.12 desktop release run, taking out all 6 build targets. The
# HF fallback added below is the long-lived backstop — if HF goes
# down too, the local-dir copy at `~/.rsclaw/models/bge-small-zh`
# wins before we ever hit the network.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="$REPO_ROOT/ui/src-tauri/resources/bge-small-zh"
TARGET_FILE="$TARGET_DIR/model.safetensors"
PRIMARY_URL="${BGE_MODEL_URL:-https://gitfast.org/tools/models/bge-small-zh-v1.5.zip}"
HF_BASE="https://huggingface.co/BAAI/bge-small-zh-v1.5/resolve/main"

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

# Helper: portable downloader. Returns non-zero on failure WITHOUT
# triggering `set -e` so callers can branch to a fallback.
fetch() {
  local url="$1"
  local out="$2"
  if command -v curl >/dev/null; then
    curl -fL --retry 3 --retry-delay 2 --connect-timeout 20 -o "$out" "$url"
  elif command -v wget >/dev/null; then
    wget --tries=3 -O "$out" "$url"
  else
    echo "[fetch-bundled-model] need curl or wget" >&2
    return 127
  fi
}

# Use a single temp dir for both archive and extraction so cleanup
# is one rm and the script stays portable across BSD/GNU mktemp
# (git-bash on Windows runners is GNU; macOS is BSD).
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

try_zip_mirror() {
  local url="$1"
  echo "[fetch-bundled-model] attempting zip mirror: $url"
  local tmp_zip="$WORK_DIR/bge-model.zip"
  local extract_dir="$WORK_DIR/extract"
  mkdir -p "$extract_dir"
  if ! fetch "$url" "$tmp_zip"; then
    echo "[fetch-bundled-model] zip download failed: $url"
    return 1
  fi
  if ! unzip -q "$tmp_zip" -d "$extract_dir" 2>/dev/null; then
    echo "[fetch-bundled-model] zip extract failed (truncated or wrong content?)"
    return 1
  fi
  # The zip ships either flat or under a subdirectory; locate the safetensors.
  local weights
  weights="$(find "$extract_dir" -name model.safetensors | head -n1)"
  if [[ -z "$weights" ]]; then
    echo "[fetch-bundled-model] zip did not contain model.safetensors"
    return 1
  fi
  local src_dir
  src_dir="$(dirname "$weights")"
  cp "$src_dir/model.safetensors" "$TARGET_FILE"
  [[ -s "$TARGET_DIR/config.json" ]]    || cp "$src_dir/config.json"    "$TARGET_DIR/"
  [[ -s "$TARGET_DIR/tokenizer.json" ]] || cp "$src_dir/tokenizer.json" "$TARGET_DIR/"
  return 0
}

try_huggingface_direct() {
  echo "[fetch-bundled-model] attempting Hugging Face direct: $HF_BASE"
  # HF serves files via 307s to a signed S3 URL; -fL handles the chain.
  if ! fetch "$HF_BASE/model.safetensors" "$TARGET_FILE"; then
    echo "[fetch-bundled-model] HF model.safetensors download failed"
    return 1
  fi
  if [[ ! -s "$TARGET_DIR/config.json" ]]; then
    fetch "$HF_BASE/config.json" "$TARGET_DIR/config.json" || true
  fi
  if [[ ! -s "$TARGET_DIR/tokenizer.json" ]]; then
    fetch "$HF_BASE/tokenizer.json" "$TARGET_DIR/tokenizer.json" || true
  fi
  return 0
}

# Try mirror first (smaller, faster), fall through to HF on any failure.
if try_zip_mirror "$PRIMARY_URL"; then
  :
elif try_huggingface_direct; then
  :
else
  echo "[fetch-bundled-model] all sources failed" >&2
  exit 1
fi

if [[ ! -s "$TARGET_FILE" ]]; then
  echo "[fetch-bundled-model] target file still missing after fetch attempts" >&2
  exit 1
fi
echo "[fetch-bundled-model] installed -> $TARGET_FILE ($(du -h "$TARGET_FILE" | cut -f1))"
