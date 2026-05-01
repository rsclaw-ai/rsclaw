---
name: doubao
description: Doubao (ByteDance AI) desktop app - image gen, video gen, chat, writing, coding
---

# Doubao Desktop App

## App Info
- Bundle: Doubao.app (also launches "Doubao Browser" for sub-features)
- UI framework: Electron (ui_tree returns almost nothing, rely on screenshot + coordinates)
- Theme: dark
- Window default: 1200x800

## Bottom Input Bar (core interaction area)

The bottom bar has two states:

### Default state - mode selector toolbar
```
[+] | [快速] | [视频生成] | [图像生成] | [帮我写作] | [编程] | [超能模式 Beta] | [更多]
```
Input placeholder: "发消息或输入'/'选择技能"

### Mode-activated state
When a mode is selected, toolbar changes to mode-specific options:

**Image generation mode (图像生成):**
```
[图像生成 x] | [参考图] | [Seedream 4.5 v] | [比例 v] | [风格 v] | [模板]
```
- Input placeholder: "描述你想要的图片"
- Models: Seedream 5.0 Lite, Seedream 4.5, Seedream 4.0
- Ratios: 1:1 (square/avatar), 2:3 (social/selfie), 3:4 (classic/photo), 4:3 (article/illustration), 9:16 (phone wallpaper/portrait), 16:9 (desktop wallpaper/landscape)
- Styles: dropdown with various artistic styles
- Template: opens template gallery
- Click x on "图像生成" tag to exit mode

**Video generation mode (视频生成):**
```
[视频生成 x] | [参考图] | [Seedance 2.0 Fast v] | [比例 v]
```
- Input placeholder: "添加照片，描述你想生成的视频"
- Models: Seedance 2.0 Fast (全能视频模型)
- Supports reference image upload
- Click x on "视频生成" tag to exit mode

### "+" menu (attachment/upload)
- 选择云盘文件 (Select cloud drive file)
- 上传代码 > (Upload code)
- 截图提问 (Screenshot question)
- 共享屏幕和应用 > (Share screen and app)
- 上传文件或图片 (Upload file or image)

## Operations

### Generate an image
1. Click "图像生成" in bottom toolbar
2. (Optional) Click "Seedream 4.5" to change model
3. (Optional) Click "比例" to set aspect ratio
4. (Optional) Click "风格" to set style
5. (Optional) Click "参考图" to upload reference image
6. Click input box, type image description in Chinese
7. Press Enter or click send button (blue arrow, right side)
8. Wait for image to appear in chat

### Generate a video
1. Click "视频生成" in bottom toolbar
2. (Optional) Click "参考图" to upload a reference image/photo
3. (Optional) Click "Seedance 2.0 Fast" to change model
4. (Optional) Click "比例" to set aspect ratio
5. Click input box, type video description in Chinese
6. Press Enter to send
7. Wait for video generation (takes longer than image)

### Normal chat / reasoning
1. Make sure no mode tag is active (no "图像生成 x" or "视频生成 x" in toolbar)
2. If a mode is active, click x on the mode tag to deactivate
3. Click input box, type message
4. Press Enter to send
5. AI response streams in chat area

### Writing mode (帮我写作)
1. Click "帮我写作" in bottom toolbar
2. Input box changes to writing-specific prompt
3. Type writing request, press Enter

### Coding mode (编程)
1. Click "编程" in bottom toolbar
2. Input box changes to coding-specific prompt
3. Type coding question, press Enter

### Super mode (超能模式 Beta)
1. Click "超能模式" in bottom toolbar
2. Enhanced reasoning mode activates
3. Type complex question, press Enter

### Quick mode (快速)
1. Click "快速" in bottom toolbar
2. Faster, shorter responses
3. Type message, press Enter

### Start new conversation
1. Click "新对话" in left sidebar top, OR
2. Click "重生" button at bottom of left sidebar

### Upload file/image to chat
1. Click "+" button in bottom toolbar
2. Select "上传文件或图片"
3. Pick file from file picker

### Use "/" skills
1. Type "/" in input box
2. Skill selection menu appears
3. Select a skill or continue typing skill name

## Left Sidebar
- Top: "豆包" (home), "AI浏览器" (opens Doubao Browser), "AI创作" (opens in Doubao Browser), "云盘" (opens in Doubao Browser), "更多" (expand)
- Note: Items with arrow icon (↗) open in separate Doubao Browser window
- Middle: "历史对话" (chat history), scrollable list
- Bottom: "重生" (new chat), gift icon

## Tips
- Electron app: ui_tree returns only 3 window buttons, useless for element detection
- Must rely entirely on screenshot + coordinate clicking
- "AI浏览器", "AI创作", "云盘" all launch Doubao Browser (separate app) as web pages
- Mode switching: click mode button to activate, click x on tag to deactivate
- Input box position is stable at bottom center of window
- When generating images/videos, results appear as chat messages
- Voice input available via microphone icon (rightmost in toolbar)
