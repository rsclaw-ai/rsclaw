# Jimeng WASM Plugin Design

**Date**: 2026-04-18
**Status**: Draft
**Author**: rsclaw team

## Overview

A WASM-based plugin for rsclaw that automates all Jimeng (即梦) AI creative platform features through browser automation. The plugin runs in a sandboxed wasmtime runtime, communicating with rsclaw's browser engine via host functions.

## Architecture

```
User message: "生成一张小猫图片"
    |
    v
rsclaw agent -> LLM selects tool: jimeng.txt2img(prompt="...")
    |
    v
rsclaw WASM plugin runtime (wasmtime)
    | loads ~/.rsclaw/plugins/jimeng.wasm
    | calls export: handle_tool("txt2img", args_json)
    |
    v
jimeng.wasm internal logic (Rust compiled to wasm32-wasip2):
    | host::browser_open("https://jimeng.jianying.com/...")
    | host::browser_snapshot() -> elements
    | host::browser_fill(ref, prompt)
    | host::browser_press("Enter")
    | host::browser_wait_text("生成完成", 60000)
    | host::browser_download(ref, path)
    | host::sleep(3000)  // rate limiting
    |
    v
Result -> agent -> user (image/video file)
```

## WASM Interface (WIT)

### Host Functions (imports) — rsclaw provides to plugin

```wit
package rsclaw:plugin;

interface browser {
    /// Navigate to URL. Returns page title.
    open: func(url: string) -> result<string, string>;

    /// Get accessibility snapshot of interactive elements.
    /// Returns JSON array of {ref, role, text, ...}.
    snapshot: func() -> result<string, string>;

    /// Click element by ref (e.g. "@e5").
    click: func(ref: string) -> result<string, string>;

    /// Click at pixel coordinates (for custom components).
    click-at: func(x: u32, y: u32) -> result<string, string>;

    /// Clear and type text into element.
    fill: func(ref: string, text: string) -> result<string, string>;

    /// Press a key (Enter, Escape, ArrowRight, etc).
    press: func(key: string) -> result<string, string>;

    /// Scroll page. direction: "up"|"down"|"left"|"right".
    scroll: func(direction: string, amount: u32) -> result<string, string>;

    /// Execute JavaScript in page context. Returns eval result.
    eval: func(code: string) -> result<string, string>;

    /// Wait for text to appear on page. timeout in ms.
    wait-text: func(text: string, timeout-ms: u32) -> result<string, string>;

    /// Wait milliseconds.
    wait-ms: func(ms: u32);

    /// Take screenshot. Returns base64-encoded PNG.
    screenshot: func() -> result<string, string>;

    /// Download file by clicking element ref. Returns saved file path.
    download: func(ref: string, filename: string) -> result<string, string>;

    /// Upload file to element ref.
    upload: func(ref: string, filepath: string) -> result<string, string>;

    /// Get current page URL.
    get-url: func() -> result<string, string>;
}

interface runtime {
    /// Log a message. level: "info"|"warn"|"error"|"debug".
    log: func(level: string, msg: string);

    /// Sleep with rate-limiting enforcement. Host enforces minimum interval.
    sleep: func(ms: u32);

    /// Read a file (for uploading user images). Returns base64 content.
    read-file: func(path: string) -> result<string, string>;

    /// Get plugin config value.
    get-config: func(key: string) -> option<string>;
}
```

### Plugin Exports — plugin provides to rsclaw

```wit
interface plugin {
    /// Return plugin manifest as JSON.
    /// Contains: name, version, description, tools[{name, description, parameters}].
    get-manifest: func() -> string;

    /// Handle a tool call. Returns result JSON.
    handle-tool: func(tool-name: string, args-json: string) -> result<string, string>;
}
```

## Tool Definitions (40 functions, 7 modules)

### Module 1: Image Generation (图片生成)

| Tool | Description | Parameters |
|------|-------------|------------|
| `jimeng.txt2img` | Text to image | prompt, model?(5.0Lite/4.6/4.5/4.1/4.0/3.1/3.0), ratio?(1:1/16:9/9:16/4:3), resolution?(2K/4K), count?(1-4) |
| `jimeng.img2img` | Image to image | image_path, prompt, similarity?(0-10) |
| `jimeng.mix_images` | Multi-image fusion | image_paths[], prompt |
| `jimeng.batch_images` | Batch series generation | prompt, count, style? |
| `jimeng.same_style` | Clone community style | reference_url, prompt |
| `jimeng.face_swap` | Face swap | source_image, target_image |

### Module 2: Video Generation (视频生成)

| Tool | Description | Parameters |
|------|-------------|------------|
| `jimeng.txt2vid` | Text to video | prompt, duration?(4-15s), resolution?(720P/1080P), camera?(push/pull/pan/rotate) |
| `jimeng.img2vid` | Image to video | image_path, prompt, duration? |
| `jimeng.frame2vid` | First+last frame to video | first_frame, last_frame, prompt? |
| `jimeng.vid_extend` | Extend/continue video | video_path, prompt? |
| `jimeng.vid_upscale` | Video upscale | video_path |
| `jimeng.lip_sync` | Lip sync to audio | video_or_image_path, audio_path_or_text |

### Module 3: Canvas/Editing (智能画布)

| Tool | Description | Parameters |
|------|-------------|------------|
| `jimeng.canvas_expand` | Smart outpaint | image_path, direction?(up/down/left/right/all), ratio? |
| `jimeng.canvas_inpaint` | Inpaint region | image_path, mask_description, prompt |
| `jimeng.canvas_erase` | Remove object | image_path, area_description |
| `jimeng.canvas_cutout` | Smart cutout | image_path |
| `jimeng.canvas_merge` | Merge images | image_paths[], layout? |

### Module 4: Digital Human (数字人)

| Tool | Description | Parameters |
|------|-------------|------------|
| `jimeng.digital_human_create` | Create digital human | description_or_image |
| `jimeng.digital_human_talk` | Talking head video | avatar_image, text_or_audio, voice? |
| `jimeng.digital_human_clone` | Clone from photo | photo_path |

### Module 5: Story Creation (故事创作)

| Tool | Description | Parameters |
|------|-------------|------------|
| `jimeng.story_script` | Write storyboard | story_description, scenes_count? |
| `jimeng.story_generate` | Generate story video | script_or_description |
| `jimeng.story_edit` | Edit storyboard | story_id, edits |

### Module 6: Advanced Control (高级控制)

| Tool | Description | Parameters |
|------|-------------|------------|
| `jimeng.character_lock` | Lock character consistency | character_image, prompt |
| `jimeng.pose_copy` | Copy pose from reference | pose_reference, character_image |
| `jimeng.camera_control` | Camera movement control | movement_type, speed?, angle? |
| `jimeng.style_apply` | Apply style preset | style_name, image_or_prompt |

### Module 7: Utilities (工具导出)

| Tool | Description | Parameters |
|------|-------------|------------|
| `jimeng.photo_restore` | Restore old photo | image_path, colorize?(bool) |
| `jimeng.photo_animate` | Animate static photo | image_path |
| `jimeng.export_image` | Export without watermark | image_id |
| `jimeng.export_video` | Export without watermark | video_id |
| `jimeng.history_list` | List generation history | type?(image/video), limit? |
| `jimeng.history_retry` | Retry from history | history_id |

## Rate Limiting & Safety

### Plugin-side (compiled into WASM)
- Minimum 3 second interval between browser actions
- No concurrent operations (sequential only)
- No queue/batch flooding

### Host-side (rsclaw enforced)
- Global per-plugin rate limit: max 20 tool calls per minute
- Browser session isolation: each plugin gets its own browser profile
- File access: only user-specified paths (no arbitrary filesystem access)
- No cookie/credential exposure to plugin

### Disclaimer (embedded in manifest)
```
This plugin is for personal use only.
Users must use their own Jimeng account.
Commercial use, bulk generation, and abuse are prohibited.
All actions are the user's responsibility.
```

## Implementation Plan

### Phase 1: Core Infrastructure
1. Add `wasmtime` dependency to rsclaw
2. Implement WASM plugin loader (scan `~/.rsclaw/plugins/*.wasm`)
3. Implement host functions (bridge to existing `BrowserSession`)
4. Register plugin tools in agent's tool list
5. Route plugin tool calls through WASM runtime

### Phase 2: Jimeng Plugin — Image & Video (Priority)
6. Create `rsclaw-plugin-jimeng` repo (Rust, wasm32-wasip2 target)
7. Implement `jimeng.txt2img` (most complex: model select + prompt + generate + wait + download)
8. Implement `jimeng.img2img`
9. Implement `jimeng.txt2vid`
10. Implement `jimeng.img2vid`
11. Test end-to-end via rsclaw gateway

### Phase 3: Remaining Functions
12. Canvas tools (expand, inpaint, erase, cutout, merge)
13. Digital human (create, talk, clone)
14. Story creation (script, generate, edit)
15. Advanced control (character_lock, pose_copy, camera_control)
16. Utilities (photo_restore, photo_animate, export, history)

### Phase 4: Polish
17. Error recovery (retry on timeout, re-login on session expire)
18. Result caching (avoid re-generating same prompt)
19. Progress reporting (stream status to user while generating)
20. Multi-language prompt optimization

## File Structure

### rsclaw (host side)
```
src/
  plugin/
    mod.rs          # existing
    wasm_runtime.rs # NEW: wasmtime loader + host functions
    manifest.rs     # extend to support .wasm plugins
```

### rsclaw-plugin-jimeng (separate repo)
```
Cargo.toml          # wasm32-wasip2 target
wit/
  world.wit         # WIT interface definition
src/
  lib.rs            # entry: get_manifest() + handle_tool()
  manifest.rs       # tool definitions
  jimeng/
    mod.rs           # router: tool name -> handler
    auth.rs          # login detection + QR code flow
    txt2img.rs       # text-to-image automation
    img2img.rs       # image-to-image
    txt2vid.rs       # text-to-video
    img2vid.rs       # image-to-video
    canvas.rs        # expand/inpaint/erase/cutout/merge
    digital_human.rs # create/talk/clone
    story.rs         # script/generate/edit
    control.rs       # character_lock/pose_copy/camera
    utils.rs         # photo_restore/animate/export/history
    common.rs        # shared helpers (wait_for_generation, select_model, etc.)
```

## Distribution

- Single file: `jimeng.wasm` (~1-5MB)
- Cross-platform: macOS/Linux/Windows (same .wasm binary)
- Install: copy to `~/.rsclaw/plugins/jimeng.wasm`
- Update: replace the file, hot-reload on next tool call
- Private distribution: direct file sharing, no public registry
