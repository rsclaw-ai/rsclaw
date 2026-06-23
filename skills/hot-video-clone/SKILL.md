---
name: hot-video-clone
description: 爆款视频复刻 — 推一个爆款视频，自动推理爆款逻辑并生成同款、本人真人出镜的视频。hot video clone, same-style, real-person, 数字人, 口播, BGM
version: 1.0.0
icon: "🎬"
author: "@rsclaw"
---

# 爆款视频复刻 (hot-video-clone)

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

## Step 3: 生成素材

### 路线 A（优先）：jimeng 插件可用
按 jimeng skill 的 web_browser 流程：
- 图生视频 / 数字人 / 声音克隆均走 jimeng。
- 用本人档案 `face.jpg` 作数字人参考、`voice.wav` 作声音克隆参考。
- 逐镜生成 talk（数字人口播）与 broll（i2v）片段，下载到本地 `seg_NN.mp4`。
若 jimeng 出整片（已含口播+画面），可直接进 Step 5 交付，跳过 ffmpeg 合成。

### 路线 B（回落）：内置工具链

**B1. 本人口播音轨（声音克隆）** — 对每个 talk 镜的 `line`：
```json
{"tool": "voice_gen", "text": "<line>", "reference_audio": "~/.rsclaw/profiles/<name>/voice.wav", "reference_text": "<voice.txt 内容，可选>"}
```
保存返回音频为 `talk_NN.wav`。

**B2. 本人出镜口播段（对口型）** — 对每个 talk 镜：
```json
{"tool": "avatar_gen", "image": "~/.rsclaw/profiles/<name>/face.jpg", "audio": "talk_NN.wav"}
```
等 job 完成，下载为 `seg_NN.mp4`（竖屏，9:16 或 profile.default_aspect）。

**B3. b-roll 画面** — 对每个 broll 镜：
```json
{"tool": "image_gen", "prompt": "<desc>", "aspect_ratio": "9:16"}
```
拿到首帧 `shot_NN.jpg` 后做图生视频：
```json
{"tool": "video_gen", "image": "shot_NN.jpg", "prompt": "<desc>", "duration": 5, "aspect_ratio": "9:16"}
```
等 job 完成，下载为 `seg_NN.mp4`。

**B4. BGM** —
```json
{"tool": "music_gen", "prompt": "<BGM 提示词>"}
```
保存返回音频为 `bgm.wav`。

生成后：各镜片段按脚本顺序命名 `seg_01.mp4 .. seg_NN.mp4`，口播总音轨拼成 `voice.wav`（见 Step 4），`bgm.wav` 备用。

## Step 4: 合成成片（仅路线 B；jimeng 出整片则跳过）

> 目标规格：竖屏 9:16、1080x1920、30fps、AAC 音频。

### 4a. 统一每段规格
对每个 `seg_NN.mp4`：
```json
{"tool": "shell", "command": "ffmpeg -i seg_NN.mp4 -vf scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,fps=30 -c:v libx264 -c:a aac -ar 48000 norm_NN.mp4 -y"}
```

### 4b. 近似卡点（可选，BPM 已知时）
拍长 = 60 / BPM 秒。把每段裁到最接近整数拍的时长（≥1 拍），使切换大致落在节拍上：
```json
{"tool": "shell", "command": "ffmpeg -i norm_NN.mp4 -t 1.6 -c copy cut_NN.mp4 -y"}
```
（`-t` 取 `round(dur/beat)*beat`；BPM 未知则跳过本步，直接用 `norm_NN.mp4`。）

### 4c. 带转场拼接（相邻段 0.3s 交叉溶解）
按顺序对相邻段用 xfade（多段时链式重复，offset = 上一段累计时长 - 0.3）：
```json
{"tool": "shell", "command": "ffmpeg -i cut_01.mp4 -i cut_02.mp4 -filter_complex \"[0][1]xfade=transition=fade:duration=0.3:offset=1.3\" -c:v libx264 video_only.mp4 -y"}
```
（多段时逐步把上一步输出与下一段再 xfade；保持 video-only，音频在 4d 统一处理。）

### 4d. 口播音轨拼接
把所有 talk 镜的 `talk_NN.wav` 按顺序 concat 成 `voice.wav`：
```json
{"tool": "shell", "command": "printf \"file 'talk_01.wav'\\nfile 'talk_02.wav'\\n\" > vlist.txt && ffmpeg -f concat -safe 0 -i vlist.txt -c copy voice.wav -y"}
```

### 4e. BGM 混音 + ducking（口播时压低 BGM）
用 sidechaincompress 让口播盖过 BGM：
```json
{"tool": "shell", "command": "ffmpeg -i video_only.mp4 -i voice.wav -i bgm.wav -filter_complex \"[2:a]volume=0.35[bg];[bg][1:a]sidechaincompress=threshold=0.03:ratio=8:attack=20:release=300[duck];[1:a][duck]amix=inputs=2:duration=longest[mix]\" -map 0:v -map \"[mix]\" -c:v copy -c:a aac -shortest final.mp4 -y"}
```

成片为 `final.mp4`。
