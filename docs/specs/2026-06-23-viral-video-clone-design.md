# viral-video-clone — Design Spec

Date: 2026-06-23
Status: Approved (brainstorming) → ready for implementation plan

## Goal

A skill that takes a "爆款" (viral) reference video, infers *why* it went viral,
and produces a same-style video of the **user themself** (real-person 出镜), end
to end. Pure skill orchestration over existing rsclaw tools — **no server / Rust
changes**.

## Non-negotiable constraints

- **No new Rust / gateway code.** Everything is a `SKILL.md` orchestrating tools
  that already ship: `web_download`, keyframe/audio extraction, vision (VLM),
  `image_gen`, `video_gen`, `avatar_gen`, `voice_gen`, `music_gen`, `shell`
  (ffmpeg), `send_file`.
- **jimeng first.** If the jimeng plugin is installed/available, prefer it for
  generation (it supports i2v, voice clone, and digital-human). Otherwise fall
  back to the built-in tool chain below.
- Generation backends are async job-based; the skill submits and waits for
  delivery like other rsclaw video skills.

## Building blocks (verified to exist)

| Need | Tool | Notes |
|---|---|---|
| Download reference video | `web_download` | URL → local mp4; browser-cookie aware |
| Sample frames / audio | ffmpeg via `shell`; `extract_keyframes`/`extract_audio_wav` | `ensure_ffmpeg()` auto-installs |
| Analyze video | vision/VLM (`model.vision`) | feed keyframes; no dedicated "describe video" tool — analyze frames |
| Generate stills | `image_gen` | per-shot first frames |
| Image→video | `video_gen` | i2v / t2v; jimeng / seedance / rsclaw / agnes |
| Real-person talking head | `avatar_gen` (talk lane) | `image`=本人照 + `audio`=口播 → lip-sync |
| Voice clone | `voice_gen` | `reference_audio`(本人样本)+ `reference_text` → 本人音色 |
| BGM | `music_gen` | 描述/情绪 → 配乐音频 |
| Stitch / transitions / mix | ffmpeg via `shell` | concat, `xfade`, `amix` + ducking |
| Deliver | `send_file` | 成片 + 文字版爆款逻辑分析 |

## Component 1 — 本人档案 (person profile)

A reusable, one-time-configured profile so every generation reuses the same
face + voice without re-uploading.

Location: `~/.rsclaw/profiles/<name>/`

| File | Purpose |
|---|---|
| `face.jpg` | 本人正脸照 → `avatar_gen` `image` |
| `voice.wav` | 本人语音样本 → `voice_gen` `reference_audio` (clone) |
| `voice.txt` | 该样本文字稿 → `voice_gen` `reference_text` (optional, improves fidelity) |
| `profile.json5` | `{ name, persona, voice_id?, default_aspect: "9:16" }` |

Skill start: load the profile. If `face.jpg` or `voice.wav` is missing, stop and
tell the user to populate the profile first (with exact paths).

## Component 2 — main flow

```
1. Input: viral video (URL or local file) + profile name
2. web_download → local mp4 (skip if local)
3. ffmpeg: extract keyframes (分镜帧) + extract original audio
4. VLM analyze frames + audio → infer 爆款逻辑:
     选题 / 钩子(前3秒) / 节奏 / 分镜结构 / 文案风格 / BGM 情绪 / 估算 BPM
5. Combine with profile.persona → 创作方案:
     新口播文案 + 分镜脚本(每镜: 时长 + 画面描述) + BGM 风格
6. Generate (priority):
     (a) jimeng plugin available → jimeng (i2v + voice clone + digital human)
     (b) built-in fallback:
         - image_gen: 每镜首帧
         - video_gen (i2v): 每镜 b-roll 片段
         - voice_gen (reference_audio=profile voice): 克隆口播音轨
         - avatar_gen (image=face.jpg, audio=口播): 本人出镜口播段
         - music_gen: 按 step-4 情绪/风格出 BGM
6.5 ffmpeg 合成 (fallback path only; skip if jimeng returns a finished cut):
     - concat: avatar 本人口播段 + 各分镜 i2v 片段, 按脚本顺序
     - 统一规格: 竖屏 9:16 (or profile.default_aspect), 统一帧率/分辨率
     - 近似卡点: 用 step-4 估算的 BPM 算拍长, 镜头切点按拍长均匀对齐
     - 转场: xfade 交叉溶解/淡入淡出 between shots
     - 音轨: 口播为主, BGM 用 amix + 侧链 ducking (口播时压低 BGM)
7. 交付: send_file 成片 + 文字版"爆款逻辑分析"到当前频道
```

## v1 scope

Included:
- ✅ 推理爆款逻辑 + 同款创作方案
- ✅ 本人出镜(avatar_gen)+ 本人音色(voice_gen 克隆)
- ✅ jimeng 优先 / 内置回落
- ✅ music_gen 自动配 BGM
- ✅ ffmpeg 拼接 + 简单转场(xfade)+ BGM ducking
- ✅ 近似卡点:按估算 BPM 均匀对齐切镜

Excluded (v2):
- ❌ 帧级/onset 级精准卡点(需 aubio/librosa 节拍检测)
- ❌ 精剪级转场编排(运镜匹配、卡点特效)
- ❌ A/B 多版本批量

## Dependencies / requirements

- Agent profile must enable the `shell` tool (excluded from the "minimal" tool
  set). The spec/skill doc must call this out.
- ffmpeg auto-installs on first use (`ensure_ffmpeg()`).
- For voice clone + avatar, the 本人档案 must be populated.

## skill vs wasm plugin — 取舍与 v2 演进

Decision: **v1 纯 skill**, ffmpeg 合成用 `shell` 直接跑。

Why skill for v1:
- 核心价值是 **LLM 推理**(爆款逻辑 / 文案 / 分镜)——wasm 内无 LLM,最终仍要回调模型。
- 编排的是**已有 native 工具**(video_gen/avatar_gen/voice_gen/music_gen);wasm 需为每个工具做 host import,纯增负担。
- **迭代速度**:爆款逻辑 prompt 会反复调,skill 改 markdown 即生效,wasm 每次重编译。
- wasm 沙箱默认禁 shell/文件系统,而本功能要跑 ffmpeg、读写本地视频文件,需 `host.cli` allowlist 放行,反成阻力。

Where wasm wins (留给 v2):
- 确定性 + 可分发 + 无 prompt 漂移。若功能产品化对外分发,不希望 LLM 随机性进入"拼接/卡点"这种应 100% 稳定的环节。

v2 evolution:
- 等 ffmpeg 合成脚本稳定且需对外分发时,把 **step 6.5 单独抽成 wasm 工具** `video_assemble(shots[], audio, bgm, bpm) → mp4`,skill 调用它。确定性环节固化,LLM 环节保持灵活。其余流程仍为 skill。

## Open risks

- "有质感、不像 AI" 是模型/素材质量问题,skill 编排无法保证;成片质量取决于
  jimeng / video_gen 后端与本人素材质量。
- BPM 估算是近似;若分析步无法可靠估 BPM,回退到按脚本时长顺序拼接(不卡点)。
