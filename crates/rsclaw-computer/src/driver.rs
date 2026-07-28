//! VlmDriver — the model-agnostic GUI-agent loop.
//!
//! The driver owns the per-turn flow:
//!
//!   1. Permission gate — check the cached/persisted decision; if the user has
//!      never decided, register a oneshot, emit a `PermissionRequest` event for
//!      the UI to surface, await the user's response, and record it. `Deny`
//!      short-circuits with `DriverOutcome::PermissionDenied`.
//!   2. Build the system prompt: base GUI-agent skeleton + operator's
//!      `action_spaces()` + matched app-rules.
//!   3. Loop:
//!     a. `operator.screenshot()` captures the current screen / window.
//!     b. Compose a fresh `LlmRequest` with the screenshot + history summary as
//! a single user message — the system prompt stays the same across iterations.
//!     c. `provider.stream(req)` accumulates assistant text until
//! `StreamEvent::Done`.     d. `parser::parse_vlm_response()` extracts a
//! `Vec<ParsedAction>`.     e. Each parsed action maps to an executable
//! [`Action`] via `parsed_to_action` — `finished` / `call_user` terminate with
//! the matching [`DriverOutcome`], everything else runs through
//! `operator.execute(action)` with the result appended to history.     f. Bump
//! the loop counter, then check the abort flag + max_loop.
//!
//! The driver is fully model-agnostic — it works with any vision model
//! that follows the Thought/Action format the prompt asks for. Providers
//! are addressed via the existing [`rsclaw_provider::LlmProvider`]
//! abstraction, so any registered VLM (UI-TARS, Doubao-vision, GPT-4o,
//! Claude vision, Qwen-VL, …) can drive it.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::StreamExt;
use rsclaw_provider::{
    AgentEndpoint, ContentPart, LlmProvider, LlmRequest, Message, MessageContent, Role, StreamEvent,
};
use tracing::{debug, info, warn};

use super::{
    action::{Action, ActionSpec, ExecCtx, MouseButton, ParsedAction, ScrollDir},
    app_rules::AppRuleSet,
    operator::Operator,
    parser::{CoordFormat, parse_vlm_response},
    permission::{PermissionDecision, PermissionRequest, PermissionStore},
    prompt::{PlatformKind, PromptInputs, build_system_prompt},
    status::ComputerUseStatus,
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

/// How to interpret the bare coordinate numbers a model emits inside
/// `start_box` / `end_box`.
///
/// `Normalized` is the target convention (ui-tars-desktop's 0-1000 grid).
/// All rsclaw-vision models (v1, v2) follow the 0-1000 prompt convention.
/// `Pixels` is kept for operators/models that emit raw screenshot pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordSpace {
    /// 0-1000 normalized grid → rescale by `coord / 1000 * screen_dim`.
    Normalized,
    /// Raw pixels of the (possibly downscaled) screenshot the model was
    /// sent → physical = `coord / vision_scale`. Identity when the
    /// screenshot was not downscaled (`vision_scale == 1.0`).
    Pixels,
}

impl CoordSpace {
    /// Pick the coordinate space for a given model id. All rsclaw-vision
    /// models follow the 0-1000 normalized prompt convention.
    pub fn for_model(model: &str) -> Self {
        let _ = model;
        CoordSpace::Normalized
    }
}

pub struct VlmDriver<'a> {
    pub operator: &'a dyn Operator,
    pub provider: Arc<dyn LlmProvider>,
    pub model_name: String,
    pub coord_format: CoordFormat,
    /// How to interpret model-emitted coordinates (0-1000 grid vs raw
    /// screenshot pixels). Derive with [`CoordSpace::for_model`].
    pub coord_space: CoordSpace,
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
    /// AND `headless_auto_allow == true`, the driver behaves as if the
    /// user had answered AllowOnce (CLI use case). When `None` AND
    /// `headless_auto_allow == false`, the driver returns
    /// `PermissionDenied` — defends against a misconfigured gateway
    /// where the broadcast channel is missing but the permission gate
    /// must still block. R3 review I4.
    pub permission_emit: Option<Arc<dyn Fn(PermissionRequest) + Send + Sync + 'a>>,
    /// When true and `permission_emit` is None, silently auto-grant
    /// AllowOnce instead of denying. Set explicitly by CLI callers
    /// (`true`); gateway callers leave `false` so a wiring bug
    /// surfaces as a permission denial instead of a silent bypass.
    pub headless_auto_allow: bool,
    /// Optional sender for `ComputerUseStatus` events — when set, the
    /// driver emits a `Started` at the top of the loop, a `Step` after
    /// each executed action, and a `Finished` on exit. Surfaced to the
    /// settings UI's live status panel. `None` (CLI / tests) makes
    /// emission a no-op.
    pub status_emit: Option<Arc<dyn Fn(ComputerUseStatus) + Send + Sync + 'a>>,
    /// Stable identifier for this run, included in every emitted status
    /// event so the UI can correlate them. Caller-minted (typically
    /// `vlm_drive-<uuid>`).
    pub run_id: String,
    /// Plugin-supplied action space override. When `Some`, the driver
    /// uses these specs in the system prompt instead of
    /// `operator.action_spaces()`. Lets plugins declare exactly which
    /// actions work in their context (e.g. WeChat chat screen).
    pub action_spaces_override: Option<Vec<ActionSpec>>,
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
        // 1. Permission gate. No `Started` is emitted on denial — the permission dialog
        //    already handled the visual; the wrapper will still emit `Finished { kind =
        //    "permission_denied" }` so the UI can surface a brief "denied" state.
        if let Some(deny) = self.permission_gate(instruction).await? {
            return Ok(deny);
        }
        self.emit_started(instruction);

        // 2. Build the system prompt once. The action space + matched app-rules are
        //    stable across the loop, so we don't rebuild.
        // Probe the screen once up-front so the system prompt can
        // anchor "absolute pixel coordinates are in this 2880x1800
        // space" — without that, general LLMs (kimi/gpt-4o/claude
        // vision) tend to emit small numbers (top-left of a region)
        // that any heuristic re-interpretation would distort. The
        // first screenshot is reused for turn 1 so we don't pay the
        // capture cost twice.
        let probe_snap = self
            .operator
            .screenshot()
            .await
            .context("initial screenshot")?;
        let probe_dims = probe_snap.physical_size;
        let mut next_snap: Option<super::action::Screenshot> = Some(probe_snap);

        let action_spaces = self
            .action_spaces_override
            .clone()
            .unwrap_or_else(|| self.operator.action_spaces());
        let matched: Vec<&_> = self.app_rules.match_instruction(instruction);
        let platform = match self.operator.name() {
            "adb" | "android_uiauto" | "iphone_mirror" => PlatformKind::Mobile,
            _ => PlatformKind::Desktop,
        };
        let system_prompt = build_system_prompt(&PromptInputs {
            instruction,
            action_spaces: &action_spaces,
            matched_rules: &matched,
            screen_size: Some(probe_dims),
            platform,
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
        // Distinct from the above: a *completely empty* reply is a
        // transient fleet decoder dropout (rsclaw-vision-v1 occasionally
        // streams zero tokens), NOT a format error the model can correct.
        // Retry the same turn a few times before it counts as unparseable,
        // so a couple of dropped frames don't abort an otherwise-fine run.
        let mut empty_retries = 0usize;
        const MAX_EMPTY_RETRIES: usize = 3;

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
            let screen_w = snap.physical_size.0;
            let screen_h = snap.physical_size.1;
            let scale = snap.scale_factor;
            // Pre-downscale the screenshot to a size under the vision
            // encoder's pixel budget. Otherwise the encoder downscales it
            // server-side and the model reports coordinates in that
            // unknown smaller space (measured ~0.76x on a 2880x1800
            // screen), so every click lands high. Sending a known-size
            // image makes the model's pixel coords invertible: physical =
            // model / vision_scale (see parsed_to_action). No-op for
            // screens already under budget.
            let (vision_png, vision_scale) =
                downscale_for_vision(&snap.png_bytes, screen_w, screen_h);
            let snap_b64 = BASE64.encode(vision_png.as_ref());
            if vision_scale < 1.0 {
                debug!(
                    vision_scale,
                    sent = format!(
                        "{}x{}",
                        (screen_w as f32 * vision_scale).round() as u32,
                        (screen_h as f32 * vision_scale).round() as u32
                    ),
                    "downscaled screenshot for vision model"
                );
            }

            // Signal "model call starting" BEFORE the VLM round-trip
            // (typically 5–30 s on heavy VLMs). Without this, the UI
            // shows nothing between `Started` and the first `Step`,
            // and users / operators assume the agent hung.
            // R3 review I3.
            self.emit_thinking(steps + 1);

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
                rsclaw_hidden: None,
            }];

            let req = LlmRequest {
                fallback_models: Vec::new(),
                model: self.model_name.clone(),
                messages,
                tools: Vec::new(),
                system: Some(system_prompt.clone()),
                // One Thought + one Action line is well under 512 tokens.
                // Capping low bounds the cost/latency of rsclaw-vision-v1's
                // known runaway-repetition (it re-emits `Action:` until the
                // limit); the parser already takes only the first action.
                max_tokens: Some(512),
                temperature: Some(0.0),
                frequency_penalty: None,
                thinking_budget: None,
                endpoint: AgentEndpoint::Vision,
                kv_cache_mode: 0,
                session_key: None,
                system_shared: None,
                user_system: None,
                recall: None,
            };

            // 3c. Stream the prediction. Abort flag is polled per
            //     chunk so a stop click takes effect within ~one
            //     roundtrip instead of waiting for the 30s tail.
            let prediction =
                match stream_prediction(self.provider.as_ref(), req, self.abort.as_ref()).await {
                    Ok(p) => p,
                    Err(e) if e.to_string().contains(STREAM_ABORTED) => {
                        return Ok(DriverOutcome::UserAbort { steps });
                    }
                    Err(e) => {
                        // `{e:#}` joins the full anyhow source chain on one
                        // line — without the alternate flag only the outermost
                        // `.context("provider.stream() failed to start")` shows
                        // and the real cause (HTTP status, connect error,
                        // endpoint-unsupported, routing bail) is swallowed.
                        let chain = format!("{e:#}");
                        warn!(error = %chain, "VLM stream failed");
                        return Ok(DriverOutcome::OperatorError {
                            message: format!("vlm stream: {chain}"),
                            steps,
                        });
                    }
                };
            debug!(prediction_len = prediction.len(), "VLM prediction received");

            // 3d. Parse.
            let mut parsed = parse_vlm_response(&prediction, self.coord_format);
            if parsed.is_empty() {
                // Empty reply → transient decoder dropout. Re-request the
                // same turn (fresh screenshot) instead of feeding a bogus
                // "you forgot Action:" reminder the model can't act on.
                // Only exhausted retries fall through to the format-error
                // path below.
                if prediction.trim().is_empty()
                    && empty_retries < MAX_EMPTY_RETRIES
                {
                    empty_retries += 1;
                    warn!(
                        retries = empty_retries,
                        streak = consecutive_unparseable,
                        "VLM returned an empty prediction (decoder dropout); retrying same turn"
                    );
                    continue;
                }
                // The vision worker occasionally emits a run of literal
                // '?' instead of real tokens — same transient failure as
                // an empty reply, so retry it the same way.
                let all_question_marks = !prediction.trim().is_empty()
                    && prediction.trim().chars().all(|c| c == '?');
                if all_question_marks && empty_retries < MAX_EMPTY_RETRIES {
                    empty_retries += 1;
                    warn!(
                        retries = empty_retries,
                        chars = prediction.trim().len(),
                        streak = consecutive_unparseable,
                        "VLM returned all-'?' prediction (decoder dropout); retrying same turn"
                    );
                    continue;
                }
                empty_retries = 0;
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
            if parsed.len() > 1 {
                warn!(
                    action_count = parsed.len(),
                    first_action = %parsed[0].action_type,
                    "VLM emitted multiple actions in one turn; executing only the first so every action gets a fresh screenshot"
                );
                parsed.truncate(1);
            }
            // Got at least one action — reset the streak counters.
            consecutive_unparseable = 0;
            empty_retries = 0;

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
                        let content = terminal_action_text(&pa, &["content"])
                            .unwrap_or_else(|| pa.thought.clone());
                        let verified = verify_finished_claim(
                            self.provider.as_ref(),
                            &self.model_name,
                            instruction,
                            &history,
                            &pa.thought,
                            &content,
                            &snap_b64,
                            self.abort.as_ref(),
                        )
                        .await;
                        let step = Step {
                            thought: pa.thought.clone(),
                            action_summary: summary,
                            result_ok: verified,
                            result_message: if verified {
                                None
                            } else {
                                Some(
                                    "Completion verifier could not confirm the requested end state from the current screenshot; continue instead of returning completed=true."
                                        .to_owned(),
                                )
                            },
                        };
                        self.emit_step(steps + 1, &step);
                        history.push(step);
                        steps += 1;
                        if verified {
                            info!(steps, "VlmDriver: finished");
                            return Ok(DriverOutcome::Finished { content, steps });
                        }
                        continue;
                    }
                    "call_user" => {
                        let reason = terminal_action_text(&pa, &["reason", "content"])
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
                let Some(action) =
                    parsed_to_action(&pa, screen_w, screen_h, self.coord_space, vision_scale)
                else {
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

                info!(
                    step = steps + 1,
                    coord_space = ?self.coord_space,
                    vision_scale,
                    physical = ?action.coords(),
                    "VLM coord mapping"
                );

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
                info!(
                    step = steps + 1,
                    action = %step.action_summary,
                    ok = step.result_ok,
                    message = ?step.result_message,
                    "VLM action executed"
                );
                self.emit_step(steps + 1, &step);
                history.push(step);
                steps += 1;

                if self.abort.load(Ordering::SeqCst) {
                    return Ok(DriverOutcome::UserAbort { steps });
                }
                if steps >= self.max_loop {
                    return Ok(DriverOutcome::MaxLoop { steps });
                }
                // Let the UI settle after actions that trigger animations
                // (keyboard open, page transition, menu popup) before the
                // next screenshot. Skip for wait (already sleeps) and
                // terminal actions.
                if !matches!(
                    action,
                    Action::Wait { .. }
                        | Action::ClickAndWait { .. }
                        | Action::Finished { .. }
                        | Action::CallUser { .. }
                ) {
                    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
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
                // None we fall back to one of two behaviors based on
                // `headless_auto_allow`:
                //   - true  (CLI / headless test rigs): auto-AllowOnce so the loop can proceed
                //     without UI plumbing.
                //   - false (default; gateway with mis-wired channel): return PermissionDenied.
                //     The pre-fix code silently auto-allowed in both cases, so a gateway that
                //     somehow ended up with permission_emit=None would bypass every permission
                //     gate without an audit trail. R3 review I4.
                let Some(emit) = self.permission_emit.as_ref() else {
                    if self.headless_auto_allow {
                        info!("no permission emitter; headless_auto_allow → AllowOnce");
                        self.permission
                            .record(&self.agent_id, &app, PermissionDecision::AllowOnce)
                            .await
                            .ok();
                        return Ok(None);
                    }
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        app = %app,
                        "permission gate: no emitter wired AND headless_auto_allow=false; denying"
                    );
                    return Ok(Some(DriverOutcome::PermissionDenied));
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
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
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

    fn emit_thinking(&self, step_index: usize) {
        self.emit_status(ComputerUseStatus::Thinking {
            run_id: self.run_id.clone(),
            step_index,
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

fn terminal_action_text(p: &ParsedAction, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| p.raw_args.get(*key))
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

async fn verify_finished_claim(
    provider: &dyn LlmProvider,
    model_name: &str,
    instruction: &str,
    history: &[Step],
    thought: &str,
    content: &str,
    snap_b64: &str,
    abort: &AtomicBool,
) -> bool {
    let history_text = if history.is_empty() {
        "(no previous actions)".to_owned()
    } else {
        history
            .iter()
            .enumerate()
            .map(|(idx, step)| {
                format!(
                    "{}. action={} ok={} message={}",
                    idx + 1,
                    step.action_summary,
                    step.result_ok,
                    step.result_message.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let req = LlmRequest {
            fallback_models: Vec::new(),
        model: model_name.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: format!(
                        "User instruction:\n{instruction}\n\nAction history:\n{history_text}\n\nThe GUI agent now wants to stop with:\nThought: {thought}\nfinished(content='{content}')\n\nLook only at the CURRENT screenshot. Does it prove the user's requested end state is fully achieved? Reply with exactly one line starting with YES or NO, then a short reason. If the screenshot does not prove success, reply NO."
                    ),
                },
                ContentPart::Image {
                    url: format!("data:image/png;base64,{snap_b64}"),
                },
            ]),
            rsclaw_hidden: None,
        }],
        tools: Vec::new(),
        system: Some(
            "You are a strict verifier for a desktop GUI automation run. Approve completion only when the current screenshot visibly proves the user's exact requested goal is done. Submitted-but-not-confirmed, missing target app, loading states, errors, ambiguity, or lack of visible proof must be NO."
                .to_owned(),
        ),
        max_tokens: Some(256),
        temperature: Some(0.0),
        frequency_penalty: None,
        thinking_budget: None,
        endpoint: AgentEndpoint::Vision,
        kv_cache_mode: 0,
        session_key: None,
        system_shared: None,
        user_system: None,
        recall: None,
    };

    match stream_prediction(provider, req, abort).await {
        Ok(verdict) => {
            let normalized = verdict.trim_start().to_ascii_lowercase();
            let ok = normalized.starts_with("yes");
            if !ok {
                info!(
                    verdict = %verdict.chars().take(240).collect::<String>(),
                    "VlmDriver: finished claim rejected"
                );
            }
            ok
        }
        Err(e) => {
            warn!(error = %format!("{e:#}"), "VlmDriver: finished verification failed");
            false
        }
    }
}

/// Sentinel error string returned by `stream_prediction` when the
/// caller's abort flag flipped mid-stream. The driver's outer loop
/// recognises it and exits with `DriverOutcome::UserAbort` instead of
/// surfacing it as an operator error to the user.
const STREAM_ABORTED: &str = "vlm stream: aborted by user";

/// Stream a request to completion and return the accumulated assistant
/// text. Reasoning deltas are folded in as a fallback when the content
/// channel is empty (some models emit only thinking).
///
/// `abort` is polled between every chunk: a single user-initiated stop
/// drops the in-flight stream within ~one chunk roundtrip rather than
/// waiting up to 30s for the full prediction to land.
async fn stream_prediction(
    provider: &dyn LlmProvider,
    req: LlmRequest,
    abort: &AtomicBool,
) -> Result<String> {
    let mut stream = provider
        .stream(req)
        .await
        .context("provider.stream() failed to start")?;
    let mut text = String::new();
    let mut reasoning = String::new();
    while let Some(event) = stream.next().await {
        if abort.load(Ordering::SeqCst) {
            anyhow::bail!(STREAM_ABORTED);
        }
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

/// Max pixels we send to the vision model. Larger screenshots get
/// downscaled by the model's encoder server-side, after which the model
/// reports coordinates in that unknown smaller space (measured ~0.76x on
/// a 2880x1800 screen) and every click lands high. Pre-downscaling to a
/// size safely under that budget ourselves means the model receives our
/// exact dimensions and emits coords in them, so `parsed_to_action` can
/// invert by the known `vision_scale`. 1.44 MP keeps small UI text
/// legible (validated against rsclaw-vision-v1) while staying well under
/// the observed ~3 MP encoder budget.
const MAX_VISION_PIXELS: u64 = 1_440_000;

/// Downscale a screenshot PNG (aspect preserved) to at most
/// [`MAX_VISION_PIXELS`]. Returns the PNG bytes to send and the linear
/// scale factor applied (`sent_dim / original_dim`, `<= 1.0`). A no-op
/// (borrowed bytes, scale `1.0`) when the screen is already under budget.
/// On any decode/encode error it degrades to the original bytes + `1.0`
/// — at worst that single frame reverts to the pre-fix behaviour rather
/// than failing the run.
fn downscale_for_vision(png_bytes: &[u8], w: u32, h: u32) -> (std::borrow::Cow<'_, [u8]>, f32) {
    let pixels = w as u64 * h as u64;
    if pixels == 0 || pixels <= MAX_VISION_PIXELS {
        return (std::borrow::Cow::Borrowed(png_bytes), 1.0);
    }
    let factor = (MAX_VISION_PIXELS as f64 / pixels as f64).sqrt() as f32;
    // Floor (truncate) so the result is guaranteed `<= MAX_VISION_PIXELS`
    // — rounding up could nudge it back over the budget.
    let nw = ((w as f32 * factor) as u32).max(1);
    let nh = ((h as f32 * factor) as u32).max(1);
    match image::load_from_memory(png_bytes) {
        Ok(img) => {
            let small = img.resize_exact(nw, nh, image::imageops::FilterType::Triangle);
            let mut buf = std::io::Cursor::new(Vec::new());
            if small.write_to(&mut buf, image::ImageFormat::Png).is_ok() {
                // Use the actual emitted width for the scale so integer
                // rounding stays exact; aspect is preserved so the height
                // factor is within a pixel of this.
                return (
                    std::borrow::Cow::Owned(buf.into_inner()),
                    nw as f32 / w as f32,
                );
            }
            (std::borrow::Cow::Borrowed(png_bytes), 1.0)
        }
        Err(_) => (std::borrow::Cow::Borrowed(png_bytes), 1.0),
    }
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
    coord_space: CoordSpace,
    vision_scale: f32,
) -> Option<Action> {
    // Coord pipeline depends on what the model emits:
    //   - Normalized (0-1000 grid, the prompt's documented convention and
    //     ui-tars-desktop's defaultNormalizeCoords): rescale `x/1000 * screen_w` to
    //     physical pixels. Resize-invariant, so `vision_scale` does not apply.
    //   - Pixels (rsclaw-vision-v1's actual behaviour): the model emits absolute
    //     pixels of the image it was sent. We pre-downscale that image by
    //     `vision_scale` (<=1.0) to stay under the encoder's budget, so the model's
    //     pixels are in the downscaled space → physical = model / vision_scale.
    //     With no downscale (vision_scale == 1.0) this is identity.
    // In both cases the result is physical pixels; the native operator
    // divides by scale_factor for macOS Retina before driving enigo.
    let inv = if vision_scale > 0.0 {
        1.0 / vision_scale
    } else {
        1.0
    };
    let scale = |c: (f32, f32)| -> (i32, i32) {
        let (x, y) = c;
        match coord_space {
            CoordSpace::Normalized => (
                (x * screen_w as f32 / 1000.0).round() as i32,
                (y * screen_h as f32 / 1000.0).round() as i32,
            ),
            CoordSpace::Pixels => ((x * inv).round() as i32, (y * inv).round() as i32),
        }
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
        "click_and_wait" => {
            let (x, y) = start_xy?;
            let wait_ms = raw
                .get("seconds")
                .and_then(|v| v.parse::<f32>().ok())
                .map(|s| (s * 1000.0) as u32)
                .unwrap_or(2000);
            Some(Action::ClickAndWait { x, y, wait_ms })
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
        "left_double" | "double_click" | "double_tap" => {
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
        "long_press" | "long_click" => {
            let (x, y) = start_xy?;
            let duration_ms = raw
                .get("duration")
                .or_else(|| raw.get("duration_ms"))
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(1000);
            Some(Action::LongPress { x, y, duration_ms })
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
        "press_delete" | "press_del" => Some(Action::Hotkey {
            keys: "delete".to_owned(),
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
    use crate::action::ParsedAction;

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
        let a = parsed_to_action(&p, 2880, 1800, CoordSpace::Normalized, 1.0).unwrap();
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
        let a = parsed_to_action(&p, 2880, 1800, CoordSpace::Normalized, 1.0).unwrap();
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
        let a = parsed_to_action(&p, 1920, 1080, CoordSpace::Normalized, 1.0).unwrap();
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
        let a = parsed_to_action(&p, 2880, 1800, CoordSpace::Normalized, 1.0).unwrap();
        match a {
            Action::Click { x, y, .. } => {
                assert_eq!(x, 115); // 40/1000*2880 = 115.2
                assert_eq!(y, 90); // 50/1000*1800 = 90
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pixels_mode_passes_coords_through_unscaled() {
        // rsclaw-vision-v1 real output: `click(start_box='(1204,1357)')`
        // on a 2880x1800 screen. In Pixels mode the coord IS already a
        // physical pixel → identity. (The Normalized path would rescale it
        // to 1204/1000*2880 = 3468 — off-screen — which was the bug.)
        let mut p = pa("click", &[]);
        p.start = Some((1204.0, 1357.0));
        let a = parsed_to_action(&p, 2880, 1800, CoordSpace::Pixels, 1.0).unwrap();
        match a {
            Action::Click { x, y, .. } => {
                assert_eq!(x, 1204);
                assert_eq!(y, 1357);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pixels_mode_upscales_by_inverse_vision_scale() {
        // When the screenshot was downscaled by 0.5 before sending, the
        // model emits coords in that half-size space; physical = model /
        // 0.5 = model * 2. Matches the live validation: model (688,672)
        // on a 1440x900 image -> (1376,1344) on the 2880x1800 screen.
        let mut p = pa("click", &[]);
        p.start = Some((688.0, 672.0));
        let a = parsed_to_action(&p, 2880, 1800, CoordSpace::Pixels, 0.5).unwrap();
        match a {
            Action::Click { x, y, .. } => {
                assert_eq!(x, 1376);
                assert_eq!(y, 1344);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn downscale_for_vision_noop_under_budget() {
        // A small image (under MAX_VISION_PIXELS) is returned untouched
        // with scale 1.0 — no decode/re-encode.
        let (bytes, scale) = downscale_for_vision(b"not-a-real-png", 1280, 800);
        assert_eq!(scale, 1.0);
        assert_eq!(bytes.as_ref(), b"not-a-real-png");
    }

    #[test]
    fn downscale_for_vision_scales_large_image() {
        // 2880x1800 (5.18 MP) is over budget → factor = sqrt(1.44M/5.18M)
        // ≈ 0.527. Build a real PNG so decode/encode succeed.
        let img = image::RgbImage::from_pixel(2880, 1800, image::Rgb([10, 20, 30]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let png = buf.into_inner();
        let (bytes, scale) = downscale_for_vision(&png, 2880, 1800);
        assert!(scale > 0.5 && scale < 0.55, "scale was {scale}");
        // Sent image must be a valid, smaller PNG under the pixel budget.
        let sent = image::load_from_memory(bytes.as_ref()).unwrap();
        assert!((sent.width() as u64 * sent.height() as u64) <= MAX_VISION_PIXELS);
    }

    #[test]
    fn coord_space_for_model_always_normalized() {
        assert_eq!(
            CoordSpace::for_model("rsclaw-vision-v1"),
            CoordSpace::Normalized
        );
        assert_eq!(
            CoordSpace::for_model("rsclaw/rsclaw-vision-v1"),
            CoordSpace::Normalized
        );
        assert_eq!(
            CoordSpace::for_model("rsclaw-vision-v2"),
            CoordSpace::Normalized
        );
        assert_eq!(CoordSpace::for_model("ui-tars-1.5"), CoordSpace::Normalized);
        assert_eq!(
            CoordSpace::for_model("doubao-vision"),
            CoordSpace::Normalized
        );
        assert_eq!(CoordSpace::for_model(""), CoordSpace::Normalized);
    }

    #[test]
    fn maps_drag_with_both_endpoints() {
        let mut p = pa("drag", &[]);
        p.start = Some((100.0, 100.0));
        p.end = Some((200.0, 200.0));
        let a = parsed_to_action(&p, 1920, 1080, CoordSpace::Normalized, 1.0).unwrap();
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
        let a = parsed_to_action(&p, 1920, 1080, CoordSpace::Normalized, 1.0).unwrap();
        match a {
            Action::Type { text } => assert_eq!(text, "hello world"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn terminal_action_text_prefers_reason_for_call_user() {
        let p = pa("call_user", &[("reason", "login required")]);

        assert_eq!(
            terminal_action_text(&p, &["reason", "content"]).as_deref(),
            Some("login required")
        );
    }

    #[test]
    fn terminal_action_text_keeps_content_for_finished() {
        let p = pa("finished", &[("content", "sent")]);

        assert_eq!(
            terminal_action_text(&p, &["content"]).as_deref(),
            Some("sent")
        );
    }

    #[test]
    fn maps_scroll_with_direction() {
        let mut p = pa("scroll", &[("direction", "up"), ("clicks", "5")]);
        p.start = Some((1000.0, 500.0));
        let a = parsed_to_action(&p, 1920, 1080, CoordSpace::Normalized, 1.0).unwrap();
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
        assert!(parsed_to_action(&p, 1920, 1080, CoordSpace::Normalized, 1.0).is_none());
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
