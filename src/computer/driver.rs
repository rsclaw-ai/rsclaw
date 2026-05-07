//! VlmDriver — the model-agnostic GUI-agent loop.
//!
//! The driver owns the per-turn flow:
//!
//!   1. Permission gate — check the cached/persisted decision; if the
//!      user has never decided, register a oneshot, emit a
//!      `PermissionRequest` event for the UI to surface, await the
//!      user's response, and record it. `Deny` short-circuits with
//!      `DriverOutcome::PermissionDenied`.
//!   2. Build the system prompt: base GUI-agent skeleton + operator's
//!      `action_spaces()` + matched app-rules.
//!   3. Loop:
//!        a. `operator.screenshot()` — capture the current screen / window.
//!        b. Compose a fresh `LlmRequest` with the screenshot + history
//!           summary as a single user message. The system prompt stays
//!           the same across the loop (the model sees the same rules).
//!        c. `provider.stream(req)` and accumulate the assistant text
//!           until `StreamEvent::Done`.
//!        d. `parser::parse_vlm_response()` — extract a `Vec<ParsedAction>`.
//!        e. For each parsed action:
//!             - Map to an executable [`Action`] via `parsed_to_action`.
//!             - `finished` / `call_user` terminate the loop with the
//!               corresponding [`DriverOutcome`].
//!             - `operator.execute(action)` for everything else; record
//!               the result in history.
//!        f. Bump the loop counter and check abort flag + max_loop.
//!
//! The driver is fully model-agnostic — it works with any vision model
//! that follows the Thought/Action format the prompt asks for. Providers
//! are addressed via the existing [`crate::provider::LlmProvider`]
//! abstraction, so any registered VLM (UI-TARS, Doubao-vision, GPT-4o,
//! Claude vision, Qwen-VL, …) can drive it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use tracing::{debug, info, warn};

use super::action::{Action, ExecCtx, MouseButton, ParsedAction, ScrollDir};
use super::app_rules::AppRuleSet;
use super::operator::Operator;
use super::parser::{CoordFormat, parse_vlm_response};
use super::permission::{PermissionDecision, PermissionRequest, PermissionStore};
use super::prompt::{PromptInputs, build_system_prompt};
use super::status::ComputerUseStatus;

use crate::provider::{
    ContentPart, LlmProvider, LlmRequest, Message, MessageContent, Role, StreamEvent,
};

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// Why the driver loop stopped.
#[derive(Debug, Clone)]
pub enum DriverOutcome {
    /// Model emitted `finished(content='...')`. Carries the model's
    /// summary and the number of action steps executed.
    Finished { content: String, steps: usize },
    /// Model emitted `call_user(...)`. Driver returns control to the
    /// agent so the user can be asked for help.
    CallUser { reason: String, steps: usize },
    /// Hit `max_loop` without `finished` / `call_user`.
    MaxLoop { steps: usize },
    /// Caller flipped the abort flag mid-loop.
    UserAbort { steps: usize },
    /// Permission gate returned `Deny` or the request timed out.
    PermissionDenied,
    /// Operator returned a hard error mid-loop.
    OperatorError { message: String, steps: usize },
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// One executed step in the driver loop. Surfaced to callers via the
/// final outcome and persisted in the in-memory history that's fed
/// back into subsequent turns' prompt.
#[derive(Debug, Clone)]
pub struct Step {
    pub thought: String,
    pub action_summary: String,
    pub result_ok: bool,
    pub result_message: Option<String>,
}

pub struct VlmDriver<'a> {
    pub operator: &'a dyn Operator,
    pub provider: Arc<dyn LlmProvider>,
    pub model_name: String,
    pub coord_format: CoordFormat,
    pub max_loop: usize,
    pub abort: Arc<AtomicBool>,
    pub app_rules: &'a AppRuleSet,
    pub permission: Arc<dyn PermissionStore>,
    pub agent_id: String,
    /// Display name of the app the model is being asked to drive
    /// (e.g. "WeChat" / "Doubao"). Used in the permission prompt and
    /// logs. May be empty when the instruction is generic-desktop.
    pub app: String,
    /// Optional sender for `PermissionRequest` events — when set, the
    /// driver emits a request rather than auto-allowing. When `None`
    /// (e.g. CLI / headless), the driver behaves as if the user had
    /// answered AllowOnce.
    pub permission_emit:
        Option<Arc<dyn Fn(PermissionRequest) + Send + Sync + 'a>>,
    /// Optional sender for `ComputerUseStatus` events — when set, the
    /// driver emits a `Started` at the top of the loop, a `Step` after
    /// each executed action, and a `Finished` on exit. Surfaced to the
    /// settings UI's live status panel. `None` (CLI / tests) makes
    /// emission a no-op.
    pub status_emit:
        Option<Arc<dyn Fn(ComputerUseStatus) + Send + Sync + 'a>>,
    /// Stable identifier for this run, included in every emitted status
    /// event so the UI can correlate them. Caller-minted (typically
    /// `ui_tars-<uuid>`).
    pub run_id: String,
}

impl VlmDriver<'_> {
    /// Run the full loop. The instruction is the user's natural-language
    /// goal (e.g. "open WeChat and check the latest 5 messages").
    pub async fn run(&self, instruction: &str) -> Result<DriverOutcome> {
        let outcome = self.run_inner(instruction).await?;
        self.emit_finished(&outcome);
        Ok(outcome)
    }

    async fn run_inner(&self, instruction: &str) -> Result<DriverOutcome> {
        // 1. Permission gate. No `Started` is emitted on denial — the
        //    permission dialog already handled the visual; the wrapper
        //    will still emit `Finished { kind = "permission_denied" }`
        //    so the UI can surface a brief "denied" state.
        if let Some(deny) = self.permission_gate(instruction).await? {
            return Ok(deny);
        }
        self.emit_started(instruction);

        // 2. Build the system prompt once. The action space + matched
        //    app-rules are stable across the loop, so we don't rebuild.
        // Probe the screen once up-front so the system prompt can
        // anchor "absolute pixel coordinates are in this 2880x1800
        // space" — without that, general LLMs (kimi/gpt-4o/claude
        // vision) tend to emit small numbers (top-left of a region)
        // that any heuristic re-interpretation would distort. The
        // first screenshot is reused for turn 1 so we don't pay the
        // capture cost twice.
        let probe_snap = self.operator.screenshot().await.context("initial screenshot")?;
        let probe_dims = probe_snap.physical_size;
        let mut next_snap: Option<super::action::Screenshot> = Some(probe_snap);

        let action_spaces = self.operator.action_spaces();
        let matched: Vec<&_> = self.app_rules.match_instruction(instruction);
        let system_prompt = build_system_prompt(&PromptInputs {
            instruction,
            action_spaces: &action_spaces,
            matched_rules: &matched,
            screen_size: Some(probe_dims),
        });

        info!(
            agent = %self.agent_id,
            app = %self.app,
            operator = %self.operator.name(),
            model = %self.model_name,
            max_loop = self.max_loop,
            matched_rules = matched.len(),
            screen = format!("{}x{}", probe_dims.0, probe_dims.1),
            "VlmDriver.run starting"
        );

        let mut history: Vec<Step> = Vec::new();
        let mut steps = 0usize;
        let mut consecutive_unparseable = 0usize;
        // After this many turns with zero `Action:` lines we abort
        // rather than burning the whole `max_loop` budget. Catches
        // models (especially coding-tuned ones like kimi-for-coding)
        // that fall back to "I should call tool X" meta-prose without
        // ever emitting an Action.
        const MAX_CONSECUTIVE_UNPARSEABLE: usize = 3;

        loop {
            if self.abort.load(Ordering::SeqCst) {
                return Ok(DriverOutcome::UserAbort { steps });
            }
            if steps >= self.max_loop {
                return Ok(DriverOutcome::MaxLoop { steps });
            }

            // 3a. Screenshot. The first iteration reuses the probe
            // snap captured before the prompt was built, so we don't
            // pay the capture cost twice. Subsequent iterations
            // capture fresh.
            let snap = if let Some(s) = next_snap.take() {
                s
            } else {
                match self.operator.screenshot().await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "screenshot failed");
                        return Ok(DriverOutcome::OperatorError {
                            message: format!("screenshot: {e}"),
                            steps,
                        });
                    }
                }
            };
            let snap_b64 = BASE64.encode(&snap.png_bytes);
            let screen_w = snap.physical_size.0;
            let screen_h = snap.physical_size.1;
            let scale = snap.scale_factor;

            // 3b. Build the LLM request.
            let user_text = build_user_message(instruction, &history);
            let messages = vec![Message {
                role: Role::User,
                content: MessageContent::Parts(vec![
                    ContentPart::Text { text: user_text },
                    ContentPart::Image {
                        url: format!("data:image/png;base64,{snap_b64}"),
                    },
                ]),
            }];

            let req = LlmRequest {
                model: self.model_name.clone(),
                messages,
                tools: Vec::new(),
                system: Some(system_prompt.clone()),
                max_tokens: Some(2048),
                temperature: Some(0.0),
                frequency_penalty: None,
                thinking_budget: None,
                kv_cache_mode: 0,
                session_key: None,
            };

            // 3c. Stream the prediction.
            let prediction = match stream_prediction(self.provider.as_ref(), req).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "VLM stream failed");
                    return Ok(DriverOutcome::OperatorError {
                        message: format!("vlm stream: {e}"),
                        steps,
                    });
                }
            };
            debug!(prediction_len = prediction.len(), "VLM prediction received");

            // 3d. Parse.
            let parsed = parse_vlm_response(&prediction, self.coord_format);
            if parsed.is_empty() {
                consecutive_unparseable += 1;
                warn!(
                    prediction = %prediction.chars().take(200).collect::<String>(),
                    streak = consecutive_unparseable,
                    "VLM produced no parseable actions"
                );
                if consecutive_unparseable >= MAX_CONSECUTIVE_UNPARSEABLE {
                    return Ok(DriverOutcome::OperatorError {
                        message: format!(
                            "model produced no `Action:` line for {} consecutive turns. \
                             First reply preview: {}",
                            consecutive_unparseable,
                            prediction.chars().take(200).collect::<String>(),
                        ),
                        steps,
                    });
                }
                // Feed the failure back into history so the next turn's
                // user-message tells the model exactly what went wrong.
                // This is more effective than retrying blind: the
                // model sees the format error and corrects itself.
                let step = Step {
                    thought: String::new(),
                    action_summary: "(no parseable action — your reply was missing the required `Action: ...` line)".to_owned(),
                    result_ok: false,
                    result_message: Some(
                        "Reminder: every reply must end with one `Action:` line picking from the Action Space (click/type/scroll/wait/finished/etc). Do NOT discuss tools."
                            .to_owned(),
                    ),
                };
                self.emit_step(steps + 1, &step);
                history.push(step);
                steps += 1;
                continue;
            }
            // Got at least one action — reset the streak counter.
            consecutive_unparseable = 0;

            // 3e. Execute each action.
            for pa in parsed {
                let summary = summarize_parsed(&pa);
                // Diagnostic: surface the model's raw extracted coords +
                // screen dims at INFO level so coordinate-system bugs are
                // visible without bumping the whole crate to debug. Cheap
                // to keep — fires at most once per executed step.
                info!(
                    step = steps + 1,
                    action_type = %pa.action_type,
                    raw_start = ?pa.start,
                    raw_end = ?pa.end,
                    screen_w,
                    screen_h,
                    scale,
                    "VLM action parsed"
                );

                // Terminal actions short-circuit the whole loop.
                match pa.action_type.as_str() {
                    "finished" => {
                        let content = pa
                            .raw_args
                            .get("content")
                            .cloned()
                            .unwrap_or_else(|| pa.thought.clone());
                        info!(steps, "VlmDriver: finished");
                        let step = Step {
                            thought: pa.thought.clone(),
                            action_summary: summary,
                            result_ok: true,
                            result_message: None,
                        };
                        self.emit_step(steps + 1, &step);
                        history.push(step);
                        return Ok(DriverOutcome::Finished { content, steps });
                    }
                    "call_user" => {
                        let reason = pa
                            .raw_args
                            .get("content")
                            .cloned()
                            .unwrap_or_else(|| pa.thought.clone());
                        info!(steps, "VlmDriver: call_user");
                        let step = Step {
                            thought: pa.thought.clone(),
                            action_summary: summary,
                            result_ok: true,
                            result_message: None,
                        };
                        self.emit_step(steps + 1, &step);
                        history.push(step);
                        return Ok(DriverOutcome::CallUser { reason, steps });
                    }
                    "error_env" => {
                        return Ok(DriverOutcome::OperatorError {
                            message: pa
                                .raw_args
                                .get("content")
                                .cloned()
                                .unwrap_or_else(|| "error_env".to_owned()),
                            steps,
                        });
                    }
                    _ => {}
                }

                // Map ParsedAction → executable Action.
                let Some(action) = parsed_to_action(&pa, screen_w, screen_h) else {
                    warn!(
                        action_type = %pa.action_type,
                        "could not map parsed action; skipping"
                    );
                    let step = Step {
                        thought: pa.thought.clone(),
                        action_summary: summary,
                        result_ok: false,
                        result_message: Some("unmapped action type".to_owned()),
                    };
                    self.emit_step(steps + 1, &step);
                    history.push(step);
                    steps += 1;
                    if steps >= self.max_loop {
                        return Ok(DriverOutcome::MaxLoop { steps });
                    }
                    continue;
                };

                let ctx = ExecCtx {
                    screen_w,
                    screen_h,
                    scale_factor: scale,
                    factors: [screen_w.max(1), screen_h.max(1)],
                };

                let exec_result = match self.operator.execute(&action, &ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(DriverOutcome::OperatorError {
                            message: format!("operator.execute: {e}"),
                            steps,
                        });
                    }
                };

                let step = Step {
                    thought: pa.thought.clone(),
                    action_summary: summary,
                    result_ok: exec_result.ok,
                    result_message: exec_result.message.clone(),
                };
                self.emit_step(steps + 1, &step);
                history.push(step);
                steps += 1;

                if self.abort.load(Ordering::SeqCst) {
                    return Ok(DriverOutcome::UserAbort { steps });
                }
                if steps >= self.max_loop {
                    return Ok(DriverOutcome::MaxLoop { steps });
                }
            }
        }
    }

    /// Run the permission flow. Returns:
    ///   `Ok(None)` when the user has already allowed (or bypass mode is on),
    ///   `Ok(Some(DriverOutcome::PermissionDenied))` when denied,
    ///   `Err(...)` only on infrastructure errors.
    async fn permission_gate(&self, instruction: &str) -> Result<Option<DriverOutcome>> {
        if self.permission.bypass_all() {
            return Ok(None);
        }

        let app = if self.app.is_empty() {
            self.operator.name().to_owned()
        } else {
            self.app.clone()
        };

        match self.permission.check(&self.agent_id, &app).await? {
            Some(PermissionDecision::AllowAlways)
            | Some(PermissionDecision::AllowSession)
            | Some(PermissionDecision::AllowOnce) => Ok(None),
            Some(PermissionDecision::Deny) => Ok(Some(DriverOutcome::PermissionDenied)),
            None => {
                // First-time decision: emit a request to the UI and
                // await the user's response. When `permission_emit` is
                // None (CLI / headless), auto-allow once so the loop
                // can proceed without UI plumbing.
                let Some(emit) = self.permission_emit.as_ref() else {
                    info!("no permission emitter configured; treating as AllowOnce");
                    self.permission
                        .record(&self.agent_id, &app, PermissionDecision::AllowOnce)
                        .await
                        .ok();
                    return Ok(None);
                };

                let request_id = format!(
                    "{}-{}",
                    self.agent_id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                );
                let req = PermissionRequest {
                    request_id: request_id.clone(),
                    agent_id: self.agent_id.clone(),
                    app: app.clone(),
                    reason: format!(
                        "Run a GUI agent loop on {}: \"{}\"",
                        if app.is_empty() {
                            self.operator.name()
                        } else {
                            app.as_str()
                        },
                        truncate(instruction, 200)
                    ),
                    estimated_steps: self.max_loop,
                };
                emit(req);

                // The store side resolves the oneshot when the WS layer
                // calls `resolve_pending_request` with our request_id.
                // The driver doesn't directly own the channel — it
                // checks the store again after a short window. For v1
                // we poll with backoff up to ~60s.
                //
                // (A future revision can have permission_emit return the
                // oneshot rx so the driver awaits directly. Polling is
                // simpler and good enough since user-decision latency
                // is human-scale.)
                let deadline = tokio::time::Instant::now()
                    + std::time::Duration::from_secs(60);
                let mut delay = std::time::Duration::from_millis(200);
                loop {
                    if tokio::time::Instant::now() >= deadline {
                        warn!(
                            agent = %self.agent_id,
                            app = %app,
                            "permission request timed out"
                        );
                        return Ok(Some(DriverOutcome::PermissionDenied));
                    }
                    tokio::time::sleep(delay).await;
                    if self.abort.load(Ordering::SeqCst) {
                        return Ok(Some(DriverOutcome::UserAbort { steps: 0 }));
                    }
                    match self.permission.check(&self.agent_id, &app).await? {
                        Some(PermissionDecision::Deny) => {
                            return Ok(Some(DriverOutcome::PermissionDenied));
                        }
                        Some(_) => return Ok(None),
                        None => {
                            delay = (delay * 2).min(std::time::Duration::from_secs(2));
                        }
                    }
                }
            }
        }
    }

    fn emit_status(&self, ev: ComputerUseStatus) {
        if let Some(emit) = self.status_emit.as_ref() {
            emit(ev);
        }
    }

    fn emit_started(&self, instruction: &str) {
        self.emit_status(ComputerUseStatus::Started {
            run_id: self.run_id.clone(),
            agent_id: self.agent_id.clone(),
            app: self.app.clone(),
            instruction: truncate(instruction, 200),
            max_steps: self.max_loop,
        });
    }

    fn emit_step(&self, step_index: usize, step: &Step) {
        self.emit_status(ComputerUseStatus::Step {
            run_id: self.run_id.clone(),
            step_index,
            action_summary: step.action_summary.clone(),
            thought: truncate(&step.thought, 200),
            result_ok: step.result_ok,
            result_message: step.result_message.as_deref().map(|m| truncate(m, 120)),
        });
    }

    fn emit_finished(&self, outcome: &DriverOutcome) {
        let (kind, steps, summary) = match outcome {
            DriverOutcome::Finished { content, steps } => {
                ("finished", *steps, truncate(content, 200))
            }
            DriverOutcome::CallUser { reason, steps } => {
                ("call_user", *steps, truncate(reason, 200))
            }
            DriverOutcome::MaxLoop { steps } => ("max_loop", *steps, String::new()),
            DriverOutcome::UserAbort { steps } => ("user_abort", *steps, String::new()),
            DriverOutcome::PermissionDenied => ("permission_denied", 0, String::new()),
            DriverOutcome::OperatorError { message, steps } => {
                ("operator_error", *steps, truncate(message, 200))
            }
        };
        self.emit_status(ComputerUseStatus::Finished {
            run_id: self.run_id.clone(),
            outcome_kind: kind.to_owned(),
            steps,
            summary,
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compose the user-facing message for one turn. We feed the model
/// (a) the original instruction and (b) a compact log of the previous
/// steps so it can plan the next one.
fn build_user_message(instruction: &str, history: &[Step]) -> String {
    if history.is_empty() {
        return format!("Task: {instruction}");
    }
    let mut s = String::with_capacity(512 + history.len() * 64);
    s.push_str("Task: ");
    s.push_str(instruction);
    s.push_str("\n\nHistory (most recent last):\n");
    // Cap to the last 10 steps so the prompt stays bounded.
    let tail = if history.len() > 10 {
        &history[history.len() - 10..]
    } else {
        history
    };
    for (i, step) in tail.iter().enumerate() {
        s.push_str(&format!("{}. {}", i + 1, step.action_summary));
        if let Some(msg) = step.result_message.as_deref() {
            if !msg.is_empty() {
                s.push_str(&format!(" → {}", truncate(msg, 80)));
            }
        }
        s.push('\n');
    }
    s
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn summarize_parsed(p: &ParsedAction) -> String {
    let pretty_args = p
        .raw_args
        .iter()
        .map(|(k, v)| format!("{k}={}", truncate(v, 40)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({pretty_args})", p.action_type)
}

/// Stream a request to completion and return the accumulated assistant
/// text. Reasoning deltas are folded in as a fallback when the content
/// channel is empty (some models emit only thinking).
async fn stream_prediction(
    provider: &dyn LlmProvider,
    req: LlmRequest,
) -> Result<String> {
    let mut stream = provider
        .stream(req)
        .await
        .context("provider.stream() failed to start")?;
    let mut text = String::new();
    let mut reasoning = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta(d) => text.push_str(&d),
            StreamEvent::ReasoningDelta(d) => reasoning.push_str(&d),
            StreamEvent::ToolCall { .. } => {} // unused in VLM-driven flow
            StreamEvent::Done { .. } => break,
            StreamEvent::Error(e) => anyhow::bail!("VLM stream error: {e}"),
        }
    }
    Ok(if text.trim().is_empty() {
        reasoning
    } else {
        text
    })
}

/// Translate a parser-emitted [`ParsedAction`] into an executable
/// [`Action`]. Returns `None` for action types this layer can't map
/// (caller will skip + log).
///
/// Coordinates are treated as **absolute pixels in the screenshot's
/// physical pixel space** (i.e. the size the VLM literally saw). The
/// system prompt tells the model the screenshot dimensions and asks
/// for absolute pixels. The native operator divides by `scale_factor`
/// for macOS Retina before driving enigo (see `scale_for_input`).
///
/// Why no 0-1000 normalization here: general LLMs (kimi-for-coding,
/// gpt-4o, claude vision, etc.) are NOT GUI-fine-tuned and don't know
/// the UI-TARS 1.5 normalized convention. They look at the screenshot
/// and emit pixel-space coords. A heuristic that auto-renormalises
/// "small" coordinates was rewriting valid clicks at the top-left
/// (e.g. an OS menu bar at y=80) into the screen middle — exactly the
/// "everything clicks the wrong place" symptom we hit in testing.
/// To support UI-TARS 1.5 (which emits 0-1000 internally), add an
/// explicit `coord_space="normalized"` config flag and a separate
/// codepath; do NOT bring back a magnitude heuristic.
fn parsed_to_action(
    p: &ParsedAction,
    screen_w: u32,
    screen_h: u32,
) -> Option<Action> {
    // Coord pipeline: model emits in a 0-1000 normalized grid (see
    // the prompt's "Coordinate Space" section + UITARS_1_5-style
    // examples), so we rescale `x/1000 * screen_w` to physical
    // pixels. This matches ui-tars-desktop's defaultNormalizeCoords
    // pipeline and works across UI-TARS, Doubao-vision, Claude,
    // GPT-4o, kimi etc. — the in-context examples anchor any
    // vision-capable LLM into the 0-1000 range, and the same rescale
    // takes them home.
    let scale = |c: (f32, f32)| -> (i32, i32) {
        let (x, y) = c;
        (
            (x * screen_w as f32 / 1000.0).round() as i32,
            (y * screen_h as f32 / 1000.0).round() as i32,
        )
    };

    let start_xy = p.start.map(scale);
    let end_xy = p.end.map(scale);
    let raw = &p.raw_args;

    match p.action_type.as_str() {
        "click" | "left_click" | "left_single" | "tap" => {
            let (x, y) = start_xy?;
            Some(Action::Click {
                x,
                y,
                button: MouseButton::Left,
            })
        }
        "right_click" | "right_single" => {
            let (x, y) = start_xy?;
            Some(Action::Click {
                x,
                y,
                button: MouseButton::Right,
            })
        }
        "middle_click" => {
            let (x, y) = start_xy?;
            Some(Action::Click {
                x,
                y,
                button: MouseButton::Middle,
            })
        }
        "left_double" | "double_click" => {
            let (x, y) = start_xy?;
            Some(Action::DoubleClick { x, y })
        }
        "mouse_move" | "hover" => {
            let (x, y) = start_xy?;
            Some(Action::MouseMove { x, y })
        }
        "drag" | "swipe" | "left_click_drag" | "select" => {
            let (a, b) = start_xy?;
            let (c, d) = end_xy?;
            Some(Action::Drag {
                from_x: a,
                from_y: b,
                to_x: c,
                to_y: d,
            })
        }
        "long_press" => {
            // Approximated as a click; iOS / Android operators may
            // upgrade this to a hold internally.
            let (x, y) = start_xy?;
            Some(Action::Click {
                x,
                y,
                button: MouseButton::Left,
            })
        }
        "scroll" => {
            let (x, y) = start_xy.unwrap_or((screen_w as i32 / 2, screen_h as i32 / 2));
            let dir = match raw.get("direction").map(String::as_str) {
                Some("up") => ScrollDir::Up,
                Some("down") => ScrollDir::Down,
                Some("left") => ScrollDir::Left,
                Some("right") => ScrollDir::Right,
                _ => ScrollDir::Down,
            };
            let clicks = raw
                .get("clicks")
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(3);
            Some(Action::Scroll {
                x,
                y,
                direction: dir,
                clicks,
            })
        }
        "type" => {
            let text = raw.get("content").cloned().unwrap_or_default();
            Some(Action::Type { text })
        }
        "hotkey" => {
            let keys = raw
                .get("key")
                .or_else(|| raw.get("hotkey"))
                .cloned()
                .unwrap_or_default();
            Some(Action::Hotkey { keys })
        }
        "press_home" => Some(Action::Hotkey {
            keys: "press_home".to_owned(),
        }),
        "press_back" => Some(Action::Hotkey {
            keys: "press_back".to_owned(),
        }),
        "activate_app" | "open_app" | "launch_app" => {
            let app = raw
                .get("app")
                .or_else(|| raw.get("app_name"))
                .or_else(|| raw.get("name"))
                .cloned()
                .unwrap_or_default();
            Some(Action::ActivateApp { app })
        }
        "wait" => {
            // Default 1s — most UI feedback (button click reaction,
            // small DOM updates, scroll repaint) is sub-second. The
            // upstream UI-TARS used 5s as a worst-case ceiling, but
            // burning 5s per turn is a huge UX cost in tight loops.
            // Models that genuinely need longer can pass
            // `wait(seconds=5)`; the operator clamps to [0, 60].
            let seconds = raw
                .get("seconds")
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(1.0);
            Some(Action::Wait { seconds })
        }
        _ => None,
    }
}

// Suppress an unused-import lint for BTreeMap when no test exercises it.
const _: fn() -> BTreeMap<String, String> = BTreeMap::new;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::action::ParsedAction;

    fn pa(action_type: &str, args: &[(&str, &str)]) -> ParsedAction {
        let mut raw_args = BTreeMap::new();
        for (k, v) in args {
            raw_args.insert((*k).to_owned(), (*v).to_owned());
        }
        ParsedAction {
            thought: String::new(),
            action_type: action_type.to_owned(),
            raw_args,
            start: None,
            end: None,
        }
    }

    // Coordinate convention: 0-1000 normalized grid (matches
    // ui-tars-desktop's defaultNormalizeCoords). Whatever the model
    // emits inside `start_box` / `end_box` is treated as a point on
    // the 0-1000 plane and rescaled to physical pixels via
    // `x / 1000 * screen_w`. The system prompt's Coordinate Space
    // section + UITARS_1_5-style examples anchor any vision-capable
    // LLM into this range without per-model fine-tuning.

    #[test]
    fn maps_click_top_left_corner() {
        // (0, 0) on the grid → (0, 0) on the screen.
        let mut p = pa("click", &[]);
        p.start = Some((0.0, 0.0));
        let a = parsed_to_action(&p, 2880, 1800).unwrap();
        match a {
            Action::Click { x, y, .. } => {
                assert_eq!(x, 0);
                assert_eq!(y, 0);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn maps_click_centre_of_screen() {
        // (500, 500) on the grid → midpoint of the screen.
        let mut p = pa("click", &[]);
        p.start = Some((500.0, 500.0));
        let a = parsed_to_action(&p, 2880, 1800).unwrap();
        match a {
            Action::Click { x, y, .. } => {
                assert_eq!(x, 1440);
                assert_eq!(y, 900);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn maps_click_bottom_right_corner() {
        // (1000, 1000) on the grid → bottom-right of the screen.
        let mut p = pa("click", &[]);
        p.start = Some((1000.0, 1000.0));
        let a = parsed_to_action(&p, 1920, 1080).unwrap();
        match a {
            Action::Click { x, y, .. } => {
                assert_eq!(x, 1920);
                assert_eq!(y, 1080);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn maps_click_arbitrary_point() {
        // (40, 50) — small grid coords (e.g. WeChat search box top
        // left). Must NOT be passed through as raw pixels.
        let mut p = pa("click", &[]);
        p.start = Some((40.0, 50.0));
        let a = parsed_to_action(&p, 2880, 1800).unwrap();
        match a {
            Action::Click { x, y, .. } => {
                assert_eq!(x, 115); // 40/1000*2880 = 115.2
                assert_eq!(y, 90); // 50/1000*1800 = 90
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn maps_drag_with_both_endpoints() {
        let mut p = pa("drag", &[]);
        p.start = Some((100.0, 100.0));
        p.end = Some((200.0, 200.0));
        let a = parsed_to_action(&p, 1920, 1080).unwrap();
        match a {
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                assert_eq!(from_x, 192);
                assert_eq!(from_y, 108);
                assert_eq!(to_x, 384);
                assert_eq!(to_y, 216);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn maps_type_action() {
        let p = pa("type", &[("content", "hello world")]);
        let a = parsed_to_action(&p, 1920, 1080).unwrap();
        match a {
            Action::Type { text } => assert_eq!(text, "hello world"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn maps_scroll_with_direction() {
        let mut p = pa("scroll", &[("direction", "up"), ("clicks", "5")]);
        p.start = Some((1000.0, 500.0));
        let a = parsed_to_action(&p, 1920, 1080).unwrap();
        match a {
            Action::Scroll {
                direction, clicks, ..
            } => {
                assert!(matches!(direction, ScrollDir::Up));
                assert_eq!(clicks, 5);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn unmapped_action_returns_none() {
        let p = pa("teleport", &[]);
        assert!(parsed_to_action(&p, 1920, 1080).is_none());
    }

    #[test]
    fn build_user_message_with_history() {
        let history = vec![
            Step {
                thought: String::new(),
                action_summary: "click(start_box=...)".to_owned(),
                result_ok: true,
                result_message: None,
            },
            Step {
                thought: String::new(),
                action_summary: "type(content=hello)".to_owned(),
                result_ok: false,
                result_message: Some("not focused".to_owned()),
            },
        ];
        let msg = build_user_message("send a hi", &history);
        assert!(msg.contains("Task: send a hi"));
        assert!(msg.contains("1. click"));
        assert!(msg.contains("2. type"));
        assert!(msg.contains("not focused"));
    }

    #[test]
    fn build_user_message_no_history() {
        let msg = build_user_message("open WeChat", &[]);
        assert_eq!(msg, "Task: open WeChat");
    }
}
