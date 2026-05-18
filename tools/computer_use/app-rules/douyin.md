---
name: douyin
description: Douyin (TikTok China) desktop app - browse, search, publish, live, messaging
---

# Douyin Desktop — Views & Pitfalls

## App framework

- Bundle: `抖音.app`
- UI: Electron. `ui_tree` is **completely useless** — only 3 window
  buttons exposed. Use screenshot + coordinates or vlm_drive.
- Theme: dark.

## Two main view modes (very different)

The most important distinction is **精选 vs 推荐**:

- **精选** (Featured) — grid of video thumbnails (2-3 columns). Standard
  scroll. Click thumbnail to play. Category tabs (全部 / 知识 / 影视 /
  音乐 / 美食 / 旅行 / 三农 ...) only visible here.
- **推荐** (Recommended) — full-screen single video player. Scroll or
  `Down` arrow advances to next video (one at a time). Right-side
  interaction icons (❤ / 💬 / ⭐ / ↗) only visible in this view.
- Press `Space` to pause/resume. Press `F` or double-click for
  fullscreen. `Esc` exits fullscreen.

## Non-obvious knowledge

- **投稿** (upload/post) in top-right bar may redirect to a web browser
  for the full upload flow — not an in-app panel.
- **AI 搜索** in left sidebar is a separate AI-powered search page,
  different from the top center search box.
- **弹幕** (danmaku) = floating text comments overlaid on video — a
  unique Chinese-platform feature. Toggle via bottom video controls.
- **直播 / 放映厅 / 短剧** are content-type filters in the left sidebar
  (live streams / theatre / short dramas), not just video categories.
- Notification badges may appear on 关注 / 我的 / 通知 / 私信 — check
  before clicking to know what's new.
- Video info (author, date, hashtags) sits at the bottom of the player
  in 推荐 view, not in a separate panel.

## Common pitfalls

- Don't expect the same controls in 精选 and 推荐 — right-side icons,
  bottom player bar, and scroll behaviour all differ.
- "听抖音" (Listen mode) in 推荐 view plays audio only — don't click it
  if you want video to keep playing.
