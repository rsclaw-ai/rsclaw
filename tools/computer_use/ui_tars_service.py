#!/usr/bin/env python3
"""UI-TARS inference service for RsClaw — persistent process, stdio JSON protocol.

Protocol (line-delimited JSON on stdin/stdout):
  Request:  {"image": "/path/to/screenshot.png", "max_tokens": 400}
  Response: {"ok": true,  "elements": [...]}
  Response: {"ok": false, "error": "..."}

Elements format:
  {"type": "button", "label": "发送", "coords": [450, 780]}

Coords are normalized 0-1000 and must be scaled to screen pixels by the caller.
"""

import json
import os
import re
import sys
import time
from pathlib import Path

# ---------------------------------------------------------------------------
# Model loading
# ---------------------------------------------------------------------------

DEFAULT_MODEL = str(
    Path.home()
    / ".cache/modelscope/hub/models/mlx-community/UI-TARS-1___5-7B-4bit"
)
MODEL_PATH = os.environ.get("UI_TARS_MODEL", DEFAULT_MODEL)

print(f"[ui-tars] Loading model: {MODEL_PATH}", file=sys.stderr, flush=True)
start = time.time()

from mlx_vlm import load, generate

_model, _processor = load(MODEL_PATH)

print(f"[ui-tars] Model loaded in {time.time() - start:.1f}s", file=sys.stderr, flush=True)

# ---------------------------------------------------------------------------
# Prompt template (COMPUTER_USE variant)
# ---------------------------------------------------------------------------

ANALYZE_PROMPT = """You are a GUI agent analyzing a desktop screenshot.

Task: List all interactive UI elements visible on the screen.

For each element, output exactly one line in this format:
- type=<button|input|link|checkbox|dropdown|tab|icon|menu|text|other>, label=<text>, coords=(x,y)

Rules:
- Coordinates are normalized 0-1000 (top-left is 0,0; bottom-right is 1000,1000).
- Only list elements the user can actually click, type into, or interact with.
- Include the element's visible text label if any.
- Output at least 3 elements and at most 20.
- Do NOT explain; only output the numbered list.

Example output:
1. type=button, label=发送, coords=(850,920)
2. type=input, label=搜索, coords=(300,120)
3. type=link, label=设置, coords=(900,50)
"""


# ---------------------------------------------------------------------------
# Response parser
# ---------------------------------------------------------------------------

_ELEMENT_RE = re.compile(
    r"type\s*=\s*(\w+)\s*,\s*label\s*=\s*([^,]+)\s*,\s*coords\s*=\s*\((\d+),(\d+)\)"
)


def parse_elements(text: str) -> list[dict]:
    elements = []
    for match in _ELEMENT_RE.finditer(text):
        el_type = match.group(1).strip().lower()
        label = match.group(2).strip()
        x = int(match.group(3))
        y = int(match.group(4))
        # Clamp to 0-1000
        x = max(0, min(1000, x))
        y = max(0, min(1000, y))
        elements.append({"type": el_type, "label": label, "coords": [x, y]})
    return elements


# ---------------------------------------------------------------------------
# Inference
# ---------------------------------------------------------------------------

def analyze(image_path: str, max_tokens: int = 400) -> dict:
    if not Path(image_path).exists():
        return {"ok": False, "error": f"image not found: {image_path}"}

    response = generate(
        _model,
        _processor,
        prompt=ANALYZE_PROMPT,
        image=image_path,
        temp=0.2,
        max_tokens=max_tokens,
        verbose=False,
    )

    elements = parse_elements(response)
    return {"ok": True, "elements": elements, "raw": response}


# ---------------------------------------------------------------------------
# Main loop — read JSON requests from stdin, write JSON responses to stdout
# ---------------------------------------------------------------------------

def main():
    print("[ui-tars] Ready", file=sys.stderr, flush=True)

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            print(json.dumps({"ok": False, "error": f"invalid JSON: {e}"}), flush=True)
            continue

        image = req.get("image", "")
        max_tokens = req.get("max_tokens", 400)

        try:
            result = analyze(image, max_tokens)
            print(json.dumps(result), flush=True)
        except Exception as e:
            print(json.dumps({"ok": False, "error": str(e)}), flush=True)


if __name__ == "__main__":
    main()
