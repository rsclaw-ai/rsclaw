---
name: doubao
description: Doubao (ByteDance AI) desktop app - image gen, video gen, chat, writing, coding
---

# Doubao Desktop — Modes & Pitfalls

## App framework

- Bundle: `Doubao.app` (also launches "Doubao Browser" for sub-features)
- UI: Electron. `ui_tree` returns ~3 window buttons — **useless for
  element detection**. Rely entirely on screenshot + coordinates (or
  vlm_drive).
- Theme: dark.

## Mode selection (bottom toolbar)

The bottom input bar has two states: a **mode selector** (default) and
**mode-activated** (after picking a mode, a tag like `图像生成 x`
appears; click the x to exit).

Modes and when to use each:

- **快速** (Quick) — faster, shorter responses.
- **图像生成** (Image) — Seedream models. Has parameters: 参考图
  (reference image), 模型选择, 比例 (1:1 / 2:3 / 3:4 / 4:3 / 9:16 /
  16:9), 风格, 模板. Placeholder: `描述你想要的图片`.
- **视频生成** (Video) — Seedance 2.0 Fast. Supports reference image.
  Slow — wait longer than for images. Placeholder: `添加照片，描述你想生成的视频`.
- **帮我写作** (Writing) — writing-focused prompt.
- **编程** (Coding) — coding-focused prompt.
- **超能模式 Beta** (Super) — enhanced reasoning, complex questions.
- **Normal chat** — make sure NO mode tag is active (click x on any tag
  to deactivate before sending a plain message).

## Non-obvious knowledge

- Left sidebar items with `↗` icon (AI浏览器 / AI创作 / 云盘) launch a
  **separate "Doubao Browser" app** as web pages — they're not in-app
  panels. If a click seems to do nothing, check whether a Doubao Browser
  window opened behind.
- "+" attachment menu has: 选择云盘文件 / 上传代码 / 截图提问 /
  共享屏幕和应用 / 上传文件或图片. Use 上传文件或图片 for plain file
  upload.
- Typing `/` in the input box opens a skill-selection menu.
- Input box position is stable at bottom center of window.
