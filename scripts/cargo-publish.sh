#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

export RSCLAW_BUILD_VERSION="${RSCLAW_BUILD_VERSION:-dev}"
export RSCLAW_BUILD_DATE="${RSCLAW_BUILD_DATE:-test}"

# Publish in dependency order. The root package is published last.
CRATES=(
  rsclaw-a2a-types
  rsclaw-config
  rsclaw-events
  rsclaw-i18n
  rsclaw-types
  rsclaw-util
  rsclaw-artifact
  rsclaw-cli
  rsclaw-doc
  rsclaw-retry
  rsclaw-evolution
  rsclaw-embed
  rsclaw-platform
  rsclaw-desktop
  rsclaw-store
  rsclaw-browser
  rsclaw-provider
  rsclaw-cron
  rsclaw-mcp
  rsclaw-memory
  rsclaw-cap
  rsclaw-channel
  rsclaw-computer
  rsclaw-jobs
  rsclaw-kb
  rsclaw-migrate
  rsclaw-skill
  rsclaw-watch
  rsclaw-heartbeat
  rsclaw-plugin
  rsclaw-tools
  rsclaw-agent
  rsclaw-runtime
)

MAX_RETRIES="${PUBLISH_MAX_RETRIES:-15}"
RETRY_DELAY="${PUBLISH_RETRY_DELAY:-60}"

publish_package() {
  local name="$1"
  local dir="$2"
  local attempt=1
  local output

  while (( attempt <= MAX_RETRIES )); do
    echo ">>> Publishing ${name} (attempt ${attempt}/${MAX_RETRIES})..."

    if output=$(cd "$dir" && cargo publish --no-verify --allow-dirty 2>&1); then
      printf '%s\n' "$output"
      echo "<<< ${name} published"
      return 0
    fi

    printf '%s\n' "$output"
    if grep -qi 'already exists' <<<"$output"; then
      echo "!!! ${name} version already exists on crates.io; bump every changed crate version before publishing"
      return 1
    fi

    if grep -q '429 Too Many Requests' <<<"$output"; then
      if (( attempt == MAX_RETRIES )); then
        break
      fi
      echo "--- Rate limited; retrying in ${RETRY_DELAY}s ---"
      sleep "$RETRY_DELAY"
      ((attempt += 1))
      continue
    fi

    echo "!!! Failed to publish ${name}"
    return 1
  done

  echo "!!! Failed to publish ${name} after ${MAX_RETRIES} attempts"
  return 1
}

for crate in "${CRATES[@]}"; do
  publish_package "$crate" "$ROOT/crates/$crate"
done

publish_package "rsclaw" "$ROOT"
echo "=== All crates published ==="
