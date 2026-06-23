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

## Step 1: 下载并拆解爆款逻辑

### 1a. 拿到本地 mp4
若输入是 URL（用 web-video-download skill 的方式抓真实地址再下载）：
```json
{"tool": "web_browser", "action": "capture_video", "url": "<viral_url>"}
```
```json
{"tool": "web_download", "url": "<best_mp4_url>", "path": "ref.mp4", "use_browser_cookies": true}
```
若输入已是本地文件，跳过下载，记其路径为 `ref.mp4`。

### 1b. 抽分镜帧 + 抽原声 + 测时长
```json
{"tool": "shell", "command": "ffmpeg -i ref.mp4 -vf fps=1/2 -vframes 12 frame_%02d.jpg -y && ffmpeg -i ref.mp4 -vn -ar 16000 -ac 1 ref.wav -y && ffprobe -v error -show_entries format=duration -of default=nk=1:nw=1 ref.mp4"}
```

### 1c. VLM 推理爆款逻辑
把 `frame_01.jpg`..`frame_12.jpg` 作为图片输入，让视觉模型分析，产出结构化结论：
- 选题/赛道
- 钩子（前 3 秒发生了什么）
- 节奏（镜头平均时长、快慢）
- 分镜结构（开场→展开→反转→收尾，各占几镜）
- 文案/口播风格
- BGM 情绪与风格（用于后续 music_gen）
- 估算 BPM（从原声节奏粗估，用于近似卡点；测不准则记 null）

把结论以简洁要点写进上下文（后续步骤引用）。

## Step 2: 写同款创作方案

结合 Step 1 的爆款逻辑 + 本人档案的 `persona`，产出一份创作方案（不复制原文案，做同款重写）：
- **口播全文**（用户母语，贴合 persona 口吻）
- **分镜脚本**：一个有序列表，每镜含
  - `role`: `talk`（本人出镜口播）或 `broll`（画面镜头）
  - `dur`: 时长秒（参考原片节奏）
  - `desc`: 画面描述（broll 用于 image_gen/video_gen 提示词）
  - `line`: 该镜对应的口播句（talk 镜用）
- **BGM 提示词**：基于 BGM 情绪/风格，给 music_gen 的一句话描述

把该方案写进上下文，后续生成逐镜引用。建议总时长贴近原片。
