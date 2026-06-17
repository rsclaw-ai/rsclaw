//! CapAgentManager — owns four respawnable driver slots, one per
//! `AgentKind`. Each slot fronts an actor task that owns a
//! `Box<cap_rs::driver::Driver>`.
//!
//! Each `tool_cap` call returns IMMEDIATELY with `status: submitted` once
//! the prompt is queued. The actual coding-agent work runs in the actor
//! task; progress is reported live to the user's IM channel via
//! `notif_tx`, and the final summary is reinjected into the originating
//! agent's inbox via `inbox_tx` on a `:cap-followup` sub-session. This
//! mirrors the old `tool_acp*` behaviour (LLM ack-fast + background
//! delivery) — see `src/agent/tools_acp.rs` in commit 9deb237 for the
//! original pattern.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use cap_rs::core::{AgentEvent, ClientFrame, Content};
use cap_rs::driver::Driver;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};

use super::{bridge, permission};
use rsclaw_types::OutboundMessage;
use rsclaw_i18n as i18n;

/// Which coding agent a `tool_cap` call dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claudecode,
    Openclaude,
    Opencode,
    Codex,
    Qoder,
}

impl AgentKind {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "claudecode" | "claude" => Some(Self::Claudecode),
            "openclaude" => Some(Self::Openclaude),
            "opencode" => Some(Self::Opencode),
            "codex" => Some(Self::Codex),
            "qoder" => Some(Self::Qoder),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claudecode => "claudecode",
            Self::Openclaude => "openclaude",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Qoder => "qoder",
        }
    }

    /// Display name used in i18n strings (e.g. "OpenCode", "Claude Code").
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claudecode => "Claude Code",
            Self::Openclaude => "OpenClaude",
            Self::Opencode => "OpenCode",
            Self::Codex => "Codex",
            Self::Qoder => "Qoder",
        }
    }
}

/// IM channel routing for live progress + completion notifications.
/// `lang` is a static i18n key (e.g. "en", "zh") — see `rsclaw_i18n::resolve_lang`.
#[derive(Clone)]
pub struct NotifTarget {
    pub tx: broadcast::Sender<OutboundMessage>,
    pub target_id: String,
    pub is_group: bool,
    pub channel: String,
    pub lang: &'static str,
}

/// Where to reinject the completion as a follow-up agent message so the
/// LLM can act on the result (e.g. send_file). The follow-up runs on a
/// `:cap-followup` sub-session to avoid re-activating the user-visible
/// session.
#[derive(Clone)]
pub struct InboxTarget {
    pub session_key: String,
    pub channel: String,
    pub peer_id: String,
    pub chat_id: String,
}

/// Returned to the LLM immediately after the prompt is queued.
pub struct Submitted {
    pub session_id: String,
}

pub enum ToolCapRequest {
    Prompt {
        task: String,
        notif: Option<NotifTarget>,
        inbox: Option<InboxTarget>,
        /// Resolved as soon as the initial notification is sent — BEFORE
        /// the driver actually runs the prompt. The LLM gets back
        /// `Submitted { session_id }` and moves on.
        reply: oneshot::Sender<Result<Submitted>>,
    },
}

type Slot = Arc<RwLock<Option<mpsc::Sender<ToolCapRequest>>>>;

pub struct CapAgentManager {
    claudecode: Slot,
    openclaude: Slot,
    opencode: Slot,
    codex: Slot,
    qoder: Slot,
    bus: broadcast::Sender<rsclaw_events::AgentEvent>,
}

impl CapAgentManager {
    pub fn new(bus: broadcast::Sender<rsclaw_events::AgentEvent>) -> Self {
        Self {
            claudecode: Arc::new(RwLock::new(None)),
            openclaude: Arc::new(RwLock::new(None)),
            opencode: Arc::new(RwLock::new(None)),
            codex: Arc::new(RwLock::new(None)),
            qoder: Arc::new(RwLock::new(None)),
            bus,
        }
    }

    fn slot(&self, kind: AgentKind) -> Slot {
        match kind {
            AgentKind::Claudecode => Arc::clone(&self.claudecode),
            AgentKind::Openclaude => Arc::clone(&self.openclaude),
            AgentKind::Opencode => Arc::clone(&self.opencode),
            AgentKind::Codex => Arc::clone(&self.codex),
            AgentKind::Qoder => Arc::clone(&self.qoder),
        }
    }

    /// Queue a prompt on the agent's driver. Returns as soon as the
    /// initial notification fires; the actual driver run + completion
    /// happens asynchronously and is delivered via `notif` (IM live
    /// progress) and `inbox` (agent inbox reinjection on Done).
    pub async fn dispatch_async(
        &self,
        kind: AgentKind,
        task: String,
        cwd: std::path::PathBuf,
        notif: Option<NotifTarget>,
        inbox: Option<InboxTarget>,
    ) -> Result<Submitted> {
        let tx = self.ensure_actor(kind, cwd).await?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ToolCapRequest::Prompt {
            task,
            notif,
            inbox,
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow!("cap actor for {} closed", kind.as_str()))?;
        reply_rx.await.map_err(|_| anyhow!("cap actor dropped reply"))?
    }

    async fn ensure_actor(
        &self,
        kind: AgentKind,
        cwd: std::path::PathBuf,
    ) -> Result<mpsc::Sender<ToolCapRequest>> {
        let slot = self.slot(kind);
        {
            let g = slot.read().await;
            if let Some(tx) = g.as_ref() {
                return Ok(tx.clone());
            }
        }
        let mut g = slot.write().await;
        if let Some(tx) = g.as_ref() {
            return Ok(tx.clone());
        }
        let driver = spawn_driver(kind, &cwd).await?;
        let (tx, rx) = mpsc::channel::<ToolCapRequest>(8);
        let bus = self.bus.clone();
        let slot_for_actor = Arc::clone(&slot);
        tokio::spawn(actor_loop(kind, cwd, driver, rx, bus, slot_for_actor));
        *g = Some(tx.clone());
        Ok(tx)
    }
}

pub async fn spawn_driver(
    kind: AgentKind,
    cwd: &std::path::Path,
) -> Result<Box<dyn Driver>> {
    spawn_driver_inner(kind, cwd, ResumeMode::None).await
}

/// Spawn a driver that resumes an existing on-disk session by the
/// agent's NATIVE session id (claudecode UUID, opencode session id,
/// codex thread_id). Each driver maps the id to its own CLI flag /
/// subcommand:
///
///   * claudecode/openclaude: `claude --resume <uuid>` (replays
///     `~/.claude/projects/<path>/<uuid>.jsonl` into memory).
///   * opencode: `opencode run --session <id>`.
///   * codex: `codex exec resume <thread_id>`.
///
/// If the id doesn't match a stored session for that agent the CLI
/// errors out at spawn — cap-rs surfaces the error to the actor which
/// propagates it back to /cap-resume's caller.
pub async fn spawn_driver_resume(
    kind: AgentKind,
    cwd: &std::path::Path,
    agent_session_id: &str,
) -> Result<Box<dyn Driver>> {
    spawn_driver_inner(kind, cwd, ResumeMode::ById(agent_session_id)).await
}

/// Spawn a driver that resumes the MOST RECENT saved session for
/// this agent in `cwd`. Equivalent to `claude --continue` / `opencode
/// run --continue` / `codex exec resume --last`.
pub async fn spawn_driver_continue_last(
    kind: AgentKind,
    cwd: &std::path::Path,
) -> Result<Box<dyn Driver>> {
    spawn_driver_inner(kind, cwd, ResumeMode::ContinueLast).await
}

#[derive(Copy, Clone)]
enum ResumeMode<'a> {
    None,
    ById(&'a str),
    ContinueLast,
}

/// Probe whether the local codex binary's `exec` subcommand still
/// accepts the stream-json flags cap-rs drives (`--input-format`).
/// Mainline codex-cli ≥0.139 dropped them in favour of `--json`, so we
/// fall back to the MCP driver for those. Resolves the binary the same
/// way cap-rs does: `$CODEX_BIN` override, else `codex` on PATH. Any
/// probe failure (binary missing, help errored) returns `false` so we
/// take the safer MCP path rather than spawn a driver that dies on
/// arg-parse.
async fn codex_supports_stream_json() -> bool {
    let bin = std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
    match tokio::process::Command::new(&bin)
        .arg("exec")
        .arg("--help")
        .output()
        .await
    {
        Ok(out) => {
            let help = String::from_utf8_lossy(&out.stdout);
            help.contains("--input-format")
        }
        Err(_) => false,
    }
}

/// Spawn the codex MCP driver (`codex mcp-server`). `why` is folded into
/// the error context so a downstream failure makes clear we arrived here
/// as a stream-json fallback, not a primary path.
async fn spawn_codex_mcp(
    cwd: &std::path::Path,
    why: &str,
) -> Result<cap_rs::driver::codex_mcp::CodexMcpDriver> {
    cap_rs::driver::codex_mcp::CodexMcpDriver::builder(cwd)
        .approval_policy("never")
        .spawn()
        .await
        .map_err(|e| anyhow!("cap codex spawn (stream-json: {why}, MCP: {e})"))
}

async fn spawn_driver_inner(
    kind: AgentKind,
    cwd: &std::path::Path,
    resume_mode: ResumeMode<'_>,
) -> Result<Box<dyn Driver>> {
    use cap_rs::driver::stream_json::ClaudeCodeDriver;

    fn apply_resume_mode(
        b: cap_rs::driver::stream_json::ClaudeCodeDriverBuilder,
        mode: ResumeMode<'_>,
    ) -> cap_rs::driver::stream_json::ClaudeCodeDriverBuilder {
        match mode {
            ResumeMode::None => b,
            ResumeMode::ById(rid) => b.resume(rid),
            ResumeMode::ContinueLast => b.continue_last(true),
        }
    }

    let driver: Box<dyn Driver> = match kind {
        AgentKind::Claudecode => {
            let mut b = ClaudeCodeDriver::builder(cwd).dangerously_skip_permissions(true);
            b = apply_resume_mode(b, resume_mode);
            Box::new(
                b.spawn()
                    .await
                    .map_err(|e| anyhow!("cap claudecode spawn: {e}"))?,
            )
        }
        AgentKind::Openclaude => {
            let mut b = ClaudeCodeDriver::builder(cwd)
                .bin("openclaude")
                .dangerously_skip_permissions(true);
            b = apply_resume_mode(b, resume_mode);
            Box::new(
                b.spawn()
                    .await
                    .map_err(|e| anyhow!("cap openclaude spawn: {e}"))?,
            )
        }
        AgentKind::Opencode => {
            // Try stream-json first (faster, lower overhead). Fall
            // back to ACP (`opencode acp`) if stream-json spawn fails
            // — older / non-fork opencode binaries may not support
            // `--output-format stream-json --persist`.
            let mut b = ClaudeCodeDriver::opencode_builder(cwd);
            b = apply_resume_mode(b, resume_mode);
            match b.spawn().await {
                Ok(d) => Box::new(d),
                Err(e) => {
                    tracing::info!(
                        target: "cap",
                        error = %e,
                        "opencode stream-json spawn failed, falling back to ACP"
                    );
                    Box::new(
                        cap_rs::driver::acp::AcpDriver::opencode(&cwd)
                            .await
                            .map_err(|e2| anyhow!("cap opencode spawn (stream-json: {e}, ACP: {e2})"))?,
                    )
                }
            }
        }
        AgentKind::Codex => {
            // Codex stream-json needs `codex exec --input-format
            // stream-json`. Mainline codex-cli (≥0.139) DROPPED those
            // flags — its `exec` only speaks `--json` (one-shot JSONL),
            // which cap-rs can't drive. Spawning stream-json against
            // such a binary "succeeds" (the process launches) then dies
            // on arg-parse, so a plain `match b.spawn()` never sees the
            // error and never falls back → empty replies. Preflight the
            // binary's `exec --help` for `--input-format` and route to
            // the MCP driver (`codex mcp-server`, supported by mainline)
            // when the stream-json flags are absent.
            if codex_supports_stream_json().await {
                let mut b =
                    ClaudeCodeDriver::codex_builder(cwd).dangerously_skip_permissions(true);
                b = apply_resume_mode(b, resume_mode);
                match b.spawn().await {
                    Ok(d) => Box::new(d),
                    Err(e) => {
                        tracing::info!(
                            target: "cap",
                            error = %e,
                            "codex stream-json spawn failed, falling back to MCP"
                        );
                        Box::new(spawn_codex_mcp(cwd, &e.to_string()).await?)
                    }
                }
            } else {
                tracing::info!(
                    target: "cap",
                    "codex binary lacks stream-json exec flags; using MCP driver"
                );
                Box::new(spawn_codex_mcp(cwd, "stream-json flags unsupported").await?)
            }
        }
        AgentKind::Qoder => {
            // Qoder uses the same stream-json protocol as Claude Code.
            // Binary: `qodercli` (or $QODER_BIN via .bin()).
            let mut b = ClaudeCodeDriver::builder(cwd)
                .bin("qodercli")
                .dangerously_skip_permissions(true);
            b = apply_resume_mode(b, resume_mode);
            Box::new(
                b.spawn()
                    .await
                    .map_err(|e| anyhow!("cap qoder spawn: {e}"))?,
            )
        }
    };
    Ok(driver)
}

/// Send a notification to the IM channel, if a target is configured.
/// Logs at warn! on send error so a transient channel issue doesn't kill
/// the whole turn.
pub fn push_notif(target: &NotifTarget, text: String) {
    let msg = OutboundMessage {
        target_id: target.target_id.clone(),
        is_group: target.is_group,
        text,
        reply_to: None,
        images: Vec::new(),
        files: Vec::new(),
        channel: Some(target.channel.clone()),
        account: None,
    };
    if let Err(e) = target.tx.send(msg) {
        tracing::warn!(target: "cap", err = %e, "cap notif send failed");
    }
}

/// Replace a dead driver in-place with a freshly spawned one of the same
/// `kind` (ResumeMode::None). Best-effort: shuts down the old driver,
/// spawns a replacement, and swaps it into `*driver` on success. Returns
/// `true` when the swap happened (caller may retry the prompt), `false`
/// when the respawn failed (caller surfaces the original error). Shared
/// shape with `live::respawn_driver`; kept separate to avoid coupling the
/// task-mode actor to the live module.
async fn respawn_cap_driver(
    kind: AgentKind,
    cwd: &std::path::Path,
    driver: &mut Box<dyn Driver>,
    session_id: &str,
    reason: &str,
) -> bool {
    match spawn_driver(kind, cwd).await {
        Ok(fresh) => {
            let _ = driver.shutdown().await;
            *driver = fresh;
            tracing::info!(
                target: "cap",
                session_id,
                agent = kind.as_str(),
                reason,
                "cap respawned driver after death; retrying prompt once"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                target: "cap",
                session_id,
                agent = kind.as_str(),
                reason,
                error = %e,
                "cap driver respawn failed; surfacing error"
            );
            false
        }
    }
}

async fn actor_loop(
    kind: AgentKind,
    cwd: std::path::PathBuf,
    mut driver: Box<dyn Driver>,
    mut rx: mpsc::Receiver<ToolCapRequest>,
    bus: broadcast::Sender<rsclaw_events::AgentEvent>,
    slot: Slot,
) {
    let agent_id = kind.as_str();
    let display = kind.display_name();
    while let Some(req) = rx.recv().await {
        match req {
            ToolCapRequest::Prompt {
                task,
                notif,
                inbox,
                reply,
            } => {
                let session_id = format!("cap-{agent_id}-{}", uuid::Uuid::new_v4());

                // Note: no "submitted" ack notification — the calling LLM's
                // text reply already tells the user the dispatch happened.
                // Pushing an extra IM message per cap call floods rate-limited
                // channels (wechat ret=-2 anti-spam) when several agents are
                // dispatched in one turn; completion notifications still go
                // through below.

                // 2. Tell the LLM the prompt is queued. From here on the
                //    LLM is free; the result is delivered async.
                let _ = reply.send(Ok(Submitted {
                    session_id: session_id.clone(),
                }));

                // 3-4. Send the prompt + run the turn, with ONE automatic
                //    retry on driver death (send failure or mid-turn
                //    exit/timeout). Mirrors the cap_live retry: the first
                //    `opencode acp` launch on a cold machine can die
                //    mid-turn; respawning a fresh driver and replaying the
                //    prompt hides that (and any transient CLI crash). A
                //    fresh respawn loses in-process context, which is fine
                //    for a driver that already died. `run_turn` is wrapped
                //    in a 5-minute timeout so a hung CLI can't tie up the
                //    actor indefinitely.
                const TURN_TIMEOUT: std::time::Duration =
                    std::time::Duration::from_secs(300);
                let mut reply_buf = String::new();
                let mut attempt = 0u8;
                let outcome = loop {
                    reply_buf.clear();
                    if let Err(e) = driver
                        .send(ClientFrame::Prompt {
                            content: vec![Content::text(task.clone())],
                        })
                        .await
                    {
                        if attempt == 0
                            && respawn_cap_driver(
                                kind,
                                &cwd,
                                &mut driver,
                                &session_id,
                                "send failed",
                            )
                            .await
                        {
                            attempt += 1;
                            continue;
                        }
                        break Err(anyhow!("cap send: {e}"));
                    }
                    let turn = match tokio::time::timeout(
                        TURN_TIMEOUT,
                        run_turn(
                            driver.as_mut(),
                            &bus,
                            &session_id,
                            agent_id,
                            notif.as_ref(),
                            &mut reply_buf,
                        ),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(anyhow!(
                            "cap {display}: turn timed out after {}s (driver hang?)",
                            TURN_TIMEOUT.as_secs()
                        )),
                    };
                    match turn {
                        Ok(()) => break Ok(()),
                        Err(e) => {
                            if attempt == 0
                                && respawn_cap_driver(
                                    kind,
                                    &cwd,
                                    &mut driver,
                                    &session_id,
                                    "exited mid-turn",
                                )
                                .await
                            {
                                attempt += 1;
                                continue;
                            }
                            break Err(e);
                        }
                    }
                };

                // 5. Completion / error notification + inbox reinjection.
                match &outcome {
                    Ok(()) => {
                        if let Some(n) = &notif {
                            let body = if reply_buf.is_empty() {
                                i18n::t_fmt(
                                    "acp_done_empty",
                                    n.lang,
                                    &[("status", "✅"), ("name", display)],
                                )
                            } else {
                                i18n::t_fmt(
                                    "acp_done_summary",
                                    n.lang,
                                    &[
                                        ("status", "✅"),
                                        ("name", display),
                                        ("count", "0"),
                                        ("summary", reply_buf.as_str()),
                                    ],
                                )
                            };
                            push_notif(n, body);
                        }
                        // inject_followup intentionally NOT called: the
                        // followup turn doubles every cap completion (one
                        // push_notif + one agent reply restating the same
                        // text), and 4 caps × 2 msgs hammers rate-limited
                        // channels. push_notif above is the canonical
                        // result delivery. If you need the agent to chain
                        // a follow-up action, re-enable inject_followup
                        // here and route the result back through the user
                        // session instead of `:cap-followup`.
                        let _ = &inbox;
                    }
                    Err(e) => {
                        if let Some(n) = &notif {
                            push_notif(
                                n,
                                i18n::t_fmt(
                                    "acp_error",
                                    n.lang,
                                    &[("name", display), ("error", &e.to_string())],
                                ),
                            );
                        }
                    }
                }

                if outcome.is_err() {
                    // Driver died mid-turn → respawn.
                    break;
                }
            }
        }
    }
    let _ = driver.shutdown().await;
    let mut g = slot.write().await;
    *g = None;
}

// `inject_followup` (cap → agent inbox re-injection) was dead code
// (#[allow(dead_code)], "intentionally NOT called" — see actor_loop). Removed
// during crate-split: it was the only constructor of agent::registry::AgentMessage
// in cap, and dropping it lets cap become a lower crate (agent depends on cap,
// not vice versa). If follow-up re-injection is revived, do it via a trait
// injected from the runtime (FollowupSink), not a direct AgentMessage build.

pub async fn run_turn(
    driver: &mut dyn Driver,
    bus: &broadcast::Sender<rsclaw_events::AgentEvent>,
    session_id: &str,
    agent_id: &str,
    notif: Option<&NotifTarget>,
    reply_buf: &mut String,
) -> Result<()> {
    loop {
        let Some(event) = driver.next_event().await else {
            return Err(anyhow!("cap driver exited mid-turn"));
        };
        if let AgentEvent::PermissionRequest {
            req_id,
            tool,
            risk_level,
            ..
        } = &event
        {
            let resp = permission::auto_approve(req_id, tool, *risk_level);
            if let Err(e) = driver.send(resp).await {
                return Err(anyhow!("cap permission send: {e}"));
            }
            continue;
        }
        if let AgentEvent::AskUser { ask_id, .. } = &event {
            let resp = ClientFrame::AskUserAnswer {
                ask_id: ask_id.clone(),
                value: serde_json::json!("cancelled"),
            };
            if let Err(e) = driver.send(resp).await {
                return Err(anyhow!("cap ask_user cancel: {e}"));
            }
            continue;
        }
        let mut sinks = bridge::Sinks {
            notif,
            agent_event: Some(bus),
            reply: Some(reply_buf),
            session_id,
            agent_id,
        };
        let done = bridge::dispatch(&event, &mut sinks);
        if done {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cap_rs::core::{RiskLevel, StopReason, TextChannel, Usage};
    use cap_rs::driver::DriverError;
    use std::collections::VecDeque;

    #[test]
    fn agent_kind_aliases() {
        assert_eq!(AgentKind::from_str("claude"), Some(AgentKind::Claudecode));
        assert_eq!(AgentKind::from_str("claudecode"), Some(AgentKind::Claudecode));
        assert_eq!(AgentKind::from_str("code"), None);
        assert_eq!(AgentKind::from_str("openclaude"), Some(AgentKind::Openclaude));
        assert_eq!(AgentKind::from_str("opencode"), Some(AgentKind::Opencode));
        assert_eq!(AgentKind::from_str("codex"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::from_str("qoder"), Some(AgentKind::Qoder));
        assert_eq!(AgentKind::from_str("unknown"), None);
    }

    /// Real-machine smoke: spawn each agent's CLI driver, fire one
    /// trivial prompt, assert a non-empty reply. Exercises real binary
    /// spawn (PATH resolution + protocol handshake) and, for
    /// opencode/codex, the stream-json → ACP/MCP fallback. Ignored by
    /// default (needs the CLIs installed + burns one LLM turn each); run
    /// explicitly:
    ///
    ///   cargo test -p rsclaw-cap cap_live_invoke_all -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "spawns real coding-agent CLIs; run manually"]
    async fn cap_live_invoke_all() {
        let cwd = std::env::temp_dir();
        let (bus, _rx) = broadcast::channel(64);
        let mut failures = Vec::new();
        for kind in [
            AgentKind::Claudecode,
            AgentKind::Openclaude,
            AgentKind::Opencode,
            AgentKind::Codex,
            AgentKind::Qoder,
        ] {
            let name = kind.as_str();
            let mut driver = match spawn_driver(kind, &cwd).await {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[skip] {name}: spawn failed: {e}");
                    continue;
                }
            };
            if let Err(e) = driver
                .send(ClientFrame::Prompt {
                    content: vec![Content::text(
                        "Reply with exactly the word OK and nothing else.".to_string(),
                    )],
                })
                .await
            {
                failures.push(format!("{name}: send: {e}"));
                let _ = driver.shutdown().await;
                continue;
            }
            let mut reply = String::new();
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                run_turn(driver.as_mut(), &bus, "smoke", name, None, &mut reply),
            )
            .await;
            let _ = driver.shutdown().await;
            match outcome {
                Ok(Ok(())) if reply.trim().is_empty() => {
                    failures.push(format!("{name}: empty reply"));
                    eprintln!("[FAIL] {name}: empty reply");
                }
                Ok(Ok(())) => eprintln!("[ok] {name}: {:?}", reply.trim()),
                Ok(Err(e)) => {
                    failures.push(format!("{name}: run_turn error: {e}"));
                    eprintln!("[FAIL] {name}: {e}");
                }
                Err(_) => {
                    failures.push(format!("{name}: timed out after 120s"));
                    eprintln!("[FAIL] {name}: timed out");
                }
            }
        }
        assert!(failures.is_empty(), "cap agent failures: {failures:?}");
    }

    /// Real-machine smoke for the opencode stream-json → ACP fallback
    /// target. `cap_live_invoke_all` exercises opencode's PRIMARY
    /// (stream-json) path; this drives the FALLBACK driver
    /// (`opencode acp`) directly — the branch `spawn_driver_inner` takes
    /// when stream-json spawn fails — so we know the fallback actually
    /// works on this machine instead of only being wired. Ignored by
    /// default; run explicitly:
    ///
    ///   cargo test -p rsclaw-cap opencode_acp_fallback -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "spawns real opencode ACP server; run manually"]
    async fn opencode_acp_fallback() {
        let cwd = std::env::temp_dir();
        let (bus, _rx) = broadcast::channel(64);
        let mut driver: Box<dyn Driver> = match cap_rs::driver::acp::AcpDriver::opencode(&cwd).await
        {
            Ok(d) => Box::new(d),
            Err(e) => panic!("opencode ACP spawn failed: {e}"),
        };
        driver
            .send(ClientFrame::Prompt {
                content: vec![Content::text(
                    "Reply with exactly the word OK and nothing else.".to_string(),
                )],
            })
            .await
            .expect("opencode ACP send");
        let mut reply = String::new();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            run_turn(driver.as_mut(), &bus, "smoke", "opencode", None, &mut reply),
        )
        .await;
        let _ = driver.shutdown().await;
        match outcome {
            Ok(Ok(())) => {
                assert!(!reply.trim().is_empty(), "opencode ACP: empty reply");
                eprintln!("[ok] opencode (ACP fallback): {:?}", reply.trim());
            }
            Ok(Err(e)) => panic!("opencode ACP run_turn error: {e}"),
            Err(_) => panic!("opencode ACP timed out after 120s"),
        }
    }

    struct FakeDriver {
        events: VecDeque<AgentEvent>,
    }

    impl FakeDriver {
        fn new(events: Vec<AgentEvent>) -> Self {
            Self {
                events: events.into(),
            }
        }
    }

    #[async_trait]
    impl Driver for FakeDriver {
        async fn send(&mut self, _frame: ClientFrame) -> Result<(), DriverError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<AgentEvent> {
            self.events.pop_front()
        }
        async fn shutdown(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    fn done() -> AgentEvent {
        AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn text(t: &str) -> AgentEvent {
        AgentEvent::TextChunk {
            msg_id: "m".into(),
            text: t.into(),
            channel: TextChannel::Assistant,
        }
    }

    #[tokio::test]
    async fn run_turn_collects_text_until_done() {
        let mut driver = FakeDriver::new(vec![text("Hello "), text("world"), done()]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        run_turn(&mut driver, &bus, "sess", "claudecode", None, &mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "Hello world");
    }

    #[tokio::test]
    async fn run_turn_auto_approves_permission() {
        let mut driver = FakeDriver::new(vec![
            AgentEvent::PermissionRequest {
                req_id: "p1".into(),
                tool: "shell".into(),
                intent: serde_json::json!({}),
                scope: cap_rs::core::PermissionScope::Execute,
                risk_level: RiskLevel::Low,
            },
            text("ok"),
            done(),
        ]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        run_turn(&mut driver, &bus, "sess", "claudecode", None, &mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "ok");
    }

    #[tokio::test]
    async fn run_turn_cancels_ask_user() {
        use cap_rs::core::AskKind;
        let mut driver = FakeDriver::new(vec![
            AgentEvent::AskUser {
                ask_id: "q1".into(),
                prompt: "Continue?".into(),
                ask_kind: AskKind::YesNo,
                options: vec![],
                timeout_seconds: None,
            },
            text("ok"),
            done(),
        ]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        run_turn(&mut driver, &bus, "sess", "claudecode", None, &mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "ok");
    }

    #[tokio::test]
    async fn run_turn_surfaces_mid_turn_exit() {
        let mut driver = FakeDriver::new(vec![text("partial")]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        let err = run_turn(&mut driver, &bus, "sess", "claudecode", None, &mut reply)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exited mid-turn"));
    }

    #[tokio::test]
    async fn run_turn_pushes_tool_call_progress_to_notif() {
        let mut driver = FakeDriver::new(vec![
            AgentEvent::ToolCallStart {
                call_id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/etc/hosts"}),
            },
            text("done reading"),
            done(),
        ]);
        let (bus, _rx) = broadcast::channel(8);
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let notif = NotifTarget {
            tx: notif_tx,
            target_id: "user@feishu".into(),
            is_group: false,
            channel: "feishu".into(),
            lang: "en",
        };
        let mut reply = String::new();
        run_turn(
            &mut driver,
            &bus,
            "sess",
            "claudecode",
            Some(&notif),
            &mut reply,
        )
        .await
        .unwrap();
        // Bridge should have pushed at least one OutboundMessage for the
        // ToolCallStart.
        let m = notif_rx.try_recv().expect("expected tool-call notif");
        assert!(
            m.text.contains("read_file"),
            "got notif: {:?}",
            m.text
        );
    }
}
