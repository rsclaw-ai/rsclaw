---
name: jimeng
description: 即梦 Jimeng AI 生成图片 生成视频 文生图 文生视频 图生视频 数字人 配音 超清 扩图 重绘 消除 对口型 动作模仿 text-to-image text-to-video image-to-video digital-human TTS super-resolution inpainting outpainting object-removal lip-sync
version: 1.0.0
author: "@rsclaw"
---

# Jimeng AI Automation

IMPORTANT:
- You MUST use the `web_browser` tool to execute these actions. Do NOT use `image_gen` or `video_gen` tools -- those use a different API. Jimeng requires browser automation via `web_browser`.
- Do NOT output JSON text -- instead, make actual tool calls.
- Each step requires a separate web_browser tool call. Execute them sequentially, checking the result before proceeding.

This skill automates all Jimeng AI creative platform features through browser automation.
Requires Jimeng account login (handled via browser session persistence).

Base URL: `https://jimeng.jianying.com`

## Authentication

Jimeng uses cookie-based auth. On first use, login is required:

```json
{"tool": "web_browser", "action": "open", "url": "https://jimeng.jianying.com/ai-tool/generate/?type=image"}
```

If not logged in, screenshot the QR code and send to user:
```json
{"tool": "web_browser", "action": "screenshot"}
```
Wait for user to scan and confirm login, then proceed.

## Available Functions

### 1. Text-to-Image (文生图)

Navigate to image generation page, select model, input prompt, generate and download.

**Models available**: 5.0 Lite, 4.6, 4.5, 4.1, 4.0, 3.1, 3.0
**Resolution**: 2K (default), 4K (members)
**Aspect ratios**: 智能, 1:1, 16:9, 9:16, 4:3, 3:4, 2:3, 3:2, 2.39:1

**Step 1**: Navigate to image generation
```json
{"tool": "web_browser", "action": "open", "url": "https://jimeng.jianying.com/ai-tool/generate/?type=image"}
```

**Step 2**: Select model (open dropdown, click model option)
```json
{"tool": "web_browser", "action": "eval", "code": "var els=document.querySelectorAll('[class*=select]');for(var e of els){if(e.innerText&&e.innerText.match(/图片\\s*\\d/)&&e.offsetHeight>0&&e.offsetHeight<50){e.click();break}}"}
```
Wait 2s, then snapshot to find model options:
```json
{"tool": "web_browser", "action": "snapshot"}
```
Click the desired model option ref (e.g., `@e4` for 5.0 Lite, `@e5` for 4.6, etc.)

**Step 3**: Input prompt
```json
{"tool": "web_browser", "action": "snapshot"}
```
Find the textbox ref, then:
```json
{"tool": "web_browser", "action": "fill", "ref": "<textbox_ref>", "text": "<prompt>"}
```

**Step 4**: Generate
```json
{"tool": "web_browser", "action": "press", "key": "Enter"}
```
Wait 10-30s for generation to complete. Poll with snapshot looking for "生成完成".

**Step 5**: View and download results
Click first generated image (use eval with dispatchEvent for reliable click):
```json
{"tool": "web_browser", "action": "eval", "code": "var imgs=document.querySelectorAll('img');for(var img of imgs){var src=img.src||'';if(src.indexOf('dreamina-sign')>-1&&img.offsetHeight>80){var rect=img.getBoundingClientRect();if(rect.y>0&&rect.y<600){['mousedown','mouseup','click'].forEach(function(t){img.dispatchEvent(new MouseEvent(t,{bubbles:true,cancelable:true,view:window,clientX:rect.x+rect.width/2,clientY:rect.y+rect.height/2,button:0}))});break}}}"}
```
Wait 3s, then find and click download button:
```json
{"tool": "web_browser", "action": "snapshot"}
```
Find download button ref, then:
```json
{"tool": "web_browser", "action": "download", "ref": "<download_ref>", "path": "<filename>"}
```
Use ArrowRight/ArrowLeft to navigate between images in the set.

### 2. Text-to-Video (文生视频)

**Step 1**: Navigate
```json
{"tool": "web_browser", "action": "open", "url": "https://jimeng.jianying.com/ai-tool/generate/?type=video"}
```

**Step 2**: Select "视频生成" mode if not already selected

**Step 3**: Input prompt and generate (same as image: fill textbox, press Enter)

**Step 4**: Wait for video generation (30-120s depending on length and resolution)

**Step 5**: Download result video

### 3. Image-to-Video (图生视频)

Two approaches:

**Approach A**: From generation page
- Navigate to video generation page
- Upload reference image (click upload area, select file)
- Input motion description prompt
- Generate

**Approach B**: From image preview (after generating an image)
- Click "生成视频" button in image preview panel
- This takes the current image and generates a video from it

### 4. Digital Human (数字人)

Navigate to digital human mode:
```json
{"tool": "web_browser", "action": "open", "url": "https://jimeng.jianying.com/ai-tool/generate/?type=avatar"}
```
- Upload a portrait photo
- Upload audio or input text for speech
- Select voice style
- Generate talking-head video

### 5. TTS / Voice Generation (配音生成)

```json
{"tool": "web_browser", "action": "open", "url": "https://jimeng.jianying.com/ai-tool/generate/?type=tts"}
```
- Input text
- Select voice/speaker
- Generate audio
- Download result

### 6. Motion Imitation (动作模仿)

- Upload source image (person)
- Upload reference video (motion to imitate)
- Generate video with source person performing reference motion

### 7. Super Resolution (智能超清 / 超清)

From image preview panel:
- Click "智能超清" or "超清" button
- Wait for processing
- Download enhanced image

### 8. Detail Repair (细节修复)

From image preview panel:
- Click "细节修复" button
- Wait for processing
- Download repaired image

### 9. Inpainting (局部重绘)

From image preview panel:
- Click "局部重绘" button
- Use brush to paint mask area
- Input prompt for what to replace with
- Generate

### 10. Outpainting (扩图)

From image preview panel:
- Click "扩图" button
- Select expansion direction and ratio
- Generate

### 11. Object Removal (消除笔)

From image preview panel:
- Click "消除笔" button
- Paint over object to remove
- Generate

### 12. Lip Sync (对口型)

From image preview panel:
- Click "对口型" button
- Upload audio or input text
- Generate lip-synced video

## Important Notes

- Always use `dispatchEvent(new MouseEvent(...))` for clicking generated images (standard click often fails on Jimeng's custom components)
- After any navigation or generation, re-snapshot to get fresh refs
- Generated images are grouped in sets of 4 by default
- Poll for "生成完成" or "造梦中" text to track generation progress
- Model selector uses `combobox` elements; open dropdown first, then click option
- Download button ref may change between sessions; always snapshot first
- Session cookies persist across browser restarts when using `--session-name` or `--profile`
- For batch operations, close image preview (press Escape) before starting next generation
