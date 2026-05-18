---
name: wechat
description: WeChat (微信) desktop client — group chat monitoring, search, quote-reply, cross-channel automation.
triggers:
  - wechat
  - 微信
  - weixin
---

# WeChat Desktop — Policy & Pitfalls

## Reply policy (group chat monitoring)

Only reply when the message is clearly directed at the bot:

1. Bubble text contains `@<bot-name>` (rendered as blue inline mention).
2. Bubble text starts with the bot's nickname (e.g., `螃蟹，...` / `助手，...`).
3. Bubble is a direct question that mentions the bot in context.

If none hold, **do not reply** — just summarise messages back to the user.
A bare weather question without `@bot` should be ignored.

## Pre-conditions

- WeChat must be running and **logged in**. If a QR-code login screen is
  showing, stop, take a screenshot, and `call_user` reporting login is
  needed. Never attempt to dismiss the QR screen.
- WeChat should be frontmost. The driver brings the app forward; you
  don't need to click the dock.

## Quote-reply (引用) — keyboard-only

The right-click context menu is brittle to pixel coordinates and prone
to loop-clicking. **Always drive Quote with the keyboard**: right-click
the target bubble → wait for menu → press `down` 9 times (Quote is item
10) → press `return`. The quoted block then appears above the input.

## Common pitfalls

- The search box is at the **top of the left sidebar**, not the right
  pane. Don't confuse it with the chat input box.
- If a search miss happens, scroll the conversation list rather than
  retyping the name.
- WeChat's in-app strings stay Chinese (`搜索` / `引用` / `转发` /
  `撤回`) even when the OS is in English — don't assume English labels.
- Color semantics: green bubble = sent by you, white bubble = received.
- After replying, verify the new green bubble appears below the quoted
  block. If not, treat the send as failed and surface to user.
