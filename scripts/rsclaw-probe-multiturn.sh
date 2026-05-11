#!/usr/bin/env bash
# rsclaw kvCacheMode=2 multi-turn probe.
#
# Drives a single agent session through many turns against the
# RsClaw gateway's /api/v1/message endpoint. Used to validate:
#   1. The rsclaw provider's open/turn/replay state machine survives
#      a real conversation (no version_drift / parse errors).
#   2. KV cache effectiveness — second and subsequent turns should be
#      noticeably faster than the first because rsclaw-server reuses
#      the prefix slot. Compare the latency column across the script's
#      Phase 1 (cold) and Phase 4 (warm) rows.
#   3. Auto-compaction integrity — the script plants three facts in
#      Phase 1, fills context past the agent's 80% threshold in
#      Phases 2-3 (compaction MUST fire), then recalls each planted
#      fact in Phase 4 to prove the compaction summary preserved
#      them. Phases 5-6 push through 2nd and 3rd compactions and
#      re-recall to verify facts survive repeated rounds.
#
# Prerequisites:
#   - A rsclaw gateway running with an agent named `${AGENT_ID}`
#     configured for `kvCacheMode: 2` and a model that resolves to
#     the `rsclaw` provider (RSCLAW_URL + RSCLAW_KEY env vars or
#     a `rsclaw/<id>` model alias). Set `contextTokens: 32000` on
#     the agent's defaults to make compaction trip within the
#     script's prompt budget.
#   - `jq` and `python3` on PATH (timing math + JSON wrangling).
#
# Usage:
#   scripts/rsclaw-probe-multiturn.sh
#   GATEWAY=http://127.0.0.1:18891 AGENT_ID=rsctest \
#       scripts/rsclaw-probe-multiturn.sh
#
# Environment:
#   GATEWAY      Gateway base URL (default: http://127.0.0.1:18888).
#   AGENT_ID     Agent id from config.agents.list (default: rsctest).
#   SESSION_KEY  Session id; defaults to a fresh `multiturn:<unix-ts>`.
#   TURN_TIMEOUT Per-turn HTTP timeout, seconds (default: 240).
#
# Exit code is 0 if every turn returns a 2xx + non-empty reply.
# Non-zero indicates at least one turn failed — useful in CI.

set -euo pipefail

GATEWAY="${GATEWAY:-http://127.0.0.1:18888}"
AGENT_ID="${AGENT_ID:-rsctest}"
SESSION_KEY="${SESSION_KEY:-multiturn:$(date +%s)}"
TURN_TIMEOUT="${TURN_TIMEOUT:-240}"

# Per-turn long-context filler. Four ~1400-char blocks concatenated
# (~5600 chars ≈ 1300 tokens) so each fill / push / more turn adds
# enough to push the session past the 80% threshold within Phase
# 3. Without this scaling, 1400-char prompts only contribute ~250
# session tokens per turn pair and compaction wouldn't fire within
# any reasonable turn budget.
FILLER_BLOCK='Background context (please remember verbatim): The Mariana Trench reaches 10994 meters at Challenger Deep. The Voyager 1 spacecraft entered interstellar space on 25 August 2012. Marie Curie won Nobel Prizes in 1903 (physics) and 1911 (chemistry). The Antikythera mechanism dates to roughly 100 BCE. The Library of Alexandria likely burned in stages between 48 BCE and the 7th century. The first commercial transistor radio (Regency TR-1) shipped in 1954 for $49.95. The Burj Khalifa is 828 meters tall and opened in 2010. The deepest known cave is Veryovkina in Abkhazia at 2212 meters. The South Pole receives no sunlight from late March to late September. The blue whale heart can weigh 180 kg and the aorta is wide enough for a small child to swim through.'
FILLER="${FILLER_BLOCK} ${FILLER_BLOCK} ${FILLER_BLOCK} ${FILLER_BLOCK}"

# Long-answer prompt suffix — asks the model to echo many facts so
# assistant tokens also contribute to session growth. Combined with
# FILLER, each turn pair adds ~1500-2000 tokens to the session and
# the 25.6k threshold (80% of 32k context) is crossed within 6-8
# fill turns rather than 50+.
LONG_ANSWER='List all ten background facts above in full sentences, then summarise each one in turn. Do not skip any. Use complete English sentences.'

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: required command '$1' not found on PATH" >&2
    exit 2
  }
}
require_cmd curl
require_cmd jq
require_cmd python3

TURN=0
FAILED=0

# Run a single turn against the gateway, print a timing + reply
# summary, and bump $FAILED on any error.
#
# Args:  $1 = phase label (≤ 9 chars), $2 = user prompt text.
run_turn() {
  local label="$1"
  local prompt="$2"
  TURN=$((TURN + 1))

  local t0
  t0=$(python3 -c 'import time; print(time.time())')

  local body
  body=$(jq -nc \
    --arg agent "${AGENT_ID}" \
    --arg sk    "${SESSION_KEY}" \
    --arg text  "${prompt}" \
    '{agent_id: $agent, session_key: $sk, text: $text, stream: false}')

  local resp http_code
  resp=$(curl -sS --max-time "${TURN_TIMEOUT}" \
    -H "Content-Type: application/json" \
    -w '\n%{http_code}' \
    "${GATEWAY}/api/v1/message" \
    -d "${body}" \
    2>&1) || {
      printf "T%-2d [%-9s] %6s | curl failed: %s\n" "${TURN}" "${label}" "ERR" "${resp}"
      FAILED=$((FAILED + 1))
      return
    }

  http_code="${resp##*$'\n'}"
  local payload="${resp%$'\n'*}"

  local t1
  t1=$(python3 -c 'import time; print(time.time())')

  local ms
  ms=$(python3 -c "print(int((${t1} - ${t0}) * 1000))")

  if [[ "${http_code}" != "200" ]]; then
    printf "T%-2d [%-9s] %6d ms | HTTP %s — %s\n" \
      "${TURN}" "${label}" "${ms}" "${http_code}" "${payload:0:160}"
    FAILED=$((FAILED + 1))
    return
  fi

  local reply
  reply=$(printf '%s' "${payload}" \
    | python3 -c 'import json,sys
d = json.load(sys.stdin)
r = (d.get("reply") or "").replace("\n", " ").strip()
print(r[:200])
' 2>/dev/null || echo "<unparseable>")

  if [[ -z "${reply}" ]]; then
    printf "T%-2d [%-9s] %6d ms | (empty reply)\n" "${TURN}" "${label}" "${ms}"
    FAILED=$((FAILED + 1))
    return
  fi

  printf "T%-2d [%-9s] %6d ms | %s\n" "${TURN}" "${label}" "${ms}" "${reply}"
}

echo "gateway   : ${GATEWAY}"
echo "agent_id  : ${AGENT_ID}"
echo "session   : ${SESSION_KEY}"
echo "timeout   : ${TURN_TIMEOUT}s per turn"
echo

echo "==== Phase 1: plant three facts (compaction summary MUST preserve these) ===="
run_turn "plant-1"  "Remember this for later: my favourite prime number is 7919. Reply only: noted."
run_turn "plant-2"  "Remember this for later: my dog's name is Salem. Reply only: noted."
run_turn "plant-3"  "Remember this for later: I live in Reykjavik, Iceland. Reply only: noted."

echo
echo "==== Phase 2: fill context with long-form prompts ===="
run_turn "fill-1" "${FILLER}  ${LONG_ANSWER}"
run_turn "fill-2" "${FILLER}  ${LONG_ANSWER}"
run_turn "fill-3" "${FILLER}  ${LONG_ANSWER}"
run_turn "fill-4" "${FILLER}  ${LONG_ANSWER}"

echo
echo "==== Phase 3: push past 80% threshold — first compaction expected ===="
run_turn "push-1" "${FILLER}  ${LONG_ANSWER}"
run_turn "push-2" "${FILLER}  ${LONG_ANSWER}"

echo
echo "==== Phase 4: recall planted facts after compaction ===="
run_turn "recall-1" "What favourite prime number did I tell you in my first message?"
run_turn "recall-2" "What is my dog's name?"
run_turn "recall-3" "Which city do I live in?"

echo
echo "==== Phase 5: keep going — 2nd compaction expected ===="
run_turn "more-1" "${FILLER}  ${LONG_ANSWER}"
run_turn "more-2" "${FILLER}  ${LONG_ANSWER}"
run_turn "more-3" "${FILLER}  ${LONG_ANSWER}"

echo
echo "==== Phase 6: recall again after 2nd compaction ===="
run_turn "recall-4" "Remind me — what was my favourite prime number?"
run_turn "recall-5" "Remind me — what city do I live in?"

echo
echo "==== Phase 7: keep going — 3rd compaction expected ===="
run_turn "final-1" "${FILLER}  ${LONG_ANSWER}"
run_turn "final-2" "${FILLER}  ${LONG_ANSWER}"
run_turn "final-3" "${FILLER}  ${LONG_ANSWER}"

echo
echo "==== Phase 8: final recall — must survive three compactions ===="
run_turn "recall-6" "What was my dog's name and which city do I live in?"
run_turn "recall-7" "Remind me of the favourite prime number from my very first message."

echo
if (( FAILED == 0 )); then
  echo "==== done — ${TURN} turns, 0 failures ===="
  exit 0
fi
echo "==== done — ${TURN} turns, ${FAILED} failure(s) ====" >&2
exit 1
