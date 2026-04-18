#!/usr/bin/env python3
"""
Test: prefix_kv_cache=1 — verify KV cache hit with stable system+tools prefix.

Sends 3 sequential requests with identical system+tools, appending one message
each turn. Measures TTFT and checks cached_tokens to verify prefix cache hit.

Usage:
    python tests/test_prefix_kvcache.py
"""

import json, time, os, sys, urllib.request

OLLAMA_URL = "http://macstudio.local"
GATE_ROUTER_URL = "https://api.gaterouter.ai/openai/v1"

# Stable system prompt (never changes between turns)
SYSTEM_PROMPT = """You are a helpful AI assistant. You help users with coding, data analysis, and general questions.
Always respond concisely. Use Chinese when the user speaks Chinese.
Platform: macOS. Shell: bash/zsh."""

# Stable tools (never changes between turns)
TOOLS = [
    {"type": "function", "function": {"name": "execute_command", "description": "Run a shell command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}},
    {"type": "function", "function": {"name": "write_file", "description": "Write content to a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}}},
    {"type": "function", "function": {"name": "read_file", "description": "Read a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}},
    {"type": "function", "function": {"name": "memory", "description": "Search or store long-term memory", "parameters": {"type": "object", "properties": {"action": {"type": "string"}, "query": {"type": "string"}}, "required": ["action"]}}},
    {"type": "function", "function": {"name": "web_search", "description": "Search the web", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}},
]

# Conversation turns — each turn adds one more message pair
TURNS = [
    {"role": "user", "content": "你好，请简单介绍一下自己。"},
    # assistant reply will be added after turn 1
    {"role": "user", "content": "帮我写一个 Python 函数，计算斐波那契数列的第 N 项。"},
    # assistant reply will be added after turn 2
    {"role": "user", "content": "现在修改这个函数，加入缓存优化，使用 lru_cache 装饰器。"},
]

MODELS = [
    # Direct
    {"name": "deepseek/deepseek-chat", "base": "https://api.deepseek.com/v1", "key_env": "DEEPSEEK_API_KEY"},
    {"name": "doubao/doubao-seed-2-0-pro-260215", "base": "https://ark.cn-beijing.volces.com/api/v3", "key_env": "ARK_API_KEY"},
    # GateRouter
    {"name": "google/gemini-2.5-flash", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    {"name": "anthropic/claude-sonnet-4.6", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    # Remote llama
    {"name": "llama/qwen3.5-fast", "base": "http://218.22.75.183:8000/v1", "key_env": "LLAMA_REMOTE_KEY"},
    # Local ollama
    {"name": "ollama/qwen3.5:9b", "base": OLLAMA_URL, "key_env": None},
    {"name": "ollama/gemma4:26b", "base": OLLAMA_URL, "key_env": None},
]


def call_streaming(base: str, key: str, model_id: str, system: str, messages: list,
                   tools: list, is_ollama: bool = False) -> dict:
    """Send a streaming request and measure TTFT + total time."""

    if is_ollama:
        # Ollama native /api/chat
        body = json.dumps({
            "model": model_id,
            "messages": [{"role": "system", "content": system}] + messages,
            "tools": [{"type": "function", "function": t["function"]} for t in tools],
            "stream": True,
            "think": False,
            "options": {"temperature": 0.3, "num_predict": 200},
        }).encode()
        url = f"{base}/api/chat"
        headers = {"Content-Type": "application/json"}
    else:
        payload = {
            "model": model_id,
            "messages": [{"role": "system", "content": system}] + messages,
            "tools": tools,
            "stream": True,
            "max_tokens": 200,
            "temperature": 0.3,
        }
        if "doubao" in model_id or "ark" in base:
            payload["thinking"] = {"type": "disabled"}
        body = json.dumps(payload).encode()
        url = f"{base}/chat/completions"
        headers = {
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
        }

    req = urllib.request.Request(url, data=body, headers=headers)

    t0 = time.perf_counter()
    ttft = None
    content_parts = []
    cached_tokens = None
    prompt_tokens = None
    usage_data = {}

    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            for raw_line in resp:
                line = raw_line.decode(errors="replace").strip()

                if is_ollama:
                    # Ollama streams JSON objects, one per line
                    if not line:
                        continue
                    try:
                        chunk = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    msg = chunk.get("message", {})
                    c = msg.get("content", "")
                    if c and ttft is None:
                        ttft = time.perf_counter() - t0
                    if c:
                        content_parts.append(c)
                    if chunk.get("done"):
                        prompt_tokens = chunk.get("prompt_eval_count")
                        usage_data = {
                            "prompt_tokens": prompt_tokens or 0,
                            "completion_tokens": chunk.get("eval_count", 0),
                            "prompt_eval_ms": chunk.get("prompt_eval_duration", 0) / 1_000_000,
                        }
                        break
                else:
                    # OpenAI SSE format
                    if not line.startswith("data: "):
                        continue
                    data = line[6:].strip()
                    if data == "[DONE]":
                        break
                    try:
                        chunk = json.loads(data)
                    except json.JSONDecodeError:
                        continue
                    # Extract content delta
                    choices = chunk.get("choices", [])
                    if choices:
                        delta = choices[0].get("delta", {})
                        c = delta.get("content", "")
                        if c and ttft is None:
                            ttft = time.perf_counter() - t0
                        if c:
                            content_parts.append(c)
                    # Extract usage (some providers include in last chunk)
                    u = chunk.get("usage")
                    if u:
                        usage_data = u
                        cached_tokens = (u.get("prompt_tokens_details") or {}).get("cached_tokens")

    except Exception as e:
        return {"error": str(e)}

    total = time.perf_counter() - t0
    return {
        "ttft": ttft,
        "total": total,
        "content": "".join(content_parts),
        "cached_tokens": cached_tokens,
        "prompt_tokens": usage_data.get("prompt_tokens"),
        "prompt_eval_ms": usage_data.get("prompt_eval_ms"),
        "usage": usage_data,
        "error": None,
    }


def test_model(cfg: dict):
    """Run 3-turn prefix cache test for one model."""
    name = cfg["name"]
    base = cfg["base"]
    is_ollama = name.startswith("ollama/")
    model_id = name.split("/", 1)[1] if "/" in name else name
    key = os.environ.get(cfg["key_env"] or "", "") or ""

    print(f"\n  {name}")
    print(f"  {'-'*50}")

    messages = []
    results = []

    for turn_idx, turn_msg in enumerate(TURNS):
        messages.append(turn_msg)

        r = call_streaming(base, key, model_id, SYSTEM_PROMPT, messages, TOOLS, is_ollama)

        if r.get("error"):
            print(f"    Turn {turn_idx+1}: ERROR {r['error'][:80]}")
            results.append(r)
            break

        ttft_ms = r["ttft"] * 1000 if r["ttft"] else 0
        cached = r.get("cached_tokens")
        prompt = r.get("prompt_tokens")
        eval_ms = r.get("prompt_eval_ms")

        cached_str = f"cached={cached}" if cached is not None else ""
        prompt_str = f"prompt={prompt}" if prompt else ""
        eval_str = f"eval={eval_ms:.0f}ms" if eval_ms else ""
        extra = "  ".join(filter(None, [prompt_str, cached_str, eval_str]))

        print(f"    Turn {turn_idx+1}: TTFT={ttft_ms:>7.0f}ms  total={r['total']:.1f}s  {extra}")

        results.append(r)

        # Add assistant reply to messages for next turn
        reply_text = r["content"][:200] if r["content"] else "OK"
        messages.append({"role": "assistant", "content": reply_text})

    # Analysis
    if len(results) >= 3 and all(not r.get("error") for r in results):
        ttfts = [r["ttft"] * 1000 for r in results if r.get("ttft")]
        if len(ttfts) >= 2:
            improvement = (1 - ttfts[-1] / ttfts[0]) * 100 if ttfts[0] > 0 else 0
            trend = "FASTER" if improvement > 10 else "STABLE" if improvement > -10 else "SLOWER"
            print(f"    => TTFT trend: {ttfts[0]:.0f}ms -> {ttfts[-1]:.0f}ms ({trend}, {improvement:+.0f}%)")

            # Check cached_tokens
            cached_vals = [r.get("cached_tokens") for r in results if r.get("cached_tokens") is not None]
            if cached_vals:
                print(f"    => Cached tokens: {cached_vals}")

    return results


def main():
    print("=" * 60)
    print("  Prefix KV Cache Test (mode=1)")
    print("=" * 60)
    print(f"  System prompt: {len(SYSTEM_PROMPT)} chars (stable)")
    print(f"  Tools: {len(TOOLS)} (stable)")
    print(f"  Turns: {len(TURNS)} (append-only)")
    print(f"  Expected: TTFT should decrease or stay stable across turns")
    print(f"            (prefix cache hit = no re-prefill of system+tools)")

    for cfg in MODELS:
        key_env = cfg["key_env"]
        if key_env and not os.environ.get(key_env):
            print(f"\n  SKIP {cfg['name']} ({key_env} not set)")
            continue
        if "ollama" in cfg["name"]:
            try:
                urllib.request.urlopen(f"{OLLAMA_URL}/api/tags", timeout=3)
            except Exception:
                print(f"\n  SKIP {cfg['name']} (ollama not reachable)")
                continue
        test_model(cfg)

    print(f"\n{'='*60}")
    print("  Note: TTFT improvement depends on provider's cache implementation.")
    print("  Cloud APIs: automatic prefix caching (OpenAI/DeepSeek/Gemini).")
    print("  Local: depends on llama.cpp/vLLM slot reuse.")
    print(f"{'='*60}")


if __name__ == "__main__":
    main()
