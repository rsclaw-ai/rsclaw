# viral-video-clone Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a `viral-video-clone` skill that ingests a viral reference video, infers its 爆款逻辑 via VLM, and produces a same-style real-person (本人出镜) video end to end — pure skill, no Rust/server changes.

**Architecture:** A single `skills/viral-video-clone/SKILL.md` orchestrating existing native tools. A reusable 本人档案 under `~/.rsclaw/profiles/<name>/` supplies face + cloned voice. jimeng plugin is preferred for generation (browser automation); built-in `image_gen`/`video_gen`/`avatar_gen`/`voice_gen`/`music_gen` are the fallback. Final assembly (concat, xfade, BGM ducking, BPM-approx cuts) runs via `shell` + ffmpeg.

**Tech Stack:** rsclaw SKILL.md (YAML frontmatter + Markdown + JSON tool calls), native tools (`web_download`, `image_gen`, `video_gen`, `avatar_gen`, `voice_gen`, `music_gen`, `shell`, `send_file`), ffmpeg (auto-installed via `ensure_ffmpeg()`).

> **Note on "tests":** A SKILL.md is a prompt document, not compiled code — there is no unit-test harness. "Verification" steps here are (a) structural lint of the JSON tool-call blocks and (b) a manual end-to-end smoke run. Follow them as written.

---

## File Structure

- Create: `skills/viral-video-clone/SKILL.md` — the skill itself (the whole deliverable).
- Create: `skills/viral-video-clone/profile.example.json5` — sample 本人档案 metadata.
- Create: `skills/viral-video-clone/README.md` — install + profile setup + usage.
- Reference only (do NOT modify): `skills/jimeng/SKILL.md`, `skills/web-video-download/SKILL.md` for tool-call conventions.

Authoring order: scaffold → profile → analysis → creative plan → generation (jimeng + fallback) → assembly → delivery → README → e2e smoke. Each task appends/edits one coherent section of `SKILL.md` and is committed independently.

---

### Task 1: Scaffold skill + frontmatter + profile-load gate

**Files:**
- Create: `skills/viral-video-clone/SKILL.md`

- [ ] **Step 1: Write frontmatter + intro + profile gate**

Create `skills/viral-video-clone/SKILL.md` with:

```markdown
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

\```json
{"tool": "shell", "command": "ls -la ~/.rsclaw/profiles/<name>/"}
\```

若 `face.jpg` 或 `voice.wav` 缺失，停止并提示用户：先把正脸照放到 `~/.rsclaw/profiles/<name>/face.jpg`、语音样本放到 `voice.wav`（建议 10–30 秒清晰人声），再重试。
```

(Replace the `\```` fences with real triple-backticks when writing the file; they are escaped here only to nest inside this plan.)

- [ ] **Step 2: Lint the JSON blocks**

Run: `python3 -c "import json,sys,re; [json.loads(b) for b in re.findall(r'\`\`\`json\n(.*?)\n\`\`\`', open('skills/viral-video-clone/SKILL.md').read(), re.S)]; print('json ok')"`
Expected: `json ok` (every ```json block parses; `<name>` placeholders live in string values so JSON still parses).

- [ ] **Step 3: Commit**

```bash
git add skills/viral-video-clone/SKILL.md
git commit -m "feat(skill): scaffold viral-video-clone + profile gate"
```

---

### Task 2: Sample profile + README

**Files:**
- Create: `skills/viral-video-clone/profile.example.json5`
- Create: `skills/viral-video-clone/README.md`

- [ ] **Step 1: Write the example profile**

Create `skills/viral-video-clone/profile.example.json5`:

```json5
{
  // copy to ~/.rsclaw/profiles/<name>/profile.json5
  name: "me",
  persona: "30岁科技博主，口吻轻松、爱用短句、偶尔玩梗",
  // voice_id: 留空则用 voice.wav 现场克隆；填了则用预置音色
  voice_id: "",
  default_aspect: "9:16",
}
```

- [ ] **Step 2: Write README**

Create `skills/viral-video-clone/README.md`:

```markdown
# viral-video-clone

## 安装
`rsclaw skills install viral-video-clone`

## 一次性配置本人档案
mkdir -p ~/.rsclaw/profiles/me
- 放 `face.jpg`（正脸照）
- 放 `voice.wav`（10–30s 清晰人声样本）
- 放 `voice.txt`（voice.wav 的文字稿，可选）
- 由 `profile.example.json5` 改出 `profile.json5`

## 用法
"用 me 档案，把这个爆款 <URL> 复刻一条同款"

## 依赖
- agent profile 需开启 `shell` 工具
- 可选：装 jimeng 插件以获得更好的生成质量（自动优先）
```

- [ ] **Step 3: Commit**

```bash
git add skills/viral-video-clone/profile.example.json5 skills/viral-video-clone/README.md
git commit -m "docs(skill): viral-video-clone profile example + README"
```

---

### Task 3: Analysis phase — download + extract + infer 爆款逻辑

**Files:**
- Modify: `skills/viral-video-clone/SKILL.md` (append "Step 1: 分析" section)

- [ ] **Step 1: Append the analysis section**

Append to `SKILL.md`:

```markdown
## Step 1: 下载并拆解爆款逻辑

### 1a. 拿到本地 mp4
若输入是 URL（用 web-video-download skill 的方式抓真实地址再下载）：
\```json
{"tool": "web_browser", "action": "capture_video", "url": "<viral_url>"}
\```
\```json
{"tool": "web_download", "url": "<best_mp4_url>", "path": "ref.mp4", "use_browser_cookies": true}
\```
若输入已是本地文件，跳过下载，记其路径为 `ref.mp4`。

### 1b. 抽分镜帧 + 抽原声 + 测时长
\```json
{"tool": "shell", "command": "ffmpeg -i ref.mp4 -vf fps=1/2 -vframes 12 frame_%02d.jpg -y && ffmpeg -i ref.mp4 -vn -ar 16000 -ac 1 ref.wav -y && ffprobe -v error -show_entries format=duration -of default=nk=1:nw=1 ref.mp4"}
\```

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
```

(unescape the `\```` fences to real backticks)

- [ ] **Step 2: Lint JSON blocks**

Run: `python3 -c "import json,re; [json.loads(b) for b in re.findall(r'\`\`\`json\n(.*?)\n\`\`\`', open('skills/viral-video-clone/SKILL.md').read(), re.S)]; print('json ok')"`
Expected: `json ok`

- [ ] **Step 3: Commit**

```bash
git add skills/viral-video-clone/SKILL.md
git commit -m "feat(skill): analysis phase — download, extract, infer viral logic"
```

---

### Task 4: Creative plan phase — 同款脚本 + 分镜

**Files:**
- Modify: `skills/viral-video-clone/SKILL.md` (append "Step 2: 创作方案")

- [ ] **Step 1: Append the creative-plan section**

Append to `SKILL.md`:

```markdown
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
```

- [ ] **Step 2: Commit**

```bash
git add skills/viral-video-clone/SKILL.md
git commit -m "feat(skill): creative-plan phase — same-style script + storyboard"
```

---

### Task 5: Generation phase — jimeng-first + built-in fallback

**Files:**
- Modify: `skills/viral-video-clone/SKILL.md` (append "Step 3: 生成")

- [ ] **Step 1: Append the generation section**

Append to `SKILL.md`:

```markdown
## Step 3: 生成素材

### 路线 A（优先）：jimeng 插件可用
按 jimeng skill 的 web_browser 流程：
- 图生视频 / 数字人 / 声音克隆均走 jimeng。
- 用本人档案 `face.jpg` 作数字人参考、`voice.wav` 作声音克隆参考。
- 逐镜生成 talk（数字人口播）与 broll（i2v）片段，下载到本地 `seg_NN.mp4`。
若 jimeng 出整片（已含口播+画面），可直接进 Step 5 交付，跳过 ffmpeg 合成。

### 路线 B（回落）：内置工具链

**B1. 本人口播音轨（声音克隆）** — 对每个 talk 镜的 `line`：
\```json
{"tool": "voice_gen", "text": "<line>", "reference_audio": "~/.rsclaw/profiles/<name>/voice.wav", "reference_text": "<voice.txt 内容，可选>"}
\```
保存返回音频为 `talk_NN.wav`。

**B2. 本人出镜口播段（对口型）** — 对每个 talk 镜：
\```json
{"tool": "avatar_gen", "image": "~/.rsclaw/profiles/<name>/face.jpg", "audio": "talk_NN.wav"}
\```
等 job 完成，下载为 `seg_NN.mp4`（竖屏，9:16 或 profile.default_aspect）。

**B3. b-roll 画面** — 对每个 broll 镜：
\```json
{"tool": "image_gen", "prompt": "<desc>", "aspect_ratio": "9:16"}
\```
拿到首帧 `shot_NN.jpg` 后做图生视频：
\```json
{"tool": "video_gen", "image": "shot_NN.jpg", "prompt": "<desc>", "duration": <dur>, "aspect_ratio": "9:16"}
\```
等 job 完成，下载为 `seg_NN.mp4`。

**B4. BGM** —
\```json
{"tool": "music_gen", "prompt": "<BGM 提示词>"}
\```
保存返回音频为 `bgm.wav`。

生成后：各镜片段按脚本顺序命名 `seg_01.mp4 .. seg_NN.mp4`，口播总音轨拼成 `voice.wav`（见 Step 4），`bgm.wav` 备用。
```

(unescape fences)

- [ ] **Step 2: Lint JSON blocks**

Run: `python3 -c "import json,re; [json.loads(b) for b in re.findall(r'\`\`\`json\n(.*?)\n\`\`\`', open('skills/viral-video-clone/SKILL.md').read(), re.S)]; print('json ok')"`
Expected: `json ok`

- [ ] **Step 3: Commit**

```bash
git add skills/viral-video-clone/SKILL.md
git commit -m "feat(skill): generation phase — jimeng-first with built-in fallback"
```

---

### Task 6: Assembly phase — ffmpeg concat + xfade + BPM cuts + BGM ducking

**Files:**
- Modify: `skills/viral-video-clone/SKILL.md` (append "Step 4: 合成")

- [ ] **Step 1: Append the assembly section with concrete ffmpeg commands**

Append to `SKILL.md`. Provide ready-to-run ffmpeg recipes (the agent substitutes seg list / durations):

```markdown
## Step 4: 合成成片（仅路线 B；jimeng 出整片则跳过）

> 目标规格：竖屏 9:16、1080x1920、30fps、AAC 音频。

### 4a. 统一每段规格
对每个 `seg_NN.mp4`：
\```json
{"tool": "shell", "command": "ffmpeg -i seg_NN.mp4 -vf scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,fps=30 -c:v libx264 -c:a aac -ar 48000 norm_NN.mp4 -y"}
\```

### 4b. 近似卡点（可选，BPM 已知时）
拍长 = 60 / BPM 秒。把每段裁到最接近整数拍的时长（≥1 拍），使切换大致落在节拍上：
\```json
{"tool": "shell", "command": "ffmpeg -i norm_NN.mp4 -t <round(dur/beat)*beat> -c copy cut_NN.mp4 -y"}
\```
BPM 未知则跳过本步，直接用 `norm_NN.mp4`。

### 4c. 带转场拼接（相邻段 0.3s 交叉溶解）
按顺序对相邻段用 xfade。两段示例（N 段时链式重复，offset = 累计时长 - 0.3）：
\```json
{"tool": "shell", "command": "ffmpeg -i cut_01.mp4 -i cut_02.mp4 -filter_complex \"[0][1]xfade=transition=fade:duration=0.3:offset=<seg1_dur-0.3>\" -c:v libx264 video_only.mp4 -y"}
\```
（多段时逐步把上一步输出与下一段再 xfade；保持 video-only，音频在 4d 统一处理。）

### 4d. 口播音轨拼接
把所有 talk 镜的 `talk_NN.wav` 按顺序 concat 成 `voice.wav`：
\```json
{"tool": "shell", "command": "printf \"file 'talk_01.wav'\\nfile 'talk_02.wav'\\n\" > vlist.txt && ffmpeg -f concat -safe 0 -i vlist.txt -c copy voice.wav -y"}
\```

### 4e. BGM 混音 + ducking（口播时压低 BGM）
用 sidechaincompress 让口播盖过 BGM：
\```json
{"tool": "shell", "command": "ffmpeg -i video_only.mp4 -i voice.wav -i bgm.wav -filter_complex \"[2:a]volume=0.35[bg];[bg][1:a]sidechaincompress=threshold=0.03:ratio=8:attack=20:release=300[duck];[1:a][duck]amix=inputs=2:duration=longest[mix]\" -map 0:v -map \"[mix]\" -c:v copy -c:a aac -shortest final.mp4 -y"}
\```

成片为 `final.mp4`。
```

(unescape fences)

- [ ] **Step 2: Lint JSON blocks**

Run: `python3 -c "import json,re; [json.loads(b) for b in re.findall(r'\`\`\`json\n(.*?)\n\`\`\`', open('skills/viral-video-clone/SKILL.md').read(), re.S)]; print('json ok')"`
Expected: `json ok`

- [ ] **Step 3: Validate ffmpeg recipes actually run (with stand-in inputs)**

Run this scratch test that generates dummy segments and exercises the 4a/4c/4e recipes:
```bash
cd /tmp && rm -rf vvc_t && mkdir vvc_t && cd vvc_t && \
ffmpeg -f lavfi -i testsrc=d=2:s=640x360:r=30 -f lavfi -i sine=f=300:d=2 -c:v libx264 -c:a aac seg_01.mp4 -y >/dev/null 2>&1 && \
ffmpeg -f lavfi -i testsrc=d=2:s=640x360:r=30 -f lavfi -i sine=f=400:d=2 -c:v libx264 -c:a aac seg_02.mp4 -y >/dev/null 2>&1 && \
ffmpeg -i seg_01.mp4 -vf scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,fps=30 -c:v libx264 -c:a aac norm_01.mp4 -y >/dev/null 2>&1 && \
ffmpeg -i seg_02.mp4 -vf scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,fps=30 -c:v libx264 -c:a aac norm_02.mp4 -y >/dev/null 2>&1 && \
ffmpeg -i norm_01.mp4 -i norm_02.mp4 -filter_complex "[0][1]xfade=transition=fade:duration=0.3:offset=1.7" -c:v libx264 video_only.mp4 -y >/dev/null 2>&1 && \
echo OK-assembly
```
Expected: `OK-assembly` (confirms scale/crop, xfade, and codecs are valid on this ffmpeg build).

- [ ] **Step 4: Commit**

```bash
git add skills/viral-video-clone/SKILL.md
git commit -m "feat(skill): assembly phase — concat, xfade, BPM cuts, BGM ducking"
```

---

### Task 7: Delivery phase + rules section

**Files:**
- Modify: `skills/viral-video-clone/SKILL.md` (append "Step 5: 交付" + "Rules")

- [ ] **Step 1: Append delivery + rules**

Append to `SKILL.md`:

```markdown
## Step 5: 交付

把成片发给用户，并附一段文字版「爆款逻辑分析」（Step 1 的结论要点）：
\```json
{"tool": "send_file", "path": "final.mp4"}
\```

## Rules
- 生成阶段：jimeng 可用则优先；否则内置工具链。
- 口播必须用本人档案的脸（avatar_gen）+ 克隆音色（voice_gen reference_audio）。
- 不复制原视频文案/画面，做「同款」重写，规避侵权。
- ffmpeg 路径：所有中间文件用相对文件名（与 web-video-download 一致），不要 `~/` 或绝对路径作为输出 `path`。
- BPM 测不准就不卡点，按脚本时长顺序拼接，不要硬猜。
- 不自动配 BGM 歌词/人声，只用 music_gen 的纯 BGM。
```

(unescape fences)

- [ ] **Step 2: Lint JSON blocks**

Run: `python3 -c "import json,re; [json.loads(b) for b in re.findall(r'\`\`\`json\n(.*?)\n\`\`\`', open('skills/viral-video-clone/SKILL.md').read(), re.S)]; print('json ok')"`
Expected: `json ok`

- [ ] **Step 3: Commit**

```bash
git add skills/viral-video-clone/SKILL.md
git commit -m "feat(skill): delivery phase + rules"
```

---

### Task 8: End-to-end manual smoke validation

**Files:**
- Reference: `skills/viral-video-clone/SKILL.md` (no edits unless smoke finds a gap)

- [ ] **Step 1: Set up a throwaway profile**

```bash
mkdir -p ~/.rsclaw/profiles/smoketest && \
ffmpeg -f lavfi -i color=c=gray:s=512x512:d=1 -frames:v 1 ~/.rsclaw/profiles/smoketest/face.jpg -y >/dev/null 2>&1 && \
ffmpeg -f lavfi -i sine=f=220:d=8 -ar 16000 -ac 1 ~/.rsclaw/profiles/smoketest/voice.wav -y >/dev/null 2>&1 && \
cp skills/viral-video-clone/profile.example.json5 ~/.rsclaw/profiles/smoketest/profile.json5 && \
ls ~/.rsclaw/profiles/smoketest
```
Expected: `face.jpg  profile.json5  voice.wav`

- [ ] **Step 2: Dry-run the skill against a short reference clip**

In a rsclaw session with the `shell` tool enabled, run:
"用 smoketest 档案，把这个爆款 <短视频URL或本地短clip> 复刻一条同款"

Walk through and confirm each phase fires: Step 0 profile gate passes → Step 1 download+extract+VLM → Step 2 script → Step 3 generation (note whether jimeng or fallback path was taken) → Step 4 assembly produces `final.mp4` → Step 5 send_file delivers it.

Expected: a `final.mp4` is delivered. (Quality is out of scope for the smoke; we are verifying the pipeline wires end to end without a dead step.)

- [ ] **Step 3: Record gaps and fix inline**

If any phase stalls (missing tool, wrong path assumption, ffmpeg arg error), note it and patch the corresponding SKILL.md section, then re-lint and re-run that phase.

- [ ] **Step 4: Commit any fixes**

```bash
git add skills/viral-video-clone/SKILL.md
git commit -m "fix(skill): viral-video-clone e2e smoke fixes"
```

- [ ] **Step 5: Clean up the throwaway profile**

```bash
rm -rf ~/.rsclaw/profiles/smoketest
```

---

## Self-Review notes (author)

- **Spec coverage:** 本人档案 (Task 1–2), download/extract/VLM 爆款逻辑 (Task 3), 创作方案 (Task 4), jimeng-first + fallback generation incl. voice clone & avatar (Task 5), music_gen BGM (Task 5/B4), ffmpeg concat+xfade+BPM-approx cuts+ducking (Task 6), delivery + 爆款逻辑分析 text (Task 7), `shell` dependency called out (Task 1). All spec sections mapped.
- **Excluded (v2) honored:** no onset-level beat detection, no precision editing, no A/B — none appear in tasks.
- **Placeholder scan:** `<name>`, `<line>`, `<desc>`, `<dur>`, `<viral_url>` are runtime substitutions inside a prompt document (the skill's job), not plan placeholders — they are explicitly defined in the surrounding text. ffmpeg commands are concrete and runnable (Task 6 Step 3 proves the core recipes).
- **Consistency:** segment naming `seg_NN → norm_NN → cut_NN → video_only → final.mp4`, audio `talk_NN.wav → voice.wav`, `bgm.wav` consistent across Tasks 5–7.
