---
name: wechat
description: WeChat (微信) desktop client — group chat monitoring, search, quote-reply, cross-channel automation.
triggers:
  - wechat
  - 微信
  - weixin
---

# WeChat Desktop UI Guide

## Window layout

```
┌──────────────────┬──────────────────────────────────────┐
│   Left sidebar   │            Right pane                │
│  ┌────────────┐  │  ┌─────────────────────────────┐    │
│  │ 搜索 box   │  │  │  Header: chat name + tools  │    │
│  ├────────────┤  │  ├─────────────────────────────┤    │
│  │ Chat list  │  │  │                             │    │
│  │ (most-     │  │  │   Message scroll area       │    │
│  │  recent on │  │  │   (oldest top, latest       │    │
│  │  top)      │  │  │    bottom)                  │    │
│  └────────────┘  │  ├─────────────────────────────┤    │
│   Tabs:          │  │  Toolbar: emoji file image  │    │
│   消息 / 联系人  │  ├─────────────────────────────┤    │
│   收藏 / ...     │  │  Input box (multi-line)     │    │
│                  │  │  Enter sends · Shift+Enter  │    │
│                  │  │  newline                    │    │
│                  │  └─────────────────────────────┘    │
└──────────────────┴──────────────────────────────────────┘
```

## Recurring tasks

### Open a specific group / contact by name
1. Click the search box at the top of the left sidebar (placeholder `搜索`).
2. Wait briefly for input focus.
3. Type the target name (Chinese is fine — use `type(content='...')`).
4. Press `return` to enter the first match (or click the first dropdown row).

### Read recent messages
- Messages render bottom-up in the right pane. The newest are immediately
  visible without scrolling.
- Each bubble shows: sender display name (above the bubble for groups),
  avatar (left or right), bubble background (green = sent by you, white
  = received), timestamp on hover.
- `@<name>` mentions render as blue inline text inside the bubble.
- Time gaps > a few minutes get a centred grey timestamp separator.

### Reply with Quote (引用) — keyboard-driven
The right-click context menu is hard to hit by exact pixel and prone to
loop-clicking. Drive it with the keyboard instead:

1. `right_single` on the target message bubble.
2. `wait(seconds=0.5)` so the context menu has time to render.
3. Press `down` arrow 9 times — Quote (`引用`) is the 10th item.
4. Press `return`. The quoted message now appears as a grey block above
   the input box.
5. `type(content='your reply\n')` — the trailing `\n` submits.

### Verify a sent message
After replying, scroll to the bottom of the chat. The reply must show as
a green bubble immediately below the quoted block; if it doesn't, treat
the send as failed and surface that to the user.

### Reply policy (when monitoring a group)
Only reply when the message is clearly directed at the bot:

1. Bubble text contains `@<bot-name>` (blue mention).
2. Bubble text starts with the bot's nickname (e.g., `螃蟹，...` /
   `助手，...`).
3. Bubble is a direct question that mentions the bot in context.

If none of those hold, **do not reply** — just summarise the messages
back to the user. A bare weather question without `@bot` should be
ignored.

## Pre-conditions
- The `WeChat` app must be running and **logged in**. If a QR-code login
  screen is showing, stop, take a final screenshot, and `call_user`
  reporting that login is needed.
- `WeChat` should be frontmost. The driver bringing the app forward is
  the operator's job; you don't need to click the dock.

## Common pitfalls
- The search box is at the **top** of the left sidebar, not the right
  pane. Don't confuse it with the chat input box at the bottom-right.
- The conversation list scrolls; if a search miss happens, scroll the
  list rather than retyping the name.
- Don't assume English UI. Even when the OS is in English, WeChat's
  in-app strings stay Chinese (`搜索` / `引用` / `转发` / `撤回`).
