---
name: yuanbao
description: Tencent Yuanbao (腾讯元宝) desktop app - chat, deep thinking, tools, document/PPTX generation
triggers: [yuanbao, 元宝, 腾讯元宝]
---

# Tencent Yuanbao Desktop (元宝) — Layout & Pitfalls

## App framework

- Bundle: `元宝.app` (process name `yuanbao`). Launch / bring to front with
  `activate_app(app='元宝')` — `activate_app(app='Yuanbao')` also resolves
  via the alias. ALWAYS activate it first when it isn't the frontmost
  window; do not hunt for a dock icon.
- UI: Electron-style. Element detection from `ui_tree` is unreliable —
  rely on the screenshot + coordinates (vlm_drive).
- Theme: dark.

## Layout

- **Left sidebar:** 搜索 (search), 元宝 (home), 全部收藏 (favorites), then
  grouped/saved items (e.g. 分组示例, 股票分析). These are navigation, NOT
  where you type.
- **Main area:** the conversation. A fresh session shows a greeting
  ("Hi, 今天从哪里开始") with a row of suggestion chips — these are
  shortcuts, not the input box.
- **Input box: bottom-center**, placeholder `和元宝说点什么` (sometimes
  `和元宝聊点什么`). Below it: `内容由AI生成，仅供参考`. This is the only
  place you type.
- **Input toolbar (inside / under the box):** `深度思考` toggle (deep
  reasoning), `工具` dropdown, a `+` attach button, and a round **send
  arrow (↑)** at the far bottom-right.

## How to send a message (the reliable sequence)

1. `activate_app(app='元宝')` if it isn't already frontmost.
2. **Click the bottom-center input box first.** The welcome screen does
   NOT auto-focus the input — typing before a successful click goes
   nowhere. Aim for the placeholder text, near bottom-center of the window.
3. `type(content='...')` the request. Do this ONCE — re-typing appends a
   duplicate.
4. Send with `hotkey(key='enter')` ONCE (or click the round ↑ button).
   Do not press Enter repeatedly; extra Enters send blank messages.
5. `wait()` (repeat as needed) while the answer streams in token by token.
   Document / PPT / 投研报告 generation is slow — wait several turns.
6. `finished(...)` only once the screenshot shows a COMPLETE answer (a
   conclusion, a data table, or a generated/downloadable file). Do not
   finish just because the message was submitted.

## Non-obvious knowledge

- Toggle `深度思考` on before sending for complex/analytical asks (research
  reports, multi-step reasoning); leave it off for quick lookups.
- Asking for a 报告 / PPT / PPTX makes Yuanbao generate a document, which
  can appear as an in-chat preview and/or a downloadable file — both take
  noticeably longer than a plain text reply, so keep waiting.
- If the target isn't visible or the window lost focus mid-task, the
  correct recovery is `activate_app(app='元宝')`, not blind clicking.
