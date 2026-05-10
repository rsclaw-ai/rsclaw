//! Computer-use tool dispatcher.
//!
//! Thin layer over `crate::computer`. The action / parser / driver /
//! permission subsystems live in `src/computer/`; this file only
//! translates between the legacy `serde_json::Value` tool-call schema
//! and those typed building blocks.
//!
//! Action routing:
//! - `screenshot` → `tool_screenshot`. Kept here so the resize / disk-save
//!   contract used by chat history is preserved.
//! - `mouse_*` / `click` / `drag` / `scroll` / `type` / `key` / `hold_key` /
//!   `wait` → translated to [`Action`] and executed via [`NativeOperator`].
//! - `triple_click` / `cursor_position` / `get_active_window` / `ui_tree`
//!   → inline subprocess helpers; they're queries, not part of the
//!   operator action space.
//! - `list_app_rules` / `get_app_rule` → backed by [`AppRuleSet`].
//! - `ui_tars` → end-to-end VLM loop via [`VlmDriver`]; supports any
//!   vision-capable LLM provider.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::platform::{display_logical_scale, jpeg_dimensions, powershell_hidden};
use crate::computer::{
    Action, ExecCtx, MouseButton, ScrollDir,
    app_rules::AppRuleSet,
    driver::{DriverOutcome, VlmDriver},
    operator::Operator as _,
    operators::native::NativeOperator,
    parser::CoordFormat,
    permission::{PermissionRequest, RedbPermissionStore},
    status::ComputerUseStatus,
};

impl super::runtime::AgentRuntime {
    /// Top-level dispatcher for the `computer_use` tool. Routes by
    /// `args["action"]` to one of the handlers documented in the module
    /// header.
    pub(crate) async fn tool_computer_use(
        &self,
        ctx: &super::runtime::RunContext,
        args: Value,
    ) -> Result<Value> {
        let action_str = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("computer_use: `action` required"))?;

        match action_str {
            "screenshot" => {
                let region = args.get("region").and_then(|v| {
                    let x = v.get("x")?.as_f64()?;
                    let y = v.get("y")?.as_f64()?;
                    let w = v.get("width")?.as_f64()?;
                    let h = v.get("height")?.as_f64()?;
                    if w <= 0.0 || h <= 0.0 {
                        return None;
                    }
                    Some((x, y, w, h))
                });
                let max_long_edge = args["max_long_edge_px"].as_u64().map(|n| n as u32);
                self.tool_screenshot(region, max_long_edge, None).await
            }

            // ---- Operator-backed actions ---------------------------------
            "mouse_move"
            | "mouse_click"
            | "left_click"
            | "double_click"
            | "right_click"
            | "middle_click"
            | "drag"
            | "scroll"
            | "type"
            | "key"
            | "hold_key"
            | "wait" => dispatch_via_native_operator(action_str, &args).await,

            // ---- Inline query helpers ------------------------------------
            "triple_click" => triple_click(&args).await,
            "cursor_position" => cursor_position().await,
            "get_active_window" => active_window_title().await,
            "ui_tree" => ui_tree().await,

            // ---- App-rule helpers ----------------------------------------
            "list_app_rules" | "list_skills" => list_app_rules(),
            "get_app_rule" | "get_skill" => get_app_rule(&args),

            // ---- End-to-end VLM driver -----------------------------------
            "ui_tars" => self.tool_ui_tars(ctx, &args).await,

            other => Err(anyhow!(
                "computer_use: unsupported action `{other}` \
                 (supported: screenshot, mouse_move, mouse_click, double_click, triple_click, \
                 right_click, middle_click, drag, scroll, type, key, hold_key, cursor_position, \
                 get_active_window, ui_tree, list_app_rules, get_app_rule, wait, ui_tars)"
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Screenshot — preserved verbatim from the previous implementation
    // because the resize/save contract (image_path, mime, original_w/h,
    // scale) is depended on by chat-history rendering and tool callers.
    // -----------------------------------------------------------------------
    async fn tool_screenshot(
        &self,
        region: Option<(f64, f64, f64, f64)>,
        max_long_edge_px: Option<u32>,
        quality: Option<u32>,
    ) -> Result<Value> {
        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let tmp_path = std::env::temp_dir().join(format!("rsclaw_screen-{nonce}.png"));
        let tmp_path_str = tmp_path.to_string_lossy().to_string();

        struct TmpFile(std::path::PathBuf);
        impl Drop for TmpFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _tmp_guard = TmpFile(tmp_path.clone());

        let output = if is_macos {
            let mut cmd = tokio::process::Command::new("screencapture");
            cmd.arg("-x");
            if let Some((rx, ry, rw, rh)) = region {
                let scale = display_logical_scale();
                let lx = (rx / scale).round() as i64;
                let ly = (ry / scale).round() as i64;
                let lw = (rw / scale).round().max(1.0) as i64;
                let lh = (rh / scale).round().max(1.0) as i64;
                cmd.args(["-R", &format!("{lx},{ly},{lw},{lh}")]);
            }
            cmd.arg(&tmp_path_str).output().await
        } else if is_windows {
            let (rx, ry, rw, rh) = region
                .map(|(x, y, w, h)| (x as i64, y as i64, w as i64, h as i64))
                .unwrap_or((-1, -1, -1, -1));
            let region_init = if rw > 0 {
                format!("$rx={rx}; $ry={ry}; $rw={rw}; $rh={rh};")
            } else {
                "$rx=-1; $ry=-1; $rw=-1; $rh=-1;".to_owned()
            };
            let script = format!(
                r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
{region_init}
if ($rw -gt 0) {{
    $bounds = New-Object System.Drawing.Rectangle($rx, $ry, $rw, $rh)
}} else {{
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
}}
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bitmap.Save('{tmp_path_str}')
$graphics.Dispose()
$bitmap.Dispose()
"#
            );
            powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
        } else {
            let res = if let Some((rx, ry, rw, rh)) = region {
                let area = format!(
                    "{x},{y},{w},{h}",
                    x = rx as i64,
                    y = ry as i64,
                    w = rw as i64,
                    h = rh as i64
                );
                tokio::process::Command::new("scrot")
                    .args(["-a", &area, &tmp_path_str])
                    .output()
                    .await
            } else {
                tokio::process::Command::new("scrot")
                    .arg(&tmp_path_str)
                    .output()
                    .await
            };
            if !matches!(&res, Ok(o) if o.status.success()) {
                let mut cmd = tokio::process::Command::new("import");
                cmd.args(["-window", "root"]);
                if let Some((rx, ry, rw, rh)) = region {
                    cmd.args(["-crop", &format!(
                        "{w}x{h}+{x}+{y}",
                        x = rx as i64,
                        y = ry as i64,
                        w = rw as i64,
                        h = rh as i64
                    )]);
                }
                cmd.arg(&tmp_path_str).output().await
            } else {
                res
            }
        }
        .map_err(|e| anyhow!("computer_use screenshot: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("computer_use screenshot failed: {stderr}"));
        }

        let raw_bytes = tokio::fs::read(&tmp_path)
            .await
            .map_err(|e| anyhow!("computer_use: failed to read screenshot: {e}"))?;
        let (orig_w, orig_h) = if raw_bytes.len() >= 24 {
            let w = u32::from_be_bytes([raw_bytes[16], raw_bytes[17], raw_bytes[18], raw_bytes[19]]);
            let h = u32::from_be_bytes([raw_bytes[20], raw_bytes[21], raw_bytes[22], raw_bytes[23]]);
            (w, h)
        } else {
            (0, 0)
        };

        const DEFAULT_MAX_LONG_EDGE: u32 = 1024;
        let jpg_quality = quality.unwrap_or(30).clamp(0, 100);
        let keep_png = jpg_quality == 0;
        let max_long_edge = max_long_edge_px
            .filter(|n| *n >= 64 && *n <= 8192)
            .unwrap_or(DEFAULT_MAX_LONG_EDGE);
        let long_edge = orig_w.max(orig_h);
        let need_resize = long_edge > max_long_edge;
        let max_long_edge_str = max_long_edge.to_string();

        let out_path = if keep_png {
            std::env::temp_dir().join(format!("rsclaw_screen_out-{nonce}.png"))
        } else {
            std::env::temp_dir().join(format!("rsclaw_screen_out-{nonce}.jpg"))
        };
        let out_str = out_path.to_string_lossy().to_string();
        let _out_guard = TmpFile(out_path.clone());

        let converted = if is_macos {
            if keep_png && !need_resize {
                false
            } else {
                let quality_str = jpg_quality.to_string();
                let mut sips_args: Vec<&str> = vec![];
                if need_resize {
                    sips_args.extend_from_slice(&["-Z", &max_long_edge_str]);
                }
                if keep_png {
                    sips_args.extend_from_slice(&[
                        "-s", "format", "png",
                        &tmp_path_str,
                        "--out", &out_str,
                    ]);
                } else {
                    sips_args.extend_from_slice(&[
                        "-s", "format", "jpeg",
                        "-s", "formatOptions", &quality_str,
                        &tmp_path_str,
                        "--out", &out_str,
                    ]);
                }
                tokio::process::Command::new("sips")
                    .args(&sips_args)
                    .output()
                    .await
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            }
        } else if is_windows {
            let (new_w, new_h) = if need_resize {
                let ratio = max_long_edge as f64 / long_edge as f64;
                (
                    ((orig_w as f64) * ratio).round().max(1.0) as u32,
                    ((orig_h as f64) * ratio).round().max(1.0) as u32,
                )
            } else {
                (orig_w, orig_h)
            };
            let script = format!(
                r#"
Add-Type -AssemblyName System.Drawing
$src = [System.Drawing.Image]::FromFile('{tmp_path_str}')
$dst = New-Object System.Drawing.Bitmap({new_w}, {new_h})
$g = [System.Drawing.Graphics]::FromImage($dst)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.DrawImage($src, 0, 0, {new_w}, {new_h})
$codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object {{ $_.MimeType -eq 'image/jpeg' }}
$params = New-Object System.Drawing.Imaging.EncoderParameters(1)
$params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter([System.Drawing.Imaging.Encoder]::Quality, [long]{jpg_quality})
$dst.Save('{out_str}', $codec, $params)
$g.Dispose(); $dst.Dispose(); $src.Dispose()
"#
            );
            powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false)
        } else {
            let mut convert_args: Vec<&str> = vec![&tmp_path_str];
            let resize_box = format!("{m}x{m}>", m = max_long_edge);
            if need_resize {
                convert_args.extend_from_slice(&["-resize", &resize_box]);
            }
            let quality_str = if keep_png { "100".to_string() } else { jpg_quality.to_string() };
            convert_args.extend_from_slice(&["-quality", &quality_str, &out_str]);
            tokio::process::Command::new("convert")
                .args(&convert_args)
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false)
        };

        let (bytes, mime) = if converted {
            let b = tokio::fs::read(&out_path).await.unwrap_or(raw_bytes);
            let _ = tokio::fs::remove_file(&out_path).await;
            if keep_png { (b, "image/png") } else { (b, "image/jpeg") }
        } else {
            (raw_bytes, "image/png")
        };
        let _ = tokio::fs::remove_file(&tmp_path).await;

        let (width, height) = if mime == "image/jpeg" {
            jpeg_dimensions(&bytes).unwrap_or((0, 0))
        } else if bytes.len() >= 24 {
            let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
            let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            (w, h)
        } else {
            (0, 0)
        };

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let ext = if mime == "image/jpeg" { "jpg" } else { "png" };
        let save_dir = dirs_next::download_dir()
            .unwrap_or_else(|| {
                dirs_next::home_dir()
                    .unwrap_or_else(crate::config::loader::base_dir)
                    .join("Downloads")
            })
            .join("rsclaw")
            .join("screenshots");
        tokio::fs::create_dir_all(&save_dir)
            .await
            .map_err(|e| anyhow!("computer_use screenshot: create_dir: {e}"))?;
        let save_path = save_dir.join(format!("{nanos:x}.{ext}"));
        tokio::fs::write(&save_path, &bytes)
            .await
            .map_err(|e| anyhow!("computer_use screenshot: write: {e}"))?;

        let scale = if width > 0 && orig_w > width { orig_w as f64 / width as f64 } else { 1.0 };

        Ok(json!({
            "action": "screenshot",
            "image_path": save_path.to_string_lossy(),
            "mime": mime,
            "width": width,
            "height": height,
            "original_width": orig_w,
            "original_height": orig_h,
            "scale": scale
        }))
    }

    // -----------------------------------------------------------------------
    // ui_tars — end-to-end VLM loop. Resolves vision model -> provider,
    // builds VlmDriver, runs the instruction, translates outcome to JSON.
    // -----------------------------------------------------------------------
    async fn tool_ui_tars(
        &self,
        ctx: &super::runtime::RunContext,
        args: &Value,
    ) -> Result<Value> {
        let instruction = args["instruction"]
            .as_str()
            .ok_or_else(|| anyhow!("computer_use ui_tars: `instruction` required"))?;
        let max_steps = args["max_steps"].as_u64().unwrap_or(30) as usize;
        // Plan B: fire-and-forget mode. Default opt-in via `async: true`
        // (or alias `background: true`). When set, the driver runs in a
        // detached tokio task, the tool call returns a `task_id`
        // immediately, and the agent inbox stays free to process
        // heartbeats / other channels. On completion, an internal
        // AgentMessage is injected back into the same session so the
        // upstream LLM can see the outcome and respond to the user.
        let async_mode = args["async"].as_bool().unwrap_or(false)
            || args["background"].as_bool().unwrap_or(false);

        // 1. Resolve the vision model. Returns a clear, actionable error
        //    when nothing usable is configured.
        let model_name = self
            .resolve_vision_model_name()
            .map_err(|msg| anyhow!("{msg}"))?;
        let (prov_name, _model_id) = self.providers.resolve_model(&model_name);
        let provider = self.providers.get(prov_name)?;

        // 2. Load app-rules from the per-user data dir. Failures here are
        //    non-fatal — the driver just won't have app-specific guidance.
        let app_rules_dir = crate::config::loader::base_dir()
            .join("tools")
            .join("computer_use")
            .join("app-rules");
        let app_rules = AppRuleSet::load_dir(&app_rules_dir).unwrap_or_default();

        // 3. Permission store. Use the gateway-shared instance when
        //    available (so the WS handler's `resolve_pending_request`
        //    can complete drivers' awaited oneshots). Outside the
        //    gateway (tests / CLI) fall back to a per-call bypass-all
        //    store.
        let permission: Arc<RedbPermissionStore> = match &self.computer_permission {
            Some(p) => Arc::clone(p),
            None => Arc::new(RedbPermissionStore::new(self.store.db.clone(), true)),
        };

        // Permission_emit broadcasts the request on the gateway's
        // permission channel so the Tauri UI can show a modal. When
        // the channel is missing (CLI), permission_emit is None and
        // the driver behaves as if the user had answered AllowOnce.
        let permission_emit: Option<
            Arc<dyn Fn(PermissionRequest) + Send + Sync>,
        > = self.computer_permission_tx.as_ref().map(|tx| {
            let tx = tx.clone();
            Arc::new(move |req: PermissionRequest| {
                let _ = tx.send(req);
            }) as Arc<dyn Fn(PermissionRequest) + Send + Sync>
        });

        // Status events feed the live status panel. Same `None`-as-no-op
        // pattern as permission_emit.
        let status_emit: Option<
            Arc<dyn Fn(ComputerUseStatus) + Send + Sync>,
        > = self.computer_status_tx.as_ref().map(|tx| {
            let tx = tx.clone();
            Arc::new(move |ev: ComputerUseStatus| {
                let _ = tx.send(ev);
            }) as Arc<dyn Fn(ComputerUseStatus) + Send + Sync>
        });

        let abort = Arc::new(AtomicBool::new(false));
        let agent_id = self.handle.id.clone();
        let app_label = derive_app_label(instruction);
        // Run id is `ui_tars-<uuid>` for both sync and async paths so the
        // UI status panel can correlate Started → Step* → Finished.
        let run_id = format!("ui_tars-{}", uuid::Uuid::new_v4().simple());

        // Register this run's abort flag so the HTTP abort endpoint can
        // flip it. Cleared in a guard pattern at every driver-exit path
        // (ok / err / timeout / panic). Outside the gateway
        // (`computer_runs == None`) this is a no-op.
        if let Some(reg) = self.computer_runs.as_ref() {
            reg.write().await.insert(run_id.clone(), Arc::clone(&abort));
        }

        // ---------- ASYNC PATH (fire-and-forget) ----------
        if async_mode {
            // Reuse `run_id` as the task id so logs / status events / wake
            // messages all correlate via a single identifier.
            let task_id = run_id.clone();
            let instruction_owned = instruction.to_owned();
            let permission_clone = Arc::clone(&permission);
            let provider_clone = Arc::clone(&provider);
            let model_name_clone = model_name.clone();
            let app_rules_clone = app_rules.clone();
            let permission_emit_clone = permission_emit.clone();
            let status_emit_clone = status_emit.clone();
            let run_id_clone = run_id.clone();
            let run_id_for_dereg = run_id.clone();
            let computer_runs_clone = self.computer_runs.clone();
            let self_handle = Arc::clone(&self.handle);
            let notification_tx = self.notification_tx.clone();
            let session_key = ctx.session_key.clone();
            let channel = ctx.channel.clone();
            let peer_id = ctx.peer_id.clone();
            let chat_id = ctx.chat_id.clone();
            let abort_clone = Arc::clone(&abort);
            let task_id_for_log = task_id.clone();

            tokio::spawn(async move {
                tracing::info!(
                    task_id = %task_id_for_log,
                    agent = %agent_id,
                    "ui_tars: detached task started"
                );
                // Build the driver inside the spawned task — NativeOperator
                // has no state and is cheap to construct; this avoids
                // Send/Sync complications around borrowing operator/
                // app_rules across the spawn boundary.
                let operator = NativeOperator::new();
                let driver = VlmDriver {
                    operator: &operator,
                    provider: provider_clone,
                    model_name: model_name_clone,
                    coord_format: CoordFormat::Auto,
                    max_loop: max_steps,
                    abort: abort_clone,
                    app_rules: &app_rules_clone,
                    permission: permission_clone,
                    agent_id: agent_id.clone(),
                    app: app_label,
                    permission_emit: permission_emit_clone,
                    status_emit: status_emit_clone,
                    run_id: run_id_clone,
                };

                let outcome = driver.run(&instruction_owned).await
                    .unwrap_or_else(|e| DriverOutcome::OperatorError {
                        message: format!("driver run failed: {e}"),
                        steps: 0,
                    });
                // Driver has exited — drop the abort-flag registry entry
                // so the run_id is no longer abortable.
                if let Some(reg) = computer_runs_clone.as_ref() {
                    reg.write().await.remove(&run_id_for_dereg);
                }

                // Build a human-readable result for the wake message.
                let result_text = match &outcome {
                    DriverOutcome::Finished { content, steps } => {
                        format!(
                            "[ui_tars task {task_id_for_log} completed in {steps} steps] {content}"
                        )
                    }
                    DriverOutcome::CallUser { reason, steps } => {
                        format!(
                            "[ui_tars task {task_id_for_log} needs user input after {steps} steps] {reason}"
                        )
                    }
                    DriverOutcome::MaxLoop { steps } => {
                        format!(
                            "[ui_tars task {task_id_for_log}] hit max_loop ({steps} steps) without finishing — task may be incomplete"
                        )
                    }
                    DriverOutcome::UserAbort { steps } => {
                        format!("[ui_tars task {task_id_for_log}] aborted after {steps} steps")
                    }
                    DriverOutcome::PermissionDenied => {
                        format!("[ui_tars task {task_id_for_log}] permission denied — user declined")
                    }
                    DriverOutcome::OperatorError { message, steps } => {
                        format!(
                            "[ui_tars task {task_id_for_log}] failed after {steps} steps: {message}"
                        )
                    }
                };

                tracing::info!(
                    task_id = %task_id_for_log,
                    "ui_tars: detached task finished, waking parent agent"
                );

                // Wake the parent agent: send an internal message so the
                // LLM observes the task outcome on the next turn and
                // replies to the user. Mirrors the pattern used by
                // `tool_agent_task` in tools_agent.rs.
                let (wake_tx, wake_rx) =
                    tokio::sync::oneshot::channel::<crate::agent::AgentReply>();
                let wake_msg = crate::agent::AgentMessage {
                    session_key: session_key.clone(),
                    text: result_text,
                    channel: channel.clone(),
                    peer_id: peer_id.clone(),
                    chat_id: chat_id.clone(),
                    reply_tx: wake_tx,
                    extra_tools: vec![],
                    images: vec![],
                    files: vec![],
                    account: None,
                };
                if let Err(e) = self_handle.tx.send(wake_msg).await {
                    tracing::warn!(
                        task_id = %task_id_for_log,
                        "ui_tars: failed to wake parent agent: {e}"
                    );
                    return;
                }

                // Forward the agent's reply (composed from the result
                // text) to the originating channel so the user sees
                // the outcome. Same plumbing as `tool_agent_task`.
                if let Ok(reply) = wake_rx.await {
                    if reply.text.is_empty() {
                        return;
                    }
                    let Some(ref ntx) = notification_tx else { return };
                    let target = if !chat_id.is_empty() { chat_id } else { peer_id };
                    if target.is_empty() || channel.is_empty() || channel == "system" || channel == "cron" {
                        return;
                    }
                    let body = if channel == "ws" || channel == "desktop" {
                        crate::channel::outbound_with_kind(
                            crate::channel::outbound_kind::TASK_COMPLETE,
                            reply.text,
                        )
                    } else {
                        reply.text
                    };
                    let _ = ntx.send(crate::channel::OutboundMessage {
                        target_id: target,
                        is_group: false,
                        text: body,
                        reply_to: None,
                        images: reply.images.clone(),
                        files: reply.files.clone(),
                        channel: Some(channel),
                        account: None,
                    });
                }
            });

            return Ok(json!({
                "action":     "ui_tars",
                "task_id":    task_id,
                "status":     "started",
                "instruction": instruction,
                "estimated_seconds": (max_steps as u64) * 10,
                "hint": "Task is running in the background. Acknowledge to the user (e.g. \"我已经开始处理\"); the result will arrive as a separate message when the task finishes.",
            }));
        }

        // ---------- SYNC PATH (legacy behaviour, default) ----------
        // 4. Build the driver. NativeOperator is the default; Phase 2 will
        //    branch on instruction keywords to swap in IphoneMirrorOperator
        //    or AdbOperator.
        let operator = NativeOperator::new();

        let run_id_for_dereg = run_id.clone();
        let driver = VlmDriver {
            operator: &operator,
            provider,
            model_name: model_name.clone(),
            coord_format: CoordFormat::Auto,
            max_loop: max_steps,
            abort: abort.clone(),
            app_rules: &app_rules,
            permission,
            agent_id,
            app: app_label,
            permission_emit,
            status_emit,
            run_id,
        };

        // Hard timeout safety net. The agent runtime processes one
        // message at a time per agent; a runaway ui_tars loop here
        // blocks heartbeats and other channels until it returns. Cap
        // the whole driver run at 4 minutes so the heartbeat (300s
        // timeout) never fires while we're inside.
        // The async path above is the proper fix; this safety net
        // covers the (legacy) sync path.
        const UI_TARS_HARD_TIMEOUT_SECS: u64 = 240;
        let driver_result = tokio::time::timeout(
            std::time::Duration::from_secs(UI_TARS_HARD_TIMEOUT_SECS),
            driver.run(instruction),
        )
        .await;
        // Driver has exited (or hard-timed out) — drop the abort-flag
        // registry entry. Done here, before unwrapping the result, so
        // the entry never outlives the driver regardless of which exit
        // path fires.
        if let Some(reg) = self.computer_runs.as_ref() {
            reg.write().await.remove(&run_id_for_dereg);
        }
        let outcome = match driver_result {
            Ok(res) => res?,
            Err(_) => {
                abort.store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::warn!(
                    instruction,
                    timeout_secs = UI_TARS_HARD_TIMEOUT_SECS,
                    "ui_tars hard timeout exceeded; aborting driver"
                );
                DriverOutcome::OperatorError {
                    message: format!(
                        "ui_tars exceeded the {UI_TARS_HARD_TIMEOUT_SECS}s hard timeout. \
                         For long-running tasks, pass `async: true` to run \
                         in the background instead."
                    ),
                    steps: max_steps,
                }
            }
        };

        // 5. Translate the outcome to the legacy JSON shape so existing
        //    callers (chat history rendering, agent loop) keep working.
        Ok(match outcome {
            DriverOutcome::Finished { content, steps } => json!({
                "action": "ui_tars",
                "instruction": instruction,
                "completed": true,
                "steps_taken": steps,
                "result": content,
            }),
            DriverOutcome::CallUser { reason, steps } => json!({
                "action": "ui_tars",
                "instruction": instruction,
                "completed": false,
                "steps_taken": steps,
                "call_user": reason,
            }),
            DriverOutcome::MaxLoop { steps } => json!({
                "action": "ui_tars",
                "instruction": instruction,
                "completed": false,
                "steps_taken": steps,
                "error": "max_loop reached",
            }),
            DriverOutcome::UserAbort { steps } => json!({
                "action": "ui_tars",
                "instruction": instruction,
                "completed": false,
                "steps_taken": steps,
                "error": "user aborted",
            }),
            DriverOutcome::PermissionDenied => json!({
                "action": "ui_tars",
                "instruction": instruction,
                "completed": false,
                "steps_taken": 0,
                "error": "permission denied",
            }),
            DriverOutcome::OperatorError { message, steps } => json!({
                "action": "ui_tars",
                "instruction": instruction,
                "completed": false,
                "steps_taken": steps,
                "error": message,
            }),
        })
    }

}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Translate a JSON tool-call arg map to an `Action` and execute it on a
/// `NativeOperator`. Returns the legacy JSON shape `{action, ok}`.
async fn dispatch_via_native_operator(action: &str, args: &Value) -> Result<Value> {
    let action_enum = parse_native_action(action, args)?;

    let ctx = ExecCtx {
        screen_w: 0,
        screen_h: 0,
        scale_factor: display_logical_scale() as f32,
        factors: [1000, 1000],
    };
    let op = NativeOperator::new();
    let out = op
        .execute(&action_enum, &ctx)
        .await
        .map_err(|e| anyhow!("operator execute: {e}"))?;
    Ok(json!({
        "action": action,
        "ok": out.ok,
        "message": out.message,
    }))
}

/// Build an `Action` from `(action_name, args)`. Returns `Err` only on
/// unrecognised names (the dispatcher pre-filters), never on missing
/// args — sensible defaults are used so the model can omit them.
fn parse_native_action(action: &str, args: &Value) -> Result<Action> {
    let xy = || -> (i32, i32) {
        (
            args["x"].as_f64().unwrap_or(0.0) as i32,
            args["y"].as_f64().unwrap_or(0.0) as i32,
        )
    };
    match action {
        "mouse_move" => {
            let (x, y) = xy();
            Ok(Action::MouseMove { x, y })
        }
        "mouse_click" | "left_click" => {
            let (x, y) = xy();
            let button = match args["button"].as_str().unwrap_or("left") {
                "right" => MouseButton::Right,
                "middle" => MouseButton::Middle,
                _ => MouseButton::Left,
            };
            Ok(Action::Click { x, y, button })
        }
        "right_click" => {
            let (x, y) = xy();
            Ok(Action::Click { x, y, button: MouseButton::Right })
        }
        "middle_click" => {
            let (x, y) = xy();
            Ok(Action::Click { x, y, button: MouseButton::Middle })
        }
        "double_click" => {
            let (x, y) = xy();
            Ok(Action::DoubleClick { x, y })
        }
        "drag" => {
            let (from_x, from_y) = xy();
            let to_x = args["to_x"].as_f64().unwrap_or(0.0) as i32;
            let to_y = args["to_y"].as_f64().unwrap_or(0.0) as i32;
            Ok(Action::Drag { from_x, from_y, to_x, to_y })
        }
        "scroll" => {
            let (x, y) = xy();
            let direction = match args["direction"].as_str().unwrap_or("down") {
                "up" => ScrollDir::Up,
                "left" => ScrollDir::Left,
                "right" => ScrollDir::Right,
                _ => ScrollDir::Down,
            };
            let clicks = args["clicks"].as_i64().unwrap_or(3) as i32;
            Ok(Action::Scroll { x, y, direction, clicks })
        }
        "type" => {
            let text = args["text"].as_str().unwrap_or("").to_owned();
            Ok(Action::Type { text })
        }
        "key" => {
            let keys = args["key"].as_str().unwrap_or("").to_owned();
            Ok(Action::Hotkey { keys })
        }
        "hold_key" => {
            let key = args["key"].as_str().unwrap_or("").to_owned();
            let seconds = args["seconds"].as_f64().unwrap_or(0.5) as f32;
            Ok(Action::HoldKey { key, seconds })
        }
        "wait" => {
            let ms = args["ms"].as_u64().unwrap_or(500).min(60_000);
            Ok(Action::Wait { seconds: (ms as f32) / 1000.0 })
        }
        other => Err(anyhow!(
            "dispatch_via_native_operator: unrecognised action `{other}`"
        )),
    }
}

/// Triple-click is not part of the operator action space (it's rarely
/// useful and our mobile / browser operators have no equivalent), so
/// shell out per platform.
async fn triple_click(args: &Value) -> Result<Value> {
    let is_macos = cfg!(target_os = "macos");
    let is_windows = cfg!(target_os = "windows");
    let x = args["x"].as_f64().unwrap_or(0.0);
    let y = args["y"].as_f64().unwrap_or(0.0);
    let scale = if is_macos { display_logical_scale() } else { 1.0 };
    let lx = (x / scale).round() as i64;
    let ly = (y / scale).round() as i64;

    if is_macos {
        let arg = format!("tc:{lx},{ly}");
        tokio::process::Command::new("cliclick")
            .arg(&arg)
            .output()
            .await
            .map_err(|e| anyhow!("cliclick triple_click: {e}"))?;
    } else if is_windows {
        super::platform::win_mouse_click(lx, ly, "left", 3).await?;
    } else {
        tokio::process::Command::new("xdotool")
            .args([
                "mousemove",
                "--sync",
                &lx.to_string(),
                &ly.to_string(),
                "click",
                "--repeat",
                "3",
                "--delay",
                "50",
                "1",
            ])
            .output()
            .await
            .map_err(|e| anyhow!("xdotool triple_click: {e}"))?;
    }
    Ok(json!({"action": "triple_click", "ok": true}))
}

/// Read the current cursor position (physical pixels).
async fn cursor_position() -> Result<Value> {
    let is_macos = cfg!(target_os = "macos");
    let is_windows = cfg!(target_os = "windows");
    let pos = if is_macos {
        let output = tokio::process::Command::new("cliclick")
            .arg("p:.")
            .output()
            .await
            .map_err(|e| anyhow!("cliclick: {e}"))?;
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else if is_windows {
        let output = powershell_hidden()
            .args([
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $p = [System.Windows.Forms.Cursor]::Position; \"$($p.X),$($p.Y)\"",
            ])
            .output()
            .await
            .map_err(|e| anyhow!("powershell: {e}"))?;
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        let output = tokio::process::Command::new("xdotool")
            .args(["getmouselocation", "--shell"])
            .output()
            .await
            .map_err(|e| anyhow!("xdotool: {e}"))?;
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let parts: Vec<&str> = pos.split(',').collect();
    let (cx, cy) = if parts.len() >= 2 {
        (
            parts[0].trim().parse::<i64>().unwrap_or(0),
            parts[1].trim().parse::<i64>().unwrap_or(0),
        )
    } else {
        let mut cx = 0i64;
        let mut cy = 0i64;
        for line in pos.lines() {
            if let Some(v) = line.strip_prefix("X=") {
                cx = v.parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("Y=") {
                cy = v.parse().unwrap_or(0);
            }
        }
        (cx, cy)
    };
    Ok(json!({"action": "cursor_position", "x": cx, "y": cy}))
}

/// Get the title of the currently focused window for context-awareness
/// in agent loops. macOS returns `"<app> — <window>"`.
async fn active_window_title() -> Result<Value> {
    let is_macos = cfg!(target_os = "macos");
    let is_windows = cfg!(target_os = "windows");
    let title = if is_macos {
        let output = tokio::process::Command::new("osascript")
            .args([
                "-e",
                "tell application \"System Events\" to get name of first process whose frontmost is true",
            ])
            .output()
            .await
            .map_err(|e| anyhow!("osascript: {e}"))?;
        let app = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let output2 = tokio::process::Command::new("osascript")
            .args([
                "-e",
                "tell application \"System Events\" to get name of front window of (first process whose frontmost is true)",
            ])
            .output()
            .await;
        let win = output2
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if win.is_empty() { app } else { format!("{app} — {win}") }
    } else if is_windows {
        let output = powershell_hidden()
            .args([
                "-Command",
                "Add-Type @\"\nusing System;\nusing System.Runtime.InteropServices;\npublic class WinTitle {\n  [DllImport(\"user32.dll\")] static extern IntPtr GetForegroundWindow();\n  [DllImport(\"user32.dll\")] static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);\n  public static string Get() {\n    var sb = new System.Text.StringBuilder(256);\n    GetWindowText(GetForegroundWindow(), sb, 256);\n    return sb.ToString();\n  }\n}\n\"@\n[WinTitle]::Get()",
            ])
            .output()
            .await
            .map_err(|e| anyhow!("powershell: {e}"))?;
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        let output = tokio::process::Command::new("xdotool")
            .args(["getactivewindow", "getwindowname"])
            .output()
            .await
            .map_err(|e| anyhow!("xdotool: {e}"))?;
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    Ok(json!({"action": "get_active_window", "title": title}))
}

/// Dump the focused window's accessibility tree (interactive elements
/// with role / label / bounding box). This is a best-effort introspection
/// helper for agents driving native apps; many Electron apps return
/// nothing useful and the caller should fall back to screenshot reasoning.
async fn ui_tree() -> Result<Value> {
    let is_macos = cfg!(target_os = "macos");
    let is_windows = cfg!(target_os = "windows");
    let elements_json = if is_macos {
        let script = r#"
import Cocoa
import ApplicationServices
struct UiEl: Codable { let role: String; let label: String; let x: Int; let y: Int; let w: Int; let h: Int }
func ch(_ e: AXUIElement) -> [AXUIElement] { var v: CFTypeRef?; guard AXUIElementCopyAttributeValue(e, kAXChildrenAttribute as CFString, &v) == .success, let a = v as? [AXUIElement] else { return [] }; return a }
func a(_ e: AXUIElement, _ k: String) -> String? { var v: CFTypeRef?; guard AXUIElementCopyAttributeValue(e, k as CFString, &v) == .success, let s = v else { return nil }; return "\(s)" }
func pos(_ e: AXUIElement) -> (Int,Int)? { var v: CFTypeRef?; guard AXUIElementCopyAttributeValue(e, kAXPositionAttribute as CFString, &v) == .success, let ax = v else { return nil }; var p = CGPoint.zero; AXValueGetValue(ax as! AXValue, .cgPoint, &p); return (Int(p.x),Int(p.y)) }
func sz(_ e: AXUIElement) -> (Int,Int)? { var v: CFTypeRef?; guard AXUIElementCopyAttributeValue(e, kAXSizeAttribute as CFString, &v) == .success, let ax = v else { return nil }; var s = CGSize.zero; AXValueGetValue(ax as! AXValue, .cgSize, &s); return (Int(s.width),Int(s.height)) }
let roles: Set<String> = ["AXButton","AXTextField","AXTextArea","AXCheckBox","AXRadioButton","AXComboBox","AXPopUpButton","AXSlider","AXLink","AXMenuItem","AXMenuBarItem","AXTab","AXDisclosureTriangle","AXSearchField","AXSecureTextField","AXStaticText","AXCell"]
var r: [UiEl] = []
func walk(_ e: AXUIElement, _ d: Int) { guard d < 20, r.count < 200 else { return }; let ro = a(e, kAXRoleAttribute) ?? ""; if roles.contains(ro) { let l = a(e, kAXTitleAttribute) ?? a(e, kAXDescriptionAttribute) ?? a(e, kAXValueAttribute) ?? ""; if let (x,y) = pos(e), let (w,h) = sz(e), w > 0, h > 0 { r.append(UiEl(role: ro, label: String(l.prefix(100)), x: x, y: y, w: w, h: h)) } }; for c in ch(e) { walk(c, d+1) } }
guard let app = NSWorkspace.shared.frontmostApplication else { print("[]"); exit(0) }
let ax = AXUIElementCreateApplication(app.processIdentifier)
var wv: CFTypeRef?
if AXUIElementCopyAttributeValue(ax, kAXFocusedWindowAttribute as CFString, &wv) == .success, let w = wv { walk(w as! AXUIElement, 0) } else { walk(ax, 0) }
if let d = try? JSONEncoder().encode(r), let j = String(data: d, encoding: .utf8) { print(j) } else { print("[]") }
"#;
        let output = tokio::process::Command::new("swift")
            .args(["-e", script])
            .output()
            .await
            .map_err(|e| anyhow!("swift ui_tree: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("ui_tree (macos): {stderr}"));
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else if is_windows {
        let ps_script = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$auto = [System.Windows.Automation.AutomationElement]::FocusedElement
$root = $null
try {
    $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
    $cur = $auto
    while ($cur -ne $null) {
        $parent = $walker.GetParent($cur)
        if ($parent -eq [System.Windows.Automation.AutomationElement]::RootElement) { $root = $cur; break }
        $cur = $parent
    }
} catch { }
if ($root -eq $null) { $root = $auto }
$results = @()
$count = 0
function Walk($el, $depth) {
    if ($depth -gt 20 -or $script:count -ge 200) { return }
    $ct = $el.Current.ControlType.ProgrammaticName
    $interactive = @('ControlType.Button','ControlType.Edit','ControlType.CheckBox','ControlType.RadioButton',
        'ControlType.ComboBox','ControlType.Slider','ControlType.Hyperlink','ControlType.MenuItem',
        'ControlType.Tab','ControlType.TabItem','ControlType.Text','ControlType.DataItem','ControlType.ListItem')
    if ($interactive -contains $ct) {
        $rect = $el.Current.BoundingRectangle
        if ($rect.Width -gt 0 -and $rect.Height -gt 0) {
            $label = $el.Current.Name
            if ([string]::IsNullOrEmpty($label)) { $label = $el.Current.AutomationId }
            $script:results += @{ role=$ct; label=$label; x=[int]$rect.X; y=[int]$rect.Y; w=[int]$rect.Width; h=[int]$rect.Height }
            $script:count++
        }
    }
    try {
        $child = $walker.GetFirstChild($el)
        while ($child -ne $null) { Walk $child ($depth+1); $child = $walker.GetNextSibling($child) }
    } catch { }
}
$walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
Walk $root 0
$results | ConvertTo-Json -Compress
"#;
        let output = powershell_hidden()
            .args(["-Command", ps_script])
            .output()
            .await
            .map_err(|e| anyhow!("powershell ui_tree: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("ui_tree (windows): {stderr}"));
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        let py_script = r#"
import json
try:
    import gi
    gi.require_version('Atspi', '2.0')
    from gi.repository import Atspi
    desktop = Atspi.get_desktop(0)
    results = []
    interactive = {'push button','toggle button','text','password text','combo box',
                   'check box','radio button','slider','link','menu item','tab','table cell','list item'}
    def walk(el, depth):
        if depth > 20 or len(results) >= 200: return
        try:
            role = el.get_role_name()
            if role in interactive:
                c = el.get_component_iface()
                if c:
                    rect = c.get_extents(Atspi.CoordType.SCREEN)
                    if rect.width > 0 and rect.height > 0:
                        name = el.get_name() or ''
                        results.append({'role': role, 'label': name[:100], 'x': rect.x, 'y': rect.y, 'w': rect.width, 'h': rect.height})
            for i in range(el.get_child_count()):
                walk(el.get_child_at_index(i), depth + 1)
        except: pass
    for i in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(i)
        if app:
            for j in range(app.get_child_count()):
                win = app.get_child_at_index(j)
                if win:
                    try:
                        si = win.get_state_set()
                        if si.contains(Atspi.StateType.ACTIVE):
                            walk(win, 0)
                            if results: break
                    except: pass
            if results: break
    print(json.dumps(results))
except ImportError:
    print('[]')
"#;
        let output = tokio::process::Command::new("python3")
            .args(["-c", py_script])
            .output()
            .await
            .map_err(|e| anyhow!("python3 ui_tree: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("ui_tree (linux): {stderr}"));
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let elements: Value = serde_json::from_str(&elements_json).unwrap_or_else(|_| json!([]));
    let count = elements.as_array().map(|a| a.len()).unwrap_or(0);
    Ok(json!({"action": "ui_tree", "count": count, "elements": elements}))
}

/// List the per-app desktop-automation playbooks installed in
/// `~/.rsclaw/tools/computer_use/app-rules/`.
fn list_app_rules() -> Result<Value> {
    let app_rules_dir = crate::config::loader::base_dir()
        .join("tools")
        .join("computer_use")
        .join("app-rules");

    let rules: Vec<Value> = match AppRuleSet::load_dir(&app_rules_dir) {
        Ok(set) => set
            .rules
            .iter()
            .map(|r| json!({"name": r.name, "description": r.description}))
            .collect(),
        Err(_) => Vec::new(),
    };
    Ok(json!({
        "action": "list_app_rules",
        "app_rules_dir": app_rules_dir.to_string_lossy(),
        "count": rules.len(),
        "app_rules": rules,
    }))
}

/// Read a single app-rule body by name (frontmatter stripped).
fn get_app_rule(args: &Value) -> Result<Value> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow!("get_app_rule: `name` required"))?;
    let app_rules_dir = crate::config::loader::base_dir()
        .join("tools")
        .join("computer_use")
        .join("app-rules");
    let path = app_rules_dir.join(format!("{name}.md"));
    if !path.exists() {
        return Err(anyhow!(
            "app-rule not found: {name} (looked in {})",
            app_rules_dir.display()
        ));
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("read app-rule {name}: {e}"))?;
    let body = if content.starts_with("---") {
        content.splitn(3, "---").nth(2).unwrap_or(&content).trim()
    } else {
        content.trim()
    };
    Ok(json!({"action": "get_app_rule", "name": name, "content": body}))
}

/// Best-effort label of the app the model is being asked to drive,
/// used in the permission prompt. Empty string is fine — the dialog
/// just shows "RsClaw is about to control your computer" without a
/// specific app name. The full keyword set is in
/// `tools/computer_use/app-rules/*.md` (canonical aliases).
fn derive_app_label(instruction: &str) -> String {
    let lower = instruction.to_lowercase();
    for (label, keywords) in [
        ("WeChat", &["wechat", "微信", "weixin"][..]),
        ("Doubao", &["doubao", "豆包"][..]),
        ("Telegram", &["telegram"][..]),
        ("Safari", &["safari"][..]),
        ("Google Chrome", &["chrome"][..]),
        ("Finder", &["finder"][..]),
        ("Slack", &["slack"][..]),
        ("Notes", &["notes", "备忘录"][..]),
        ("Douyin", &["douyin", "抖音"][..]),
        ("TongHuaShun", &["tonghuashun", "同花顺"][..]),
    ] {
        if keywords.iter().any(|k| {
            if k.is_ascii() {
                lower.contains(k)
            } else {
                instruction.contains(k)
            }
        }) {
            return label.to_owned();
        }
    }
    String::new()
}
