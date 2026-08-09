---
name: web-video-download
description: Universal video download — capture video URL from any site and download with cookies
version: 2.0.0
icon: "🎬"
author: "@rsclaw"
---

# Video Download

3 steps only. Works with Douyin, Bilibili, Kuaishou, Xiaohongshu, YouTube, TikTok, etc.

> **Usage notice / 使用声明**
>
> Intended for content you own or are authorised to access — your own
> uploads, licensed material, or content the rights holder permits you to
> save. You are responsible for complying with each platform's terms of
> service and with applicable copyright law. Do not redistribute
> third-party content without permission.
>
> 本技能仅用于下载您自有或已获授权访问的内容。请遵守各平台服务条款及著作权
> 法律法规；未经许可请勿转载或传播他人作品。

## Step 1: Capture video URLs

```json
{"tool": "web_browser", "action": "capture_video", "url": "<video_page_url>"}
```

This opens the page, waits for video to load, and returns all detected video URLs.

## Step 2: Download

Pick the best URL from the result (prefer `mp4`/`playaddr`, avoid `thumbnail`/`poster`), then:

```json
{"tool": "web_download", "url": "<best_video_url>", "path": "video.mp4", "use_browser_cookies": true}
```

`path` is just a filename — no `~/` or absolute paths.

## Step 3: Send to user

```json
{"tool": "send_file", "path": "<downloaded_file_path>"}
```

## If login required

Use `web_browser` to screenshot the QR code and send to user:
```json
{"tool": "web_browser", "action": "screenshot"}
```
Wait for user to scan, then retry step 1.

## Rules
- Do NOT use exec/curl/wget/yt-dlp
- Always use `use_browser_cookies: true` in web_download
- If capture_video returns empty, the video may need login or is DRM-protected
