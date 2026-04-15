---
name: web-video-download
description: Universal video download — capture video URL from any site and download with cookies
version: 2.0.0
icon: "🎬"
author: "@rsclaw"
---

# Video Download

3 steps only. Works with Douyin, Bilibili, Kuaishou, Xiaohongshu, YouTube, TikTok, etc.

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
