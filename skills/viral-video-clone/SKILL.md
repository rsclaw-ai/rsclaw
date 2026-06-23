---
name: viral-video-clone
description: 爆款视频复刻 — 推一个爆款视频，自动推理爆款逻辑并生成同款、本人真人出镜的视频。viral video clone, same-style, real-person, 数字人, 口播, BGM
version: 1.0.0
icon: "🎬"
author: "@rsclaw"
---

# 爆款视频复刻 (viral-video-clone)

给我一个爆款视频（URL 或本地文件）+ 一个本人档案名，我会：
1. 下载并拆解它的爆款逻辑（钩子/节奏/分镜/文案/BGM 情绪）
2. 结合你的人设写出同款脚本
3. 生成同款视频，**用你本人的脸和声音出镜口播**
4. 配 BGM、拼接、加转场、按节拍近似卡点，交付成片

> 需要 agent profile 开启 `shell` 工具（用于 ffmpeg）。ffmpeg 首次使用会自动安装。

## 优先级规则
- 若 **jimeng 插件已安装**（存在 `skills/jimeng` 或可调用其 web_browser 流程），生成阶段优先走 jimeng（它支持 i2v + 声音克隆 + 数字人）。
- 否则走内置工具链：`image_gen` → `video_gen` → `avatar_gen` → `voice_gen` → `music_gen`。

## Step 0: 加载本人档案

档案目录：`~/.rsclaw/profiles/<name>/`，包含：
- `face.jpg` — 本人正脸照
- `voice.wav` — 本人语音样本（用于声音克隆）
- `voice.txt` — 该样本的文字稿（可选，提升克隆保真）
- `profile.json5` — `{ name, persona, voice_id?, default_aspect: "9:16" }`

读取并校验 `face.jpg`、`voice.wav` 是否存在：

```json
{"tool": "shell", "command": "ls -la ~/.rsclaw/profiles/<name>/"}
```

若 `face.jpg` 或 `voice.wav` 缺失，停止并提示用户：先把正脸照放到 `~/.rsclaw/profiles/<name>/face.jpg`、语音样本放到 `voice.wav`（建议 10–30 秒清晰人声），再重试。
