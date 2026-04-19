#!/usr/bin/env python3
"""
Test: compaction summary via KV cache reuse.

Simulates appending a summary instruction to an existing conversation
(with tools defined) and verifies each model:
1. Generates a structured summary (not a tool call)
2. Follows the 8-section format
3. Does NOT call any tools despite tools being present

Usage:
    python tests/test_compaction_kvcache.py
"""

import json, time, os, sys

# --- Config ---
OLLAMA_URL = "http://macstudio.local"

GATE_ROUTER_URL = "https://api.gaterouter.ai/openai/v1"

MODELS = [
    # Direct cloud models (need individual API keys)
    {"name": "deepseek/deepseek-chat", "base": "https://api.deepseek.com/v1", "key_env": "DEEPSEEK_API_KEY"},
    {"name": "doubao/doubao-seed-2-0-pro-260215", "base": "https://ark.cn-beijing.volces.com/api/v3", "key_env": "ARK_API_KEY"},
    # GateRouter models (single key, multiple providers)
    {"name": "google/gemini-2.5-flash", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    {"name": "google/gemini-3-flash-preview", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    {"name": "minimax/minimax-m2.5", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    {"name": "google/gemini-3.1-pro-preview", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    {"name": "z-ai/glm-5-turbo", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    {"name": "moonshotai/kimi-k2.5", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    {"name": "anthropic/claude-sonnet-4.6", "base": GATE_ROUTER_URL, "key_env": "GATE_ROUTER_KEY"},
    # Remote llama.cpp
    {"name": "llama/llama", "base": "http://218.22.75.183:8000/v1", "key_env": "LLAMA_REMOTE_KEY"},
    # Local ollama models
    {"name": "ollama/qwen3.5:9b", "base": OLLAMA_URL, "key_env": None},
    {"name": "ollama/qwen3.5:27b", "base": OLLAMA_URL, "key_env": None},
    {"name": "ollama/gemma4:26b", "base": OLLAMA_URL, "key_env": None},
]

# Simulated multi-turn conversation
CONVERSATION = [
    {"role": "user", "content": "帮我写一个Python爬虫，抓取豆瓣电影Top250的数据"},
    {"role": "assistant", "content": "好的，我来帮你写一个豆瓣电影Top250的爬虫。让我先创建项目结构。"},
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "execute_command", "arguments": "{\"command\":\"mkdir -p douban_spider && cd douban_spider\"}"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "目录已创建"},
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_2", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\":\"douban_spider/spider.py\",\"content\":\"import requests\\nfrom bs4 import BeautifulSoup\\n\\ndef crawl_top250():\\n    results = []\\n    for page in range(0, 250, 25):\\n        url = f'https://movie.douban.com/top250?start={page}'\\n        resp = requests.get(url, headers={'User-Agent': 'Mozilla/5.0'})\\n        soup = BeautifulSoup(resp.text, 'html.parser')\\n        for item in soup.select('.item'):\\n            title = item.select_one('.title').text\\n            rating = item.select_one('.rating_num').text\\n            results.append({'title': title, 'rating': rating})\\n    return results\\n\"}"}}]},
    {"role": "tool", "tool_call_id": "call_2", "content": "文件已写入: douban_spider/spider.py"},
    {"role": "assistant", "content": "爬虫脚本已创建。现在让我运行测试一下。"},
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_3", "type": "function", "function": {"name": "execute_command", "arguments": "{\"command\":\"cd douban_spider && python spider.py\"}"}}]},
    {"role": "tool", "tool_call_id": "call_3", "content": "抓取完成，共250部电影。前3部:\n1. 肖申克的救赎 9.7\n2. 霸王别姬 9.6\n3. 阿甘正传 9.5"},
    {"role": "assistant", "content": "爬虫运行成功！已抓取250部电影数据。前三名是肖申克的救赎(9.7)、霸王别姬(9.6)、阿甘正传(9.5)。需要我把数据导出为CSV吗？"},
    {"role": "user", "content": "好的，导出CSV，然后帮我分析一下评分分布"},
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_4", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\":\"douban_spider/export.py\",\"content\":\"import csv\\nfrom spider import crawl_top250\\n\\ndata = crawl_top250()\\nwith open('top250.csv', 'w', newline='') as f:\\n    writer = csv.DictWriter(f, fieldnames=['title', 'rating'])\\n    writer.writeheader()\\n    writer.writerows(data)\\nprint(f'导出完成: {len(data)}条')\"}"}}]},
    {"role": "tool", "tool_call_id": "call_4", "content": "文件已写入: douban_spider/export.py"},
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_5", "type": "function", "function": {"name": "execute_command", "arguments": "{\"command\":\"cd douban_spider && python export.py\"}"}}]},
    {"role": "tool", "tool_call_id": "call_5", "content": "导出完成: 250条"},
    {"role": "assistant", "content": "CSV已导出。让我分析评分分布。"},
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_6", "type": "function", "function": {"name": "execute_command", "arguments": "{\"command\":\"cd douban_spider && python -c \\\"import csv; data=[float(r['rating']) for r in csv.DictReader(open('top250.csv'))]; print(f'平均:{sum(data)/len(data):.1f}'); print(f'最高:{max(data)}'); print(f'最低:{min(data)}'); bins={f'{i/10:.0f}分段':sum(1 for d in data if i/10<=d<(i+1)/10) for i in range(80,100)}; print(bins)\\\"\"}"}}]},
    {"role": "tool", "tool_call_id": "call_6", "content": "平均:8.9\n最高:9.7\n最低:8.2\n{'8分段': 45, '9分段': 205}"},
    {"role": "assistant", "content": "分析完成！豆瓣Top250评分分布:\n- 平均分: 8.9\n- 最高: 9.7 (肖申克的救赎)\n- 最低: 8.2\n- 9分以上: 205部 (82%)\n- 8分段: 45部 (18%)\n\n大部分影片都在9分以上，说明Top250的门槛很高。"},
    {"role": "user", "content": "不错！再帮我加一个功能：抓取每部电影的导演和年份信息"},
]

# Tools definition (same as what RsClaw sends)
TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "execute_command",
            "description": "Run a shell command",
            "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}
        }
    },
    {
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Write content to a file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}
        }
    },
    {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }
    },
    {
        "type": "function",
        "function": {
            "name": "memory",
            "description": "Search or store long-term memory",
            "parameters": {"type": "object", "properties": {"action": {"type": "string"}, "query": {"type": "string"}}, "required": ["action"]}
        }
    },
    {
        "type": "function",
        "function": {
            "name": "web_search",
            "description": "Search the web",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}
        }
    },
]

SUMMARY_INSTRUCTION = """请将以上对话压缩为结构化摘要。不要调用任何工具，只输出文本摘要。

按以下格式输出:

## Active Task
[当前正在做什么]

## Goal
[最终目标]

## Completed
[已完成的操作和结果]

## In Progress
[正在进行、未完成的]

## Key Data
[必须原样保留的值: 路径/文件名/数字/关键输出]

## Decisions
[已做的技术决策]

## Pending
[等用户确认的、被阻塞的]

## Files
[涉及的文件和修改]

注意: 只输出摘要文本，不要调用任何工具。"""


def call_openai_compat(name: str, base: str, key: str, model_id: str, messages: list) -> dict:
    """Call OpenAI-compatible API."""
    import urllib.request

    payload = {
        "model": model_id,
        "messages": messages,
        "tools": TOOLS,
        "stream": False,
        "max_tokens": 2000,
        "temperature": 0,
    }
    # Disable thinking for providers that support it
    if "doubao" in name or "ark" in base:
        payload["thinking"] = {"type": "disabled"}

    body = json.dumps(payload).encode()

    req = urllib.request.Request(
        f"{base}/chat/completions",
        data=body,
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
        },
    )

    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            data = json.loads(resp.read())
    except urllib.request.HTTPError as e:
        err_body = e.read().decode(errors="replace")[:500]
        raise RuntimeError(f"HTTP {e.code}: {err_body}") from e
    elapsed = time.perf_counter() - t0

    choice = data.get("choices", [{}])[0]
    msg = choice.get("message", {})
    return {
        "has_tool_call": bool(msg.get("tool_calls")),
        "content": msg.get("content") or "",
        "usage": data.get("usage", {}),
        "time": elapsed,
    }


def call_ollama_native(name: str, base: str, model_id: str, messages: list) -> dict:
    """Call Ollama native /api/chat (supports think=false)."""
    import urllib.request

    # Convert OpenAI tool format to Ollama format
    ollama_tools = []
    for t in TOOLS:
        ollama_tools.append({
            "type": "function",
            "function": t["function"],
        })

    # Ollama requires tool_call arguments as objects, not strings.
    # Also remove tool_call_id from tool messages (use content only).
    ollama_msgs = []
    for m in messages:
        msg = dict(m)
        if "tool_calls" in msg and msg["tool_calls"]:
            new_calls = []
            for tc in msg["tool_calls"]:
                tc = dict(tc)
                func = dict(tc.get("function", {}))
                args = func.get("arguments", "{}")
                if isinstance(args, str):
                    try:
                        func["arguments"] = json.loads(args)
                    except json.JSONDecodeError:
                        func["arguments"] = {}
                tc["function"] = func
                new_calls.append(tc)
            msg["tool_calls"] = new_calls
            if msg.get("content") is None:
                msg["content"] = ""
        ollama_msgs.append(msg)

    body = json.dumps({
        "model": model_id,
        "messages": ollama_msgs,
        "tools": ollama_tools,
        "stream": False,
        "think": False,
        "options": {"temperature": 0, "num_predict": 2000},
    }).encode()

    req = urllib.request.Request(
        f"{base}/api/chat",
        data=body,
        headers={"Content-Type": "application/json"},
    )

    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=300) as resp:
        data = json.loads(resp.read())
    elapsed = time.perf_counter() - t0

    msg = data.get("message", {})
    return {
        "has_tool_call": bool(msg.get("tool_calls")),
        "content": msg.get("content") or "",
        "usage": {
            "prompt_tokens": data.get("prompt_eval_count", 0),
            "completion_tokens": data.get("eval_count", 0),
        },
        "time": elapsed,
    }


def call_model(model_cfg: dict) -> dict:
    """Call a model and return result dict."""
    name = model_cfg["name"]
    base = model_cfg["base"]
    is_ollama = name.startswith("ollama/")

    model_id = name.split("/", 1)[1] if "/" in name else name

    messages = list(CONVERSATION)
    messages.append({"role": "user", "content": SUMMARY_INSTRUCTION})

    try:
        if is_ollama:
            r = call_ollama_native(name, base, model_id, messages)
        else:
            key = os.environ.get(model_cfg["key_env"] or "", "") or ""
            r = call_openai_compat(name, base, key, model_id, messages)
    except Exception as e:
        return {"name": name, "error": str(e), "time": 0}

    # Strip any residual <think> tags from content
    import re
    content = re.sub(
        r"<(?:think|thinking|reasoning)>[\s\S]*?</(?:think|thinking|reasoning)>",
        "", r["content"], flags=re.IGNORECASE,
    ).strip()

    # Check summary quality
    sections_found = []
    for section in ["Active Task", "Goal", "Completed", "In Progress", "Key Data", "Decisions", "Pending", "Files"]:
        if section.lower() in content.lower():
            sections_found.append(section)

    return {
        "name": name,
        "time": r["time"],
        "has_tool_call": r["has_tool_call"],
        "content_len": len(content),
        "sections": len(sections_found),
        "sections_found": sections_found,
        "usage": r["usage"],
        "content_preview": content[:500] if content else "(empty)",
        "error": None,
    }


def main():
    print("=" * 70)
    print("  Compaction Summary via KV Cache Reuse - Model Test")
    print("=" * 70)
    print(f"  Conversation: {len(CONVERSATION)} messages")
    print(f"  Tools: {len(TOOLS)} defined")
    print(f"  Test: append summary instruction, expect text (not tool call)")
    print()

    results = []
    for cfg in MODELS:
        name = cfg["name"]
        key_env = cfg["key_env"]
        if key_env and not os.environ.get(key_env):
            print(f"  SKIP {name} ({key_env} not set)")
            continue
        if "ollama" in name:
            # Quick check if ollama is reachable
            try:
                import urllib.request
                urllib.request.urlopen(f"{OLLAMA_URL}/api/tags", timeout=3)
            except Exception:
                print(f"  SKIP {name} (ollama not reachable)")
                continue

        print(f"  Testing {name}...", end=" ", flush=True)
        r = call_model(cfg)
        results.append(r)

        if r["error"]:
            print(f"ERROR: {r['error'][:80]}")
        else:
            status = "TOOL_CALL!" if r["has_tool_call"] else "OK"
            print(f"{status}  {r['time']:.1f}s  sections={r['sections']}/8  "
                  f"len={r['content_len']}  "
                  f"tokens={r['usage'].get('prompt_tokens', '?')}/{r['usage'].get('completion_tokens', '?')}")

    print()
    print("=" * 70)
    print("  Results Summary")
    print("=" * 70)
    print(f"  {'Model':<40} {'Time':>6} {'Tool?':>6} {'Sect':>5} {'Len':>6}")
    print(f"  {'-'*40} {'-'*6} {'-'*6} {'-'*5} {'-'*6}")
    for r in results:
        if r["error"]:
            print(f"  {r['name']:<40} {'ERROR':>6}")
        else:
            tc = "YES!" if r["has_tool_call"] else "no"
            print(f"  {r['name']:<40} {r['time']:>5.1f}s {tc:>6} {r['sections']:>4}/8 {r['content_len']:>6}")

    print()
    # Print detailed output for each model
    for r in results:
        if r["error"] or not r.get("content_preview"):
            continue
        print(f"--- {r['name']} ---")
        print(r["content_preview"])
        if r["content_len"] > 500:
            print(f"  ... ({r['content_len']} chars total)")
        print()

    # Verdict
    print("=" * 70)
    print("  Verdict")
    print("=" * 70)
    for r in results:
        if r["error"]:
            verdict = "SKIP (error)"
        elif r["has_tool_call"]:
            verdict = "FAIL - called tools instead of generating summary"
        elif r["sections"] >= 6:
            verdict = "PASS - good structured summary"
        elif r["sections"] >= 4:
            verdict = "PARTIAL - some sections missing"
        elif r["content_len"] > 100:
            verdict = "WEAK - generated text but poor structure"
        else:
            verdict = "FAIL - empty or useless response"
        print(f"  {r['name']:<40} {verdict}")


# --- Iterative compaction test ---

# New conversation turns that happen AFTER the first compaction
POST_COMPACT_TURNS = [
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_7", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\":\"douban_spider/spider.py\",\"content\":\"import requests\\nfrom bs4 import BeautifulSoup\\n\\ndef crawl_top250():\\n    results = []\\n    for page in range(0, 250, 25):\\n        url = f'https://movie.douban.com/top250?start={page}'\\n        resp = requests.get(url, headers={'User-Agent': 'Mozilla/5.0'})\\n        soup = BeautifulSoup(resp.text, 'html.parser')\\n        for item in soup.select('.item'):\\n            title = item.select_one('.title').text\\n            rating = item.select_one('.rating_num').text\\n            info = item.select_one('.bd p').text.strip()\\n            director = info.split('\\\\n')[0].replace('导演:', '').strip()\\n            year = info.split('\\\\n')[1].strip()[:4]\\n            results.append({'title': title, 'rating': rating, 'director': director, 'year': year})\\n    return results\"}"}}]},
    {"role": "tool", "tool_call_id": "call_7", "content": "文件已更新: douban_spider/spider.py"},
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_8", "type": "function", "function": {"name": "execute_command", "arguments": "{\"command\":\"cd douban_spider && python spider.py\"}"}}]},
    {"role": "tool", "tool_call_id": "call_8", "content": "抓取完成，共250部电影。前3部:\n1. 肖申克的救赎 9.7 导演:弗兰克·德拉邦特 年份:1994\n2. 霸王别姬 9.6 导演:陈凯歌 年份:1993\n3. 阿甘正传 9.5 导演:罗伯特·泽米吉斯 年份:1994"},
    {"role": "assistant", "content": "导演和年份抓取成功！已更新spider.py。现在每部电影都有标题、评分、导演和年份信息了。"},
    {"role": "user", "content": "很好！现在把导出脚本也更新一下，加上导演和年份列"},
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_9", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\":\"douban_spider/export.py\",\"content\":\"import csv\\nfrom spider import crawl_top250\\n\\ndata = crawl_top250()\\nwith open('top250.csv', 'w', newline='') as f:\\n    writer = csv.DictWriter(f, fieldnames=['title', 'rating', 'director', 'year'])\\n    writer.writeheader()\\n    writer.writerows(data)\\nprint(f'导出完成: {len(data)}条')\"}"}}]},
    {"role": "tool", "tool_call_id": "call_9", "content": "文件已更新: douban_spider/export.py"},
    {"role": "assistant", "content": "导出脚本已更新，CSV现在包含title, rating, director, year四列。需要运行测试吗？"},
    {"role": "user", "content": "运行一下，然后统计一下导演出现次数最多的前5名"},
]

ITERATIVE_SUMMARY_INSTRUCTION = """请更新以下摘要，整合新增的对话内容。不要调用任何工具，只输出文本摘要。

规则:
- 保留旧摘要中仍然有效的信息
- 将已完成的工作从"In Progress"移到"Completed"
- 新增已完成的操作到 Completed 列表（继续编号）
- 更新 Active Task 为最新的未完成请求
- 更新 Key Data 中变化的数据
- 更新 Files 中新增或修改的文件

按以下格式输出:

## Active Task
[当前正在做什么]

## Goal
[最终目标]

## Completed
[已完成的操作和结果]

## In Progress
[正在进行、未完成的]

## Key Data
[必须原样保留的值]

## Decisions
[已做的技术决策]

## Pending
[等用户确认的、被阻塞的]

## Files
[涉及的文件和修改]

注意: 只输出摘要文本，不要调用任何工具。"""


def call_model_iterative(model_cfg: dict, first_summary: str) -> dict:
    """Test iterative compaction: [head 3] + [first summary] + [new turns] + [update instruction]."""
    name = model_cfg["name"]
    base = model_cfg["base"]
    is_ollama = name.startswith("ollama/")
    model_id = name.split("/", 1)[1] if "/" in name else name

    # Build: head(2) + summary + new turns (text only) + update instruction
    # Only take first 2 messages (user + assistant text) to avoid
    # orphaned tool_calls that some providers reject.
    # Convert tool_call turns to plain text descriptions so all providers
    # can process them without strict tool_call pairing requirements.
    head = CONVERSATION[:2]  # user + assistant text reply
    summary_msg = {"role": "user", "content": f"[Conversation history compacted]\n{first_summary}"}
    new_turns = []
    for m in POST_COMPACT_TURNS:
        if m.get("tool_calls"):
            # Convert tool_call to text description
            descs = []
            for tc in m["tool_calls"]:
                fn = tc.get("function", {})
                descs.append(f"[Called {fn.get('name','?')}({fn.get('arguments','')[:80]})]")
            new_turns.append({"role": "assistant", "content": " ".join(descs)})
        elif m.get("role") == "tool":
            # Convert tool result to assistant text
            new_turns.append({"role": "assistant", "content": f"[Tool result] {m.get('content','')}"})
        else:
            new_turns.append(m)

    messages = head + [summary_msg] + new_turns
    messages.append({"role": "user", "content": ITERATIVE_SUMMARY_INSTRUCTION})

    try:
        if is_ollama:
            r = call_ollama_native(name, base, model_id, messages)
        else:
            key = os.environ.get(model_cfg["key_env"] or "", "") or ""
            r = call_openai_compat(name, base, key, model_id, messages)
    except Exception as e:
        return {"name": name, "error": str(e), "time": 0}

    import re
    content = re.sub(
        r"<(?:think|thinking|reasoning)>[\s\S]*?</(?:think|thinking|reasoning)>",
        "", r["content"], flags=re.IGNORECASE,
    ).strip()

    # Check: did it preserve old info AND add new info?
    has_old_data = "8.9" in content or "250" in content  # old stats preserved
    has_new_data = "导演" in content or "director" in content.lower() or "年份" in content  # new feature
    has_new_completed = "spider.py" in content and ("更新" in content or "修改" in content or "导演" in content)

    sections_found = []
    for section in ["Active Task", "Goal", "Completed", "In Progress", "Key Data", "Decisions", "Pending", "Files"]:
        if section.lower() in content.lower():
            sections_found.append(section)

    return {
        "name": name,
        "time": r["time"],
        "has_tool_call": r["has_tool_call"],
        "content_len": len(content),
        "sections": len(sections_found),
        "has_old_data": has_old_data,
        "has_new_data": has_new_data,
        "has_new_completed": has_new_completed,
        "usage": r["usage"],
        "content_preview": content[:600] if content else "(empty)",
        "error": None,
    }


def test_iterative(first_results: list):
    """Run iterative compaction test using first summary results."""
    print()
    print("=" * 70)
    print("  Iterative Compaction Test (2nd summary updates 1st)")
    print("=" * 70)
    print(f"  Head: 3 messages | New turns: {len(POST_COMPACT_TURNS)} messages")
    print(f"  Test: update existing summary with new progress")
    print()

    for r in first_results:
        if r["error"] or not r.get("content_preview"):
            continue
        name = r["name"]
        cfg = next((m for m in MODELS if m["name"] == name), None)
        if not cfg:
            continue

        first_summary = r["content_preview"]
        if r["content_len"] > 500:
            # Need full content - reuse preview (truncated but good enough for test)
            first_summary = r["content_preview"]

        print(f"  Testing {name}...", end=" ", flush=True)
        ir = call_model_iterative(cfg, first_summary)

        if ir["error"]:
            print(f"ERROR: {ir['error'][:80]}")
        else:
            old = "yes" if ir["has_old_data"] else "NO!"
            new = "yes" if ir["has_new_data"] else "NO!"
            tc = "TOOL!" if ir["has_tool_call"] else "ok"
            print(f"{tc}  {ir['time']:.1f}s  sect={ir['sections']}/8  "
                  f"old_data={old}  new_data={new}  len={ir['content_len']}")

    print()
    print("  Legend: old_data=preserved stats from 1st summary, new_data=incorporated new features")


def main():
    print("=" * 70)
    print("  Compaction Summary via KV Cache Reuse - Model Test")
    print("=" * 70)
    print(f"  Conversation: {len(CONVERSATION)} messages")
    print(f"  Tools: {len(TOOLS)} defined")
    print(f"  Test: append summary instruction, expect text (not tool call)")
    print()

    results = []
    for cfg in MODELS:
        name = cfg["name"]
        key_env = cfg["key_env"]
        if key_env and not os.environ.get(key_env):
            print(f"  SKIP {name} ({key_env} not set)")
            continue
        if "ollama" in name:
            try:
                import urllib.request
                urllib.request.urlopen(f"{OLLAMA_URL}/api/tags", timeout=3)
            except Exception:
                print(f"  SKIP {name} (ollama not reachable)")
                continue

        print(f"  Testing {name}...", end=" ", flush=True)
        r = call_model(cfg)
        results.append(r)

        if r["error"]:
            print(f"ERROR: {r['error'][:80]}")
        else:
            status = "TOOL_CALL!" if r["has_tool_call"] else "OK"
            print(f"{status}  {r['time']:.1f}s  sections={r['sections']}/8  "
                  f"len={r['content_len']}  "
                  f"tokens={r['usage'].get('prompt_tokens', '?')}/{r['usage'].get('completion_tokens', '?')}")

    # Print summary table
    print()
    print("=" * 70)
    print("  Round 1 Results")
    print("=" * 70)
    print(f"  {'Model':<40} {'Time':>6} {'Tool?':>6} {'Sect':>5} {'Len':>6}")
    print(f"  {'-'*40} {'-'*6} {'-'*6} {'-'*5} {'-'*6}")
    for r in results:
        if r["error"]:
            print(f"  {r['name']:<40} {'ERROR':>6}")
        else:
            tc = "YES!" if r["has_tool_call"] else "no"
            print(f"  {r['name']:<40} {r['time']:>5.1f}s {tc:>6} {r['sections']:>4}/8 {r['content_len']:>6}")

    # Round 2: iterative compaction
    test_iterative(results)

    print()
    print(f"{'='*70}\nDone  {time.strftime('%H:%M:%S')}")


if __name__ == "__main__":
    main()
