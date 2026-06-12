//! Fast preparse — local slash-command handling that bypasses the agent queue.
//!
//! Functions here handle commands like `/ls`, `/status`, `/version`, `/btw`
//! without going through the LLM agent loop.

use futures::StreamExt as _;
use tracing::warn;

use crate::{
    agent::{
        LiveStatus,
        runtime::{AgentRuntime, PluginOverride},
    },
    channel::OutboundMessage,
    config::runtime::RuntimeConfig,
    config::schema::DmScope,
    gateway::session::{MessageKind, SessionKeyParams, derive_session_key},
    provider::{
        AgentEndpoint, LlmRequest, Message, MessageContent, Role, StreamEvent,
        failover::FailoverManager, registry::ProviderRegistry,
    },
};

/// Where a `/...` command text came from. `/watch` uses this to suppress
/// dedup-hit replies fired by /loop's cron-replayed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparseOrigin {
    User,
    Cron,
}

/// Parsed form of a `/plugin ...` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginCommand {
    /// `/plugin` — list every plugin override in this session.
    Status,
    /// `/plugin <name>` — show one plugin's current override.
    Info { plugin: String },
    /// `/plugin <name> off|on|all|<comma-tools>` — set session override.
    SetState { plugin: String, action: PluginAction },
    /// `/plugin reset` — clear every override for this session.
    Reset,
    /// `/plugin pin <plugin>__<tool>` (or legacy `<plugin>.<tool>`) —
    /// add this tool to the session's pin set so it shows up in
    /// `dynamic_prefix.user_tools` as a real ToolDef, on top of
    /// headline defaults. Writes `PluginOverride.pin`.
    Pin { plugin: String, tool: String },
    /// `/plugin unpin <plugin>__<tool>` — remove from `user_tools`
    /// for this session even if the plugin author marked it
    /// `headline: true`. Writes `PluginOverride.unpin`.
    Unpin { plugin: String, tool: String },
    /// `/plugin headlines <plugin>` — show which tools in `<plugin>`
    /// are auto-promoted to `user_tools` by default (i.e. carry
    /// `headline: true` in the plugin's manifest).
    Headlines { plugin: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginAction {
    /// Hide the plugin entirely (catalog + tools).
    Off,
    /// Inject every tool this plugin exposes (subject to MAX_INJECT_TOOLS).
    All,
    /// Clear inject set — back to default (catalog only, model must use
    /// plugin_search + plugin_invoke).
    Default,
    /// Inject the listed tools as `<plugin>.<tool>` ToolDefs.
    Inject(Vec<String>),
}

/// Parse a `/plugin ...` line. `None` on malformed input or explicit help
/// request — the caller falls through to `plugin_help_text()`.
///
/// Recognized aliases (so muscle memory works):
/// - `/plugin`, `/plugin list`, `/plugin ls` → Status (list overrides)
/// - `/plugin info <name>`, `/plugin show <name>`, `/plugin <name>` → Info
/// - `/plugin info` (no name), `/plugin help`, `/plugin -h`, `/plugin --help`
///   → None (caller shows help)
///
/// Tradeoff: this reserves the words `list`/`ls`/`info`/`show`/`help`/`reset`
/// as command keywords, so a plugin literally named one of those can't be
/// inspected via `/plugin <name>` — use `/plugin info <name>` instead.
pub(crate) fn parse_plugin_command(line: &str) -> Option<PluginCommand> {
    let line = line.trim();
    if line == "/plugin" {
        return Some(PluginCommand::Status);
    }
    let rest = line.strip_prefix("/plugin ")?.trim();
    if rest.is_empty() {
        return Some(PluginCommand::Status);
    }
    // Explicit help — return None so caller prints plugin_help_text.
    if matches!(rest, "help" | "-h" | "--help" | "?") {
        return None;
    }
    // List aliases — same as bare /plugin.
    if matches!(rest, "list" | "ls") {
        return Some(PluginCommand::Status);
    }
    if rest == "reset" {
        return Some(PluginCommand::Reset);
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let head = parts.next()?.trim().to_owned();
    if head.is_empty() {
        return None;
    }
    let tail = parts.next().map(str::trim).filter(|s| !s.is_empty());
    // `info <name>` / `show <name>` → explicit Info form.
    if matches!(head.as_str(), "info" | "show") {
        return match tail {
            Some(name) => Some(PluginCommand::Info {
                plugin: name.to_owned(),
            }),
            None => None, // /plugin info (no name) → help text
        };
    }
    // `pin <plugin>__<tool>` / `unpin <plugin>__<tool>` — accepts the
    // canonical `__` form, plus legacy `.` / `/` so muscle memory works.
    if matches!(head.as_str(), "pin" | "unpin") {
        let Some(qualified) = tail else {
            return None; // `/plugin pin` with no arg → help text
        };
        let Some((plugin, tool)) = crate::agent::runtime::parse_qualified_tool(qualified) else {
            return None;
        };
        return Some(match head.as_str() {
            "pin" => PluginCommand::Pin { plugin, tool },
            _ => PluginCommand::Unpin { plugin, tool },
        });
    }
    // `headlines <plugin>` — list which tools are auto-promoted.
    if head == "headlines" {
        return tail.map(|n| PluginCommand::Headlines {
            plugin: n.to_owned(),
        });
    }
    // First token is the plugin name. No second token → Info shorthand.
    let plugin = head;
    let Some(action_raw) = tail else {
        return Some(PluginCommand::Info { plugin });
    };
    let action = match action_raw {
        "off" => PluginAction::Off,
        "on" | "default" => PluginAction::Default,
        "all" => PluginAction::All,
        other => {
            let tools: Vec<String> = other
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            if tools.is_empty() {
                return None;
            }
            PluginAction::Inject(tools)
        }
    };
    Some(PluginCommand::SetState { plugin, action })
}

/// `/plugin headlines <plugin>` dispatcher — reads the plugin's
/// manifest from disk and lists tools where `headline: true`. Goes
/// straight to `~/.rsclaw/plugins/<plugin>/plugin.json5` rather than
/// through the running `AgentRuntime` because preparse handlers don't
/// hold a reference to the plugin registry and adding one would mean
/// threading an `Arc<RuntimePluginSnapshot>` through every call site
/// of `bypass_run_command` just for this read-only inspection
/// command. The on-disk manifest is also the source of truth — if it
/// disagrees with what the runtime loaded, the on-disk view is what
/// the next process restart will pick up, which is what an operator
/// inspecting headlines actually wants to see.
fn render_headlines_for_plugin(plugin: &str) -> String {
    let plugin_dir = crate::config::loader::base_dir()
        .join("plugins")
        .join(plugin);
    if !plugin_dir.exists() {
        return format!("plugin `{plugin}` not installed (no dir at ~/.rsclaw/plugins/{plugin})");
    }
    let manifest = match crate::plugin::manifest::load_manifest(&plugin_dir) {
        Ok(m) => m,
        Err(e) => return format!("failed to load `{plugin}` manifest: {e}"),
    };
    let headlines: Vec<&str> = manifest
        .tools
        .iter()
        .filter(|t| t.headline)
        .map(|t| t.name.as_str())
        .collect();
    if headlines.is_empty() {
        return format!(
            "{plugin}: no headline-marked tools (manifest defaults all {} tools to long-tail; \
             promote a tool by adding `headline: true` to its entry in plugin.json5, \
             or override per-agent with `model.plugin_tools`)",
            manifest.tools.len()
        );
    }
    let mut lines = vec![format!(
        "{plugin}: {} headline tool(s) (auto-promoted to user_tools by default; \
         {} long-tail tool(s) accessible via plugin_invoke):",
        headlines.len(),
        manifest.tools.len() - headlines.len()
    )];
    for name in headlines {
        lines.push(format!("  {plugin}__{name}"));
    }
    lines.join("\n")
}

/// Help text for `/plugin`.
pub(crate) fn plugin_help_text() -> String {
    "/plugin [list|ls]               — list session plugin overrides\n\
     /plugin info <name>             — show one plugin's state\n\
     /plugin <name>                  — shorthand for `info <name>`\n\
     /plugin <name> off              — hide plugin in this session\n\
     /plugin <name> on               — back to default (headline only)\n\
     /plugin <name> all              — inject ALL plugin tools (cap by user_tools_cap)\n\
     /plugin <name> t1,t2,t3         — replace defaults with this exact set\n\
     /plugin pin <plugin>__<tool>    — add this tool to user_tools (on top of headlines)\n\
     /plugin unpin <plugin>__<tool>  — remove this tool from user_tools (even if headline)\n\
     /plugin headlines <name>        — list which tools are auto-promoted by default\n\
     /plugin reset                   — clear every session override\n\
     /plugin help                    — this help\n\n\
     Session overrides upgrade plugin exposure from \"catalog only\" \
     (model must use plugin_search + plugin_invoke) to an \"## Active \
     Plugin Tools\" block injected into per-session system text, with \
     full input_schema. Schema lives in user_system (not in tools[]), \
     so KV cache stays shared across sessions. Useful for small fleet \
     models. Resets when the session restarts."
        .to_owned()
}

/// Derive the session_key the agent runtime will use to look up overrides.
/// v1 hardcodes dmScope=PerChannelPeer (the default and most common case);
/// users on other dmScope settings will not see /plugin overrides apply
/// until v2 surfaces dmScope to preparse.
fn preparse_session_key(
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    account: Option<&str>,
) -> String {
    derive_session_key(&SessionKeyParams {
        agent_id: handle.id.clone(),
        channel: channel.to_owned(),
        peer_id: peer_id.to_owned(),
        kind: MessageKind::DirectMessage { account_id: account.map(str::to_owned) },
        dm_scope: DmScope::PerChannelPeer,
    })
}

/// Handle certain fast preparse commands locally — without going through the
/// agent queue. Returns `Some(reply_text)` for commands that can be answered
/// immediately, `None` otherwise. This avoids blocking on the agent's
/// sequential LLM loop for simple commands like /ls, /status.
///
/// `channel` (e.g. "telegram", "wechat") and `peer_id` are passed through so
/// commands that need to schedule deliveries back to the originating
/// channel/peer (e.g. `/loop`) can populate a `CronDelivery` correctly.
/// `origin` tells preparse whether the call came from a real user or from
/// cron's replay of `/loop`-scheduled text.
pub(crate) async fn try_preparse_locally(
    text: &str,
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    origin: PreparseOrigin,
) -> Option<OutboundMessage> {
    try_preparse_locally_with_account(text, handle, channel, peer_id, None, origin).await
}

/// Account-aware variant. Channels that need multi-account routing
/// (e.g. feishu with several `appId`s) call this with the account name
/// so deliveries to long-running registrations (notably `/watch`) can
/// route back through the SAME app that received the inbound message.
/// Open IDs are per-app in feishu — sending via the wrong app fails
/// with 99992361 "open_id cross app".
pub(crate) async fn try_preparse_locally_with_account(
    text: &str,
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    account: Option<&str>,
    origin: PreparseOrigin,
) -> Option<OutboundMessage> {
    use std::sync::atomic::Ordering;
    let t = text.trim();
    let lower = t.to_lowercase();

    // Helper: text-only reply (target_id/is_group filled in by caller).
    let txt = |s: String| OutboundMessage {
        target_id: String::new(),
        is_group: false,
        text: s,
        reply_to: None,
        images: vec![],
        files: vec![],
        channel: None,
        account: None,
    };

    // Workspace resolver (shared by /ls, /cat, shell cmds).
    let workspace = || {
        let base = crate::config::loader::base_dir();
        handle
            .config
            .workspace
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| base.join("workspace"))
    };

    // /help · /?
    // /help was previously in `is_fast_preparse` whitelist but had no
    // match arm here → fell through to the agent runtime, which on
    // channels with a 10s fast-path timeout (LINE) silently dropped
    // the reply. Make it a local fast response listing all preparse
    // slash commands, so it lands within milliseconds on every channel.
    if lower == "/help" || lower == "/?" {
        return Some(txt(help_text(crate::i18n::default_lang())));
    }
    // /version
    if lower == "/version" {
        return Some(txt(format!(
            "rsclaw v{}",
            option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
        )));
    }
    // /health
    if lower == "/health" {
        let secs = handle.started_at.elapsed().as_secs();
        let uptime = if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        };
        return Some(txt(format!("OK · up {uptime}")));
    }
    // /uptime
    if lower == "/uptime" {
        let secs = handle.started_at.elapsed().as_secs();
        let s = if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        };
        return Some(txt(s));
    }
    // Helper: session_key for THIS preparse call so /abort, /clear, /new
    // only touch the originating session — not every session the agent has
    // ever seen. Cross-session blast was the cause of a real incident:
    // user types `/new` in feishu, the loop-over-all-values set every
    // wechat session's abort_flag to true too, killing an in-progress
    // wechat cap_live mid-iteration as `[aborted]`.
    let this_session_key = derive_session_key(&SessionKeyParams {
        agent_id: handle.id.clone(),
        channel: channel.to_owned(),
        peer_id: peer_id.to_owned(),
        kind: MessageKind::DirectMessage {
            account_id: account.map(|s| s.to_owned()),
        },
        // Use the default scope; IM dm sessions overwhelmingly run at
        // PerChannelPeer (the default). If a deployment ever sets a
        // different scope, /abort would target a slightly different key —
        // worst case the abort is a no-op (which is the correct fallback,
        // since the alternative — blasting every session — is what we're
        // fixing here).
        dm_scope: DmScope::PerChannelPeer,
    });

    // /abort — set abort flag for THIS session only.
    if lower == "/abort" {
        let flags = handle
            .abort_flags
            .read()
            .expect("abort_flags lock poisoned");
        let hit = flags.get(&this_session_key).map(|f| {
            f.store(true, Ordering::SeqCst);
        });
        return Some(txt(if hit.is_some() {
            "abort signal sent".to_owned()
        } else {
            "nothing to abort".to_owned()
        }));
    }
    // /clear — abort current session's running turn + signal session clear
    if lower == "/clear" {
        // 1. Abort the running turn FOR THIS SESSION only.
        let flags = handle
            .abort_flags
            .read()
            .expect("abort_flags lock poisoned");
        if let Some(f) = flags.get(&this_session_key) {
            f.store(true, Ordering::SeqCst);
        }
        drop(flags);
        // 2. Signal runtime to clear sessions at next opportunity
        handle.clear_signal.store(true, Ordering::SeqCst);
        return Some(txt(crate::i18n::t(
            "session_cleared",
            crate::i18n::default_lang(),
        )
        .to_owned()));
    }
    // /new — start a fresh conversation (new generation, no summary)
    if lower == "/new" {
        let flags = handle
            .abort_flags
            .read()
            .expect("abort_flags lock poisoned");
        if let Some(f) = flags.get(&this_session_key) {
            f.store(true, Ordering::SeqCst);
        }
        drop(flags);
        handle.new_session_signal.store(true, Ordering::SeqCst);
        return Some(txt(crate::i18n::t(
            "session_new",
            crate::i18n::default_lang(),
        )
        .to_owned()));
    }
    // /cap <agent> — open a cap_live session and bind it sticky to this
    // IM session so subsequent plain-text messages route directly to the
    // driver, skipping the main LLM. `/cap-exit` releases the binding.
    //
    // Why bypass the LLM here: phase 1 already exposes `cap_live` as a
    // tool the LLM can call, but going through the LLM costs an extra
    // round-trip (LLM reads, decides to call cap_live, then forwards).
    // Sticky direct mode is for "I want to talk to claudecode directly
    // for the next several turns" without the LLM in the middle.
    if lower == "/cap" || lower == "/cap -h" || lower == "/cap --help" || lower == "/cap help" {
        return Some(txt(crate::i18n::t("cap_help", crate::i18n::default_lang())));
    }
    if lower.starts_with("/cap ") {
        // Parse from `t` (case-preserved) so path arguments like
        // ~/Dev/MyProj survive — `lower` would have lowercased the
        // directory name and broken canonicalize() on case-sensitive
        // filesystems (and made paths confusing on macOS).
        let rest = t.get("/cap ".len()..).unwrap_or("").trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let agent_str = parts.next().unwrap_or("").trim();
        let path_arg = parts.next().unwrap_or("").trim();
        let lang = crate::i18n::default_lang();
        let Some(kind) = crate::cap::AgentKind::from_str(&agent_str.to_lowercase()) else {
            return Some(txt(crate::i18n::t_fmt(
                "cap_unknown_agent",
                lang,
                &[("agent", agent_str)],
            )));
        };
        let Some(manager) = crate::cap::GLOBAL_CAP_LIVE.get() else {
            return Some(txt(crate::i18n::t("cap_not_initialised", lang)));
        };
        let cwd = match resolve_cap_workspace(path_arg, workspace()) {
            Ok(p) => p,
            Err(reason) => {
                return Some(txt(crate::i18n::t_fmt(
                    "cap_bad_workspace",
                    lang,
                    &[("path", path_arg), ("reason", &reason)],
                )));
            }
        };
        // Write USER.md + AGENTS.md BEFORE spawning the driver process.
        // Coding agents (claudecode/codex/opencode/openclaude) all scan
        // the cwd for AGENTS.md or CLAUDE.md on startup and feed it
        // into their system prompt — if we write after spawn, the agent
        // already loaded an empty/stale view and won't reread on its
        // own. Best-effort: any write failure is logged inside the
        // helper and does not abort the bind.
        let _ = crate::cap::identity::write_identity_files(&cwd).await;
        match manager.open_session(kind, cwd).await {
            Ok(sid) => {
                manager
                    .bind_sticky(this_session_key.clone(), sid.clone(), kind)
                    .await;
                tracing::info!(
                    target: "cap",
                    session_id = %sid,
                    agent = kind.as_str(),
                    im_session_key = %this_session_key,
                    "cap_live sticky bind"
                );
                // Native session id is captured asynchronously by the
                // actor loop (claudecode's SessionStart hook can delay
                // the Ready event by 10-30s — we don't block the bind
                // reply on it). Spawn a follow-up task that waits up
                // to 30s for the id and then pushes a second IM
                // message with the resume hint, so the user has the
                // `/cap-resume` command ready to copy without having
                // to run `/status`.
                spawn_resume_hint_followup(
                    manager.clone(),
                    sid.clone(),
                    kind,
                    peer_id.to_owned(),
                    channel.to_owned(),
                    account.map(|s| s.to_owned()),
                    lang,
                );
                let reply = crate::i18n::t_fmt(
                    "cap_bound",
                    lang,
                    &[
                        ("agent", kind.display_name()),
                        ("sid", &sid[..8.min(sid.len())]),
                    ],
                );
                return Some(txt(reply));
            }
            Err(e) => {
                let err = e.to_string();
                let is_binary_missing = err.contains("binary not found on PATH");
                let hint = if is_binary_missing {
                    match kind {
                        crate::cap::AgentKind::Opencode => crate::i18n::t_fmt(
                            "cap_install_hint_go",
                            lang,
                            &[
                                ("agent", kind.display_name()),
                                ("cmd", kind.as_str()),
                            ],
                        ),
                        _ => crate::i18n::t_fmt(
                            "cap_install_hint_npm",
                            lang,
                            &[
                                ("agent", kind.display_name()),
                                ("cmd", kind.as_str()),
                            ],
                        ),
                    }
                } else {
                    crate::i18n::t_fmt("cap_open_failed", lang, &[("err", &err)])
                };
                return Some(txt(hint));
            }
        }
    }
    // /cap-resume <agent> <session_id> — like /cap but resumes an
    // existing on-disk session by the agent's NATIVE session id
    // instead of starting fresh. Format mirrors `claude --resume
    // <uuid>` / `codex exec resume <id>` / `opencode --session <id>`.
    //
    // Example:
    //   /cap-resume claudecode 00000000-0000-0000-0000-deadbeefcafe
    //
    // The id format is whatever the agent itself uses; we just pass
    // it through. If the id doesn't match a stored session the agent
    // CLI errors at spawn — we surface that as a cap_open_failed
    // reply.
    if lower.starts_with("/cap-resume ") || lower == "/cap-resume" {
        let lang = crate::i18n::default_lang();
        // Parse "<agent> [session_id]" from the original (non-lowered)
        // text because session ids are case-sensitive on opencode.
        // Empty session_id → "continue last" mode (CLI-native flag).
        let rest = t.get("/cap-resume ".len()..).unwrap_or("").trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let agent_str = parts.next().unwrap_or("").trim();
        let session_id = parts.next().unwrap_or("").trim();
        if agent_str.is_empty() {
            return Some(txt(crate::i18n::t("cap_resume_help", lang)));
        }
        let Some(kind) = crate::cap::AgentKind::from_str(&agent_str.to_lowercase()) else {
            return Some(txt(crate::i18n::t_fmt(
                "cap_unknown_agent",
                lang,
                &[("agent", agent_str)],
            )));
        };
        let Some(manager) = crate::cap::GLOBAL_CAP_LIVE.get() else {
            return Some(txt(crate::i18n::t("cap_not_initialised", lang)));
        };
        let cwd = workspace();
        let spawn_result = if session_id.is_empty() {
            manager.open_session_continue_last(kind, cwd).await
        } else {
            manager.open_session_resume(kind, cwd, session_id.to_owned()).await
        };
        match spawn_result {
            Ok(sid) => {
                manager
                    .bind_sticky(this_session_key.clone(), sid.clone(), kind)
                    .await;
                tracing::info!(
                    target: "cap",
                    session_id = %sid,
                    agent = kind.as_str(),
                    resume_id = %session_id,
                    continue_last = session_id.is_empty(),
                    im_session_key = %this_session_key,
                    "cap_live sticky bind via /cap-resume"
                );
                let label = if session_id.is_empty() {
                    "(latest)"
                } else {
                    session_id
                };
                return Some(txt(crate::i18n::t_fmt(
                    "cap_resumed",
                    lang,
                    &[("agent", kind.display_name()), ("session_id", label)],
                )));
            }
            Err(e) => {
                let err = e.to_string();
                let is_binary_missing = err.contains("binary not found on PATH");
                let hint = if is_binary_missing {
                    match kind {
                        crate::cap::AgentKind::Opencode => crate::i18n::t_fmt(
                            "cap_install_hint_go",
                            lang,
                            &[
                                ("agent", kind.display_name()),
                                ("cmd", kind.as_str()),
                            ],
                        ),
                        _ => crate::i18n::t_fmt(
                            "cap_install_hint_npm",
                            lang,
                            &[
                                ("agent", kind.display_name()),
                                ("cmd", kind.as_str()),
                            ],
                        ),
                    }
                } else {
                    crate::i18n::t_fmt("cap_open_failed", lang, &[("err", &err)])
                };
                return Some(txt(hint));
            }
        }
    }
    // /cap-exit — release any sticky binding on this IM session.
    if lower == "/cap-exit" {
        let lang = crate::i18n::default_lang();
        let Some(manager) = crate::cap::GLOBAL_CAP_LIVE.get() else {
            return Some(txt(crate::i18n::t("cap_not_initialised", lang)));
        };
        let Some((sid, kind)) = manager.unbind_sticky(&this_session_key).await else {
            return Some(txt(crate::i18n::t("cap_no_active", lang)));
        };
        // Capture the native session_id BEFORE tearing the actor down
        // — once `end_session` drops the LiveSessionHandle, that
        // information is gone. By now the actor has had plenty of
        // time to drain Ready (any /cap-exit follows actual usage),
        // so this should usually be Some(...).
        let native_sid = manager.get_agent_session_id(&sid).await;
        let _ = manager.end_session(&sid).await;
        tracing::info!(
            target: "cap",
            session_id = %sid,
            agent = kind.as_str(),
            agent_session_id = ?native_sid,
            im_session_key = %this_session_key,
            "cap_live sticky unbind + end"
        );
        // Build the close reply, append the resume hint if we have
        // the native id so the user has the `/cap-resume` command
        // ready to copy without having to run `/status`.
        let closed = crate::i18n::t_fmt(
            "cap_session_closed",
            lang,
            &[("agent", kind.display_name())],
        );
        let body = if let Some(nsid) = native_sid {
            let hint = crate::i18n::t_fmt(
                "cap_resume_hint_after_exit",
                lang,
                &[("agent", kind.as_str()), ("session_id", &nsid)],
            );
            format!("{closed}\n\n{hint}")
        } else {
            closed
        };
        return Some(txt(body));
    }
    // /status
    if lower == "/status" {
        return Some(txt(handle.format_status()));
    }
    // /ls [path] — list workspace directory
    if lower == "/ls" || lower.starts_with("/ls ") {
        let path_arg = t.get(3..).unwrap_or("").trim();
        let ws = workspace();
        let target = if path_arg.is_empty() {
            ws
        } else {
            let p = std::path::PathBuf::from(path_arg);
            if p.is_absolute() {
                p
            } else {
                ws.join(path_arg)
            }
        };
        let out = tokio::process::Command::new("ls")
            .current_dir(&target)
            .output()
            .await
            .ok()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        return Some(txt(if stdout.trim().is_empty() {
            "(empty directory)".to_owned()
        } else {
            stdout
        }));
    }
    // /cat <path> — read file from workspace
    if lower.starts_with("/cat ") {
        let path_arg = t.get(5..).unwrap_or("").trim();
        let ws = workspace();
        let target = {
            let p = std::path::PathBuf::from(path_arg);
            if p.is_absolute() {
                p
            } else {
                ws.join(path_arg)
            }
        };
        let content = tokio::fs::read_to_string(&target)
            .await
            .unwrap_or_else(|e| format!("error reading {}: {e}", target.display()));
        return Some(txt(content));
    }
    // /ss — desktop screenshot
    if lower == "/ss" || lower == "/screenshot" {
        let tmp_path = std::env::temp_dir().join("rsclaw_screen.png");
        let tmp_s = tmp_path.to_string_lossy().to_string();
        let ok = if cfg!(target_os = "macos") {
            tokio::process::Command::new("screencapture")
                .args(["-x", &tmp_s])
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false)
        } else if cfg!(target_os = "windows") {
            let script = format!(
                r#"Add-Type -AssemblyName System.Windows.Forms,System.Drawing
$b=New-Object System.Drawing.Bitmap([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width,[System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height)
$g=[System.Drawing.Graphics]::FromImage($b)
$g.CopyFromScreen(0,0,0,0,$b.Size)
$b.Save('{tmp_s}')
$g.Dispose();$b.Dispose()"#
            );
            {
                #[cfg(target_os = "windows")]
                let mut cmd = {
                    use std::os::windows::process::CommandExt;
                    let mut c = tokio::process::Command::new("powershell");
                    c.creation_flags(0x08000000);
                    c
                };
                #[cfg(not(target_os = "windows"))]
                let mut cmd = tokio::process::Command::new("powershell");
                cmd.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
                    .status()
                    .await
                    .map(|s| s.success())
                    .unwrap_or(false)
            }
        } else {
            tokio::process::Command::new("scrot")
                .arg(&tmp_s)
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if ok {
            if let Ok(bytes) = tokio::fs::read(&tmp_path).await {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                return Some(OutboundMessage {
                    target_id: String::new(),
                    is_group: false,
                    text: String::new(),
                    reply_to: None,
                    images: vec![format!("data:image/png;base64,{b64}")],
                    files: vec![],
                    channel: None,
                    account: None,
                });
            }
        }
        return Some(txt(crate::i18n::t(
            "screenshot_failed",
            crate::i18n::default_lang(),
        )
        .to_owned()));
    }
    // /webshot <url> — headless-Chrome screenshot of a web page. Distinct
    // from /ss (desktop) because the LLM's `web_browser action=screenshot`
    // path requires a navigated page and otherwise captures a blank dark
    // chrome new-tab — when the user wanted a website screenshot, that's
    // a blank image. /webshot is the explicit "screenshot a URL" command.
    if lower.starts_with("/webshot ") || lower == "/webshot" {
        let arg = t.get(9..).unwrap_or("").trim();
        if arg.is_empty() {
            return Some(txt(
                "/webshot <url> — screenshot a web page. Example: /webshot https://example.com"
                    .to_owned(),
            ));
        }
        let url = if arg.starts_with("http://") || arg.starts_with("https://") {
            arg.to_owned()
        } else {
            format!("https://{arg}")
        };
        let tmp_path = std::env::temp_dir().join("rsclaw_webshot.png");
        // Find a usable chromium binary. Order: $CHROME, common macOS
        // app bundle, common linux/bin names, common windows path.
        let chrome = std::env::var("CHROME")
            .ok()
            .filter(|p| std::path::Path::new(p).exists())
            .or_else(|| {
                for cand in [
                    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                    "/Applications/Chromium.app/Contents/MacOS/Chromium",
                    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
                    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
                    "/usr/bin/google-chrome",
                    "/usr/bin/chromium",
                    "/usr/bin/chromium-browser",
                    "/snap/bin/chromium",
                    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
                ] {
                    if std::path::Path::new(cand).exists() {
                        return Some(cand.to_owned());
                    }
                }
                None
            });
        let Some(chrome) = chrome else {
            return Some(txt(
                "/webshot: no Chrome / Chromium found. Install Google Chrome or set $CHROME=/path/to/chrome.".to_owned(),
            ));
        };
        let _ = tokio::fs::remove_file(&tmp_path).await;
        let ok = tokio::process::Command::new(&chrome)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--hide-scrollbars",
                "--window-size=1280,800",
                &format!("--screenshot={}", tmp_path.display()),
                &url,
            ])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && tmp_path.exists() {
            if let Ok(bytes) = tokio::fs::read(&tmp_path).await {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                return Some(OutboundMessage {
                    target_id: String::new(),
                    is_group: false,
                    text: format!("[webshot] {url}"),
                    reply_to: None,
                    images: vec![format!("data:image/png;base64,{b64}")],
                    files: vec![],
                    channel: None,
                    account: None,
                });
            }
        }
        return Some(txt(crate::i18n::t_fmt(
            "webshot_failed",
            crate::i18n::default_lang(),
            &[("url", &url)],
        )));
    }
    // /skill list — list installed skills (system + agent workspace)
    if lower == "/skill list" {
        let base = crate::config::loader::base_dir();
        let global_dir = base.join("skills");
        let ws_dir = workspace().join("skills");

        let scan = |dir: &std::path::Path| -> Vec<String> {
            let mut names = Vec::new();
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() && p.join("SKILL.md").exists() {
                        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                            names.push(name.to_owned());
                        }
                    }
                }
            }
            names.sort();
            names
        };

        let global = scan(&global_dir);
        let agent = scan(&ws_dir);

        let mut lines = Vec::new();
        lines.push(format!("System skills ({}):", global.len()));
        if global.is_empty() {
            lines.push("  (none)".to_owned());
        } else {
            for s in &global {
                lines.push(format!("  {s}"));
            }
        }
        lines.push(format!("Agent skills ({}):", agent.len()));
        if agent.is_empty() {
            lines.push("  (none)".to_owned());
        } else {
            for s in &agent {
                lines.push(format!("  {s}"));
            }
        }
        return Some(txt(lines.join("\n")));
    }
    // /cron — list cron jobs (reads from disk).  Routes through the same
    // formatter the agent runtime uses for `__CRON_LIST__` so feishu/wechat/etc.
    // see the same output format as the desktop console.
    if lower == "/cron" || lower == "/cron list" {
        let jobs_path = crate::config::loader::base_dir().join("cron.json5");
        let jobs = crate::agent::tools_cron::read_cron_jobs(&jobs_path).await;
        let reply = crate::agent::tools_cron::format_cron_jobs(&jobs);
        return Some(txt(reply));
    }
    // /cron remove <id-or-index> — delete a cron job. Routed locally so the
    // user gets a deterministic confirmation reply; previously this fell
    // through to the LLM, which often acknowledged silently without
    // surfacing a result message.
    if let Some(rest) = lower
        .strip_prefix("/cron remove ")
        .or_else(|| lower.strip_prefix("/cron rm "))
        .or_else(|| lower.strip_prefix("/cron delete "))
        .or_else(|| lower.strip_prefix("/cron del "))
    {
        let key = rest.trim();
        if key.is_empty() {
            return Some(txt("/cron remove: <id> or <index> required".to_owned()));
        }
        let cron_path = crate::cron::resolve_cron_store_path();
        let _guard = crate::cron::CRON_FILE_LOCK.lock().await;
        let mut jobs = crate::agent::tools_cron::read_cron_jobs(&cron_path).await;
        let zh = crate::i18n::default_lang() == "zh";
        // Match by 1-based index when the arg is a positive integer,
        // otherwise treat as job id (loop-xxxx / cron-xxxx).
        let removed = if let Ok(idx) = key.parse::<usize>()
            && idx >= 1
            && idx <= jobs.len()
        {
            Some(jobs.remove(idx - 1))
        } else {
            jobs.iter()
                .position(|j| j["id"].as_str() == Some(key))
                .map(|p| jobs.remove(p))
        };
        let Some(removed_job) = removed else {
            drop(_guard);
            return Some(txt(if zh {
                format!("/cron remove: 没找到任务 `{key}`")
            } else {
                format!("/cron remove: no job matched `{key}`")
            }));
        };
        if let Err(e) = crate::agent::tools_cron::write_cron_jobs(&cron_path, &jobs).await {
            drop(_guard);
            return Some(txt(format!("/cron remove: failed to save jobs: {e}")));
        }
        drop(_guard);
        crate::cron::trigger_reload();
        let id = removed_job["id"].as_str().unwrap_or(key);
        let summary = removed_job["payload"]["text"]
            .as_str()
            .map(|s| {
                let s = s.trim();
                if s.chars().count() > 60 {
                    let cut: String = s.chars().take(60).collect();
                    format!("{cut}…")
                } else {
                    s.to_owned()
                }
            })
            .unwrap_or_default();
        return Some(txt(if zh {
            if summary.is_empty() {
                format!("已删除任务 {id}")
            } else {
                format!("已删除任务 {id}：{summary}")
            }
        } else if summary.is_empty() {
            format!("Removed job {id}")
        } else {
            format!("Removed job {id}: {summary}")
        }));
    }
    // /loop <interval> <prompt-or-cmd> — schedule a recurring agentTurn
    // back to the originating channel/peer. Persists to cron.json5 and
    // signals the cron runner to reload.
    if lower == "/loop" || lower == "/loop -h" || lower == "/loop --help" || lower == "/loop help" {
        return Some(txt(loop_help_text(crate::i18n::default_lang())));
    }
    if lower.starts_with("/loop ") {
        let rest = t.get(6..).unwrap_or("").trim();
        let (interval_s, prompt) = match rest.split_once(char::is_whitespace) {
            Some((iv, pr)) => (iv.trim(), pr.trim()),
            None => (rest, ""),
        };
        if interval_s.is_empty() || prompt.is_empty() {
            return Some(txt(loop_help_text(crate::i18n::default_lang())));
        }
        let every_ms = match parse_interval_ms(interval_s) {
            Some(v) if v >= 2_000 => v,
            Some(_) => return Some(txt("/loop: interval must be >= 2s".to_owned())),
            None => {
                return Some(txt(format!(
                    "/loop: cannot parse interval `{interval_s}` (e.g. 30s, 5m, 1h, 2h30m)"
                )));
            }
        };
        if peer_id.is_empty() || channel.is_empty() {
            return Some(txt(
                "/loop: missing channel/peer context (cannot schedule delivery)".to_owned(),
            ));
        }
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        let id = format!("loop-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        // ws transport is registered as "desktop" on the delivery side.
        let delivery_channel: &str = if channel == "ws" { "desktop" } else { channel };
        let job = serde_json::json!({
            "id": id,
            "agentId": handle.id,
            "enabled": true,
            "schedule": {"kind": "every", "everyMs": every_ms, "anchorMs": now_ms},
            "payload": {"kind": "agentTurn", "text": prompt},
            "delivery": {"channel": delivery_channel, "to": peer_id, "mode": "always"},
            "createdAtMs": now_ms,
        });
        let cron_path = crate::cron::resolve_cron_store_path();
        let _guard = crate::cron::CRON_FILE_LOCK.lock().await;
        let mut jobs = crate::agent::tools_cron::read_cron_jobs(&cron_path).await;
        jobs.push(job);
        if let Err(e) = crate::agent::tools_cron::write_cron_jobs(&cron_path, &jobs).await {
            return Some(txt(format!("/loop: failed to save jobs: {e}")));
        }
        drop(_guard);
        crate::cron::trigger_reload();
        let zh = crate::i18n::default_lang() == "zh";
        let human = format_interval_ms(every_ms);
        return Some(txt(if zh {
            format!(
                "已安排循环（每 {human}）：{prompt}\nID: {id}\n停止：/cron remove {id}（通过 agent）"
            )
        } else {
            format!(
                "Scheduled loop (every {human}): {prompt}\nID: {id}\nStop with: /cron remove {id} (via agent)"
            )
        }));
    }
    // /watch — live event stream → chat. See
    // docs/superpowers/specs/2026-05-13-watch-design.md
    if lower == "/watch"
        || lower == "/watch -h"
        || lower == "/watch --help"
        || lower == "/watch help"
    {
        return Some(txt(watch_help_text(crate::i18n::default_lang())));
    }
    if let Some(body) = t.strip_prefix("/watch ") {
        let body = body.trim();
        let registry = match crate::gateway::watch::WatchRegistry::global() {
            Some(r) => r,
            None => {
                return Some(txt(
                    "/watch: registry not initialized (gateway still starting?)".to_owned(),
                ));
            }
        };
        let origin_for_watch = match origin {
            PreparseOrigin::User => crate::gateway::watch::Origin::User,
            PreparseOrigin::Cron => crate::gateway::watch::Origin::Cron,
        };
        return match registry
            .handle_command(
                channel,
                peer_id,
                account.map(str::to_owned),
                body,
                origin_for_watch,
            )
            .await
        {
            crate::gateway::watch::WatchCommandReply::Reply(s) => Some(txt(s)),
            crate::gateway::watch::WatchCommandReply::Silent => Some(OutboundMessage::default()),
        };
    }
    // /task with no args or -h/--help → print task help (short-circuit;
    // otherwise it would route into the task queue with an empty message).
    if lower == "/task" || lower == "/task -h" || lower == "/task --help" || lower == "/task help" {
        return Some(txt(task_help_text(crate::i18n::default_lang())));
    }
    // /model — show current model; /models — list providers; /model <name> — switch
    if lower == "/model" || lower == "/models" {
        let model = handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.primary_head())
            .unwrap_or("default");
        let mut lines = vec![format!("Current model: {model}")];
        lines.push(String::new());
        lines.push("Registered providers:".to_owned());
        for name in handle.providers.names() {
            lines.push(format!("  {name}"));
        }
        return Some(txt(lines.join("\n")));
    }
    if lower.starts_with("/model ") {
        let model = t.get(7..).unwrap_or("").trim();
        return Some(txt(format!(
            "Model switched to: {model} (runtime only, use configure to persist)"
        )));
    }
    // /plugin — session-scoped plugin activation control. See plugin_help_text.
    if lower == "/plugin" || lower.starts_with("/plugin ") {
        let Some(cmd) = parse_plugin_command(t) else {
            return Some(txt(plugin_help_text()));
        };
        let session_key = preparse_session_key(handle, channel, peer_id, account);
        let reply = match cmd {
            PluginCommand::Reset => {
                AgentRuntime::clear_plugin_overrides(handle, &session_key);
                "cleared all plugin overrides for this session".to_owned()
            }
            PluginCommand::Status => {
                let snapshot = handle
                    .plugin_overrides
                    .read()
                    .ok()
                    .and_then(|g| g.get(&session_key).cloned())
                    .unwrap_or_default();
                if snapshot.is_empty() {
                    "no plugin overrides (all plugins at default — catalog only)\n\
                     Use `/plugin <name> all` or `/plugin <name> t1,t2` to inject \
                     real ToolDefs for small models."
                        .to_owned()
                } else {
                    let mut lines = vec!["session plugin overrides:".to_owned()];
                    let mut entries: Vec<(&String, &PluginOverride)> = snapshot.iter().collect();
                    entries.sort_by(|a, b| a.0.cmp(b.0));
                    for (p, o) in entries {
                        if o.disabled {
                            lines.push(format!("  {p}: OFF"));
                        } else if o.inject_all {
                            lines.push(format!("  {p}: inject ALL"));
                        } else if o.inject.is_empty() {
                            lines.push(format!("  {p}: default"));
                        } else {
                            lines.push(format!("  {p}: inject [{}]", o.inject.join(", ")));
                        }
                    }
                    lines.join("\n")
                }
            }
            PluginCommand::Info { plugin } => {
                let snapshot = handle
                    .plugin_overrides
                    .read()
                    .ok()
                    .and_then(|g| g.get(&session_key).and_then(|m| m.get(&plugin)).cloned());
                match snapshot {
                    Some(o) if o.disabled => format!("{plugin}: OFF"),
                    Some(o) if o.inject_all => format!("{plugin}: inject ALL"),
                    Some(o) if o.inject.is_empty() => {
                        format!("{plugin}: default (catalog only)")
                    }
                    Some(o) => format!("{plugin}: inject [{}]", o.inject.join(", ")),
                    None => format!("{plugin}: default (no override)"),
                }
            }
            PluginCommand::SetState { plugin, action } => {
                let new_override = match action {
                    PluginAction::Off => PluginOverride {
                        disabled: true,
                        ..Default::default()
                    },
                    PluginAction::Default => PluginOverride::default(),
                    PluginAction::All => PluginOverride {
                        inject_all: true,
                        ..Default::default()
                    },
                    PluginAction::Inject(tools) => PluginOverride {
                        inject: tools,
                        ..Default::default()
                    },
                };
                let summary = if new_override.disabled {
                    format!("{plugin}: OFF")
                } else if new_override.inject_all {
                    format!("{plugin}: inject ALL (capped at user_tools_cap)")
                } else if new_override.inject.is_empty() {
                    format!("{plugin}: default")
                } else {
                    format!("{plugin}: inject [{}]", new_override.inject.join(", "))
                };
                AgentRuntime::set_plugin_override(handle, &session_key, &plugin, new_override);
                summary
            }
            PluginCommand::Pin { plugin, tool } => {
                // Additive: add `tool` to the session override's `pin` set
                // (and remove from `unpin` if it was there). The session
                // override may not exist yet — start from default.
                AgentRuntime::mutate_plugin_override(handle, &session_key, &plugin, |o| {
                    o.unpin.retain(|t| t != &tool);
                    if !o.pin.iter().any(|t| t == &tool) {
                        o.pin.push(tool.clone());
                    }
                });
                format!("{plugin}__{tool}: pinned to user_tools")
            }
            PluginCommand::Unpin { plugin, tool } => {
                // Additive: add `tool` to the session override's `unpin` set
                // (and remove from `pin` if it was there).
                AgentRuntime::mutate_plugin_override(handle, &session_key, &plugin, |o| {
                    o.pin.retain(|t| t != &tool);
                    if !o.unpin.iter().any(|t| t == &tool) {
                        o.unpin.push(tool.clone());
                    }
                });
                format!("{plugin}__{tool}: unpinned from user_tools")
            }
            PluginCommand::Headlines { plugin } => render_headlines_for_plugin(&plugin),
        };
        return Some(txt(reply));
    }

    // /run <cmd>, /sh <cmd>, /exec <cmd>, ! <cmd>, $ <cmd> — shell execution
    let shell_cmd: Option<&str> =
        if lower.starts_with("/run ") || lower.starts_with("/sh ") || lower.starts_with("/exec ") {
            t.find(' ').map(|i| t[i + 1..].trim())
        } else if t.starts_with("! ") {
            Some(t[2..].trim())
        } else if t.starts_with("$ ") {
            Some(t[2..].trim())
        } else {
            None
        };
    if let Some(cmd) = shell_cmd {
        tracing::warn!(command = %cmd, "executing shell command via preparse (open dmPolicy)");

        let (shell, arg) = if cfg!(target_os = "windows") {
            ("powershell", "-Command")
        } else {
            ("sh", "-c")
        };
        let ws = workspace();
        let mut proc = tokio::process::Command::new(shell);
        proc.args([arg, cmd])
            .current_dir(&ws)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            proc.creation_flags(CREATE_NO_WINDOW);
        }
        let out = proc.output().await;
        let reply = match out {
            Ok(o) => {
                let mut result = String::from_utf8_lossy(&o.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&o.stderr);
                if !stderr.trim().is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(stderr.trim());
                }
                if result.trim().is_empty() {
                    if o.status.success() {
                        "(no output)".to_owned()
                    } else {
                        format!("exit {}", o.status.code().unwrap_or(-1))
                    }
                } else {
                    result
                }
            }
            Err(e) => format!("exec error: {e}"),
        };
        return Some(txt(reply));
    }
    // /astock — A股数据/调度快路径。子命令:
    //   briefing list / next / run <slot>
    //   watchlist list / add <code...> / remove <code...> / clear
    //   quote <code>
    //   snapshot [code...]
    //   chart <code> [period] [count]
    // 不进 LLM，直接调 astock 子模块。
    // /goal — result-driven turn loop. See src/agent/goal.rs.
    //
    //   /goal <condition>             # set + start
    //   /goal                         # status
    //   /goal clear                   # abort
    //   /goal <cond> --max <N>        # override default iter cap
    if lower == "/goal" {
        return Some(txt(goal_status_handler(handle, channel, peer_id).await));
    }
    if let Some(rest_raw) = t.strip_prefix("/goal ") {
        let rest = rest_raw.trim();
        if rest.is_empty() {
            return Some(txt(goal_status_handler(handle, channel, peer_id).await));
        }
        if rest.eq_ignore_ascii_case("clear")
            || rest.eq_ignore_ascii_case("stop")
            || rest.eq_ignore_ascii_case("abort")
        {
            return Some(txt(goal_clear_handler(handle, channel, peer_id).await));
        }
        if rest.eq_ignore_ascii_case("status") {
            return Some(txt(goal_status_handler(handle, channel, peer_id).await));
        }
        return Some(txt(
            goal_set_handler(handle, channel, peer_id, account, rest).await,
        ));
    }
    if lower == "/astock" || lower == "/astock help" || lower == "/astock -h" || lower == "/astock --help" {
        return Some(txt(astock_help_text()));
    }
    if let Some(rest) = lower.strip_prefix("/astock ") {
        let raw_rest = t.strip_prefix("/astock ").unwrap_or(rest).trim();
        // chart needs to attach the rendered PNG, so it bypasses the
        // text-only `txt` helper and builds its own OutboundMessage.
        // All other sub-commands stay text-only.
        let first_word = raw_rest
            .split_whitespace()
            .next()
            .map(str::to_lowercase)
            .unwrap_or_default();
        if first_word == "chart" {
            let chart_args: Vec<String> = raw_rest
                .split_whitespace()
                .skip(1)
                .map(str::to_owned)
                .collect();
            return Some(astock_chart_outbound(chart_args).await);
        }
        return Some(txt(
            handle_astock_command(handle, channel, peer_id, raw_rest).await,
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// /astock dispatcher
// ---------------------------------------------------------------------------
//
// Routes the `/astock <sub> <args...>` form to the appropriate
// astock helper. Returns a single plain-text reply (the only shape
// the fast preparse path can use). Chart sub-command returns a
// special marker token consumed by the higher-level wrapper —
// see `dispatch_astock_chart` for the image-attachment path.

// ---------------------------------------------------------------------------
// /goal handlers — completion-driven turn loop.
// ---------------------------------------------------------------------------
//
// The goal LOOP itself lives in `gateway/startup.rs` (post-`run_turn`
// hook calling `crate::agent::goal::check_after_turn`). The slash
// command here only manages the goal STATE (set / clear / status)
// and kicks off the first turn via `submit_to_queue`.

async fn goal_set_handler(
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    account: Option<&str>,
    rest: &str,
) -> String {
    // Parse `<condition> [--max <N>]`. Last-token form so users can
    // write `/goal cargo test 全过 --max 50` without quoting.
    let (condition, max_iter) = parse_goal_args(rest);
    if condition.is_empty() {
        return "用法: /goal <condition> [--max <N>]\n例: /goal cargo test 全过 --max 50"
            .to_owned();
    }
    let Some(mem) = crate::agent::memory::global_store() else {
        return "_memory 未启用,/goal 无法持久化_".to_owned();
    };
    let session_key = preparse_session_key(handle, channel, peer_id, account);
    if let Err(e) =
        crate::agent::goal::set(&mem, &session_key, &condition, max_iter).await
    {
        return format!("/goal: 写入失败: {e:#}");
    }
    // Kick off the first turn via the task queue (same path the channel
    // dispatcher uses), so the LLM sees a synthetic user message asking
    // it to start working on the goal.
    let Some(tq) = crate::gateway::task_queue::get_task_queue() else {
        return format!(
            "Goal set: {condition} (iter cap {max_iter})\n\
             _任务队列未启用,首轮不会自动启动 — 请手动发一条消息触发_"
        );
    };
    let prompt = crate::agent::goal::build_initial_prompt(&condition, max_iter);
    let delivery_channel: &str = if channel == "ws" { "desktop" } else { channel };
    if let Err(e) = crate::gateway::task_queue::submit_to_queue(
        &tq,
        &session_key,
        &prompt,
        delivery_channel,
        peer_id,
        peer_id,
        false,
        crate::gateway::task_queue::Priority::Cron,
    ) {
        return format!(
            "Goal set: {condition} (iter cap {max_iter})\n\
             _首轮 enqueue 失败: {e}_"
        );
    }
    format!(
        "🎯 Goal set: {condition}\n\
         iter cap: {max_iter}\n\
         首轮已 enqueue,稍候片刻。\n\n\
         随时 `/goal clear` 中止,或 `/goal` 看进度。"
    )
}

async fn goal_clear_handler(
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
) -> String {
    let Some(mem) = crate::agent::memory::global_store() else {
        return "_memory 未启用_".to_owned();
    };
    let session_key = preparse_session_key(handle, channel, peer_id, None);
    let active = crate::agent::goal::read(&mem, &session_key).await;
    if let Err(e) = crate::agent::goal::clear(&mem, &session_key).await {
        return format!("/goal clear: 失败: {e:#}");
    }
    match active {
        Some(g) => format!(
            "🛑 Goal cleared: {} (was iter {}/{})",
            g.condition, g.iter, g.max_iter
        ),
        None => "_当前没有进行中的 goal_".to_owned(),
    }
}

async fn goal_status_handler(
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
) -> String {
    let Some(mem) = crate::agent::memory::global_store() else {
        return "_memory 未启用_".to_owned();
    };
    let session_key = preparse_session_key(handle, channel, peer_id, None);
    match crate::agent::goal::read(&mem, &session_key).await {
        Some(g) => {
            let elapsed_secs = (chrono::Utc::now().timestamp() - g.started_at).max(0);
            format!(
                "🎯 Goal active: {}\niter: {}/{}\n已运行: {}s",
                g.condition, g.iter, g.max_iter, elapsed_secs
            )
        }
        None => "_当前没有进行中的 goal — 用 `/goal <condition>` 启动_".to_owned(),
    }
}

/// Parse `<condition> [--max <N>]`. `--max` is the only recognised
/// flag for now. Returns `(condition_text, max_iter)`. Missing or
/// malformed `--max` falls back to `DEFAULT_MAX_ITER`.
fn parse_goal_args(rest: &str) -> (String, u32) {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let mut max_iter: u32 = crate::agent::goal::DEFAULT_MAX_ITER;
    let mut cond_toks: Vec<&str> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == "--max" || toks[i] == "-m" {
            if let Some(n) = toks.get(i + 1).and_then(|s| s.parse::<u32>().ok()) {
                max_iter = n;
                i += 2;
                continue;
            }
        }
        cond_toks.push(toks[i]);
        i += 1;
    }
    (cond_toks.join(" ").trim().to_owned(), max_iter)
}

fn astock_help_text() -> String {
    "/astock — A股数据 & 调度 (本地快路径，不进 LLM)\n\n\
     调度\n\
       /astock briefing list                 三个时段 + 下次 fire\n\
       /astock briefing next                 只看下次\n\
       /astock briefing run <slot>           立即补发 (premarket|midday|postmarket)\n\n\
     自选股\n\
       /astock watchlist list                当前自选股\n\
       /astock watchlist add <code> [...]    加入 (支持批量)\n\
       /astock watchlist remove <code> [...] 删除\n\
       /astock watchlist clear               清空\n\n\
     即时数据\n\
       /astock quote <code> [<code>...]      实时报价\n\
       /astock snapshot [<code>...]          快照 (空参 = 自选股)\n\
       /astock chart <code> [period] [count] K 线图 (会推送到当前会话)\n\
       /astock screen <filter> [limit]       异动筛选 (quick_rally / quick_goldcross / ...)\n\n\
     备注: astock 未启用时所有子命令静默回 '_astock 未启用_'。"
        .to_owned()
}

async fn handle_astock_command(
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    rest: &str,
) -> String {
    let mut parts = rest.split_whitespace();
    let Some(sub) = parts.next() else {
        return astock_help_text();
    };
    let args: Vec<String> = parts.map(str::to_owned).collect();
    match sub.to_lowercase().as_str() {
        "briefing" => astock_briefing(args).await,
        "watchlist" => astock_watchlist(handle, channel, peer_id, args).await,
        "quote" => astock_quote(args).await,
        "snapshot" => astock_snapshot(handle, channel, peer_id, args).await,
        "screen" => astock_screen(args).await,
        // `chart` is handled directly at the slash callsite so the
        // PNG can be attached as an image — it never reaches this
        // dispatcher. Document the case for the reader.
        "chart" => "internal error: chart should be handled at the slash callsite".to_owned(),
        other => format!("/astock: 未知子命令 `{other}`。看 `/astock help`"),
    }
}

async fn astock_briefing(args: Vec<String>) -> String {
    let action = args
        .first()
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "list".to_owned());
    match action.as_str() {
        "list" | "" => {
            let snapshot = crate::astock::briefing::schedule_snapshot();
            let mut lines = vec!["**早报调度**".to_owned()];
            for (slot, when) in snapshot {
                lines.push(format!(
                    "  {}  ·  {}  →  {}",
                    slot.slug(),
                    slot.label(),
                    when.format("%Y-%m-%d %H:%M %Z")
                ));
            }
            lines.push(String::new());
            lines.push(
                "提示: 调度写在 src/astock/briefing.rs，时间表硬编码。\
                 改时间需 rebuild + `cargo run -- gateway restart`。"
                    .to_owned(),
            );
            lines.join("\n")
        }
        "next" => {
            let snapshot = crate::astock::briefing::schedule_snapshot();
            // schedule_snapshot returns slots in wallclock order; find
            // the earliest absolute time among them — that's "next".
            match snapshot.into_iter().min_by_key(|(_, w)| *w) {
                Some((slot, when)) => format!(
                    "下次: {} ({}) @ {}",
                    slot.slug(),
                    slot.label(),
                    when.format("%Y-%m-%d %H:%M %Z")
                ),
                None => "_无可用 slot_".to_owned(),
            }
        }
        "run" => {
            let Some(slug) = args.get(1) else {
                return "用法: /astock briefing run <premarket|midday|postmarket>".to_owned();
            };
            let Some(slot) = crate::astock::briefing::BriefingSlot::from_slug(slug) else {
                return format!(
                    "未知 slot `{slug}`，可选: premarket | midday | postmarket"
                );
            };
            // Fire-and-forget — the dispatch path drops a synthetic
            // user turn onto the task queue, which the agent runtime
            // handles asynchronously. The reply we return below is
            // immediate; the briefing itself arrives a few seconds
            // later via the normal channel send path.
            tokio::spawn(async move {
                crate::astock::briefing::dispatch_one(slot).await;
            });
            format!("已触发 {} ({})，几秒内到达。", slot.slug(), slot.label())
        }
        other => format!("briefing: 未知动作 `{other}`，可选: list | next | run"),
    }
}

async fn astock_watchlist(
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    args: Vec<String>,
) -> String {
    let action = args
        .first()
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "list".to_owned());
    let Some(mem) = crate::agent::memory::global_store() else {
        return "_memory 未启用，自选股无法持久化_".to_owned();
    };
    let scope = crate::agent::tools_stock::watchlist_scope(&handle.id, channel, peer_id);
    let result = match action.as_str() {
        "list" | "" => crate::agent::tools_stock::watchlist_list(&mem, &scope).await,
        "add" => {
            let codes: Vec<String> = args.iter().skip(1).cloned().collect();
            crate::agent::tools_stock::watchlist_add(&mem, &scope, codes).await
        }
        "remove" | "rm" | "delete" | "del" => {
            let codes: Vec<String> = args.iter().skip(1).cloned().collect();
            crate::agent::tools_stock::watchlist_remove(&mem, &scope, codes).await
        }
        "clear" => crate::agent::tools_stock::watchlist_clear(&mem, &scope).await,
        other => {
            return format!(
                "watchlist: 未知动作 `{other}`，可选: list | add | remove | clear"
            );
        }
    };
    let v = match result {
        Ok(v) => v,
        Err(e) => return format!("/astock watchlist 出错: {e:#}"),
    };
    // For `list`, enrich with display names so the reply reads as
    // `600519 贵州茅台 / 000001 平安银行` instead of bare codes.
    // The name resolver caches aggressively (1h) so this is a noop
    // round trip after the first call.
    if matches!(action.as_str(), "list" | "") {
        if let Some(arc) = crate::astock::global_client() {
            let codes: Vec<String> = v
                .get("codes")
                .and_then(|x| x.as_array())
                .map(|arr| arr.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            if !codes.is_empty() {
                let names = arc.resolve_names(&codes).await;
                return format_watchlist_list_with_names(&codes, &names);
            }
        }
    }
    format_watchlist_reply(&action, &v)
}

fn format_watchlist_list_with_names(
    codes: &[String],
    names: &std::collections::HashMap<String, String>,
) -> String {
    if codes.is_empty() {
        return "_自选股为空 — `/astock watchlist add <code>` 加一只_".to_owned();
    }
    let mut lines = vec![format!("自选股 ({})", codes.len())];
    for c in codes {
        let name = names.get(c).map(String::as_str).unwrap_or("");
        if name.is_empty() {
            lines.push(format!("- {c}"));
        } else {
            lines.push(format!("- {c} {name}"));
        }
    }
    lines.join("\n")
}

fn format_watchlist_reply(action: &str, v: &serde_json::Value) -> String {
    let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
    if !ok {
        return v
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("_出错_")
            .to_owned();
    }
    match action {
        "list" | "" => {
            let codes: Vec<&str> = v
                .get("codes")
                .and_then(|x| x.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            if codes.is_empty() {
                "_自选股为空 — `/astock watchlist add <code>` 加一只_".to_owned()
            } else {
                format!(
                    "自选股 ({}): {}",
                    codes.len(),
                    codes.join(", ")
                )
            }
        }
        "add" => {
            let added: Vec<&str> = v
                .get("added")
                .and_then(|x| x.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
            if added.is_empty() {
                format!("已存在或无新代码 (当前 {total} 只)")
            } else {
                format!(
                    "已加入 {}: {}  (当前 {} 只)",
                    added.len(),
                    added.join(", "),
                    total
                )
            }
        }
        "remove" | "rm" | "delete" | "del" => {
            let removed = v.get("removed").and_then(|x| x.as_u64()).unwrap_or(0);
            let requested = v.get("requested").and_then(|x| x.as_u64()).unwrap_or(0);
            format!("删除 {removed}/{requested}")
        }
        "clear" => {
            let removed = v.get("removed").and_then(|x| x.as_u64()).unwrap_or(0);
            format!("已清空 (删除 {removed} 只)")
        }
        _ => v.to_string(),
    }
}

async fn astock_quote(args: Vec<String>) -> String {
    let codes: Vec<String> = args
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if codes.is_empty() {
        return "用法: /astock quote <code> [<code>...]".to_owned();
    }
    let Some(arc) = crate::astock::global_client() else {
        return "_astock 未启用_".to_owned();
    };
    let quote_codes = codes.clone();
    let names_codes = codes;
    // Fetch quotes + names in parallel. Names are best-effort —
    // we render code-only if resolution times out / fails.
    let quotes_fut = async {
        if quote_codes.len() == 1 {
            arc.quote(&quote_codes[0]).await.map(|q| vec![q])
        } else {
            arc.quote_batch(&quote_codes).await
        }
    };
    let names_fut = arc.resolve_names(&names_codes);
    let (quotes_res, names) = tokio::join!(quotes_fut, names_fut);
    let quotes = match quotes_res {
        Ok(q) => q,
        Err(e) => return format!("astock quote: {e}"),
    };
    crate::agent::tools_stock::render_quotes_markdown_with_names(&quotes, Some(&names))
}

async fn astock_screen(args: Vec<String>) -> String {
    let filter = match args.first() {
        Some(f) if !f.is_empty() => f.clone(),
        _ => {
            return "用法: /astock screen <quick_filter> [limit]\n\
                    可选 filter: quick_rally | quick_goldcross | \
                    quick_cow_catch | quick_deadtogold"
                .to_owned();
        }
    };
    let limit = args.get(1).and_then(|s| s.parse::<u32>().ok()).or(Some(10));
    let Some(arc) = crate::astock::global_client() else {
        return "_astock 未启用_".to_owned();
    };
    let resp = match arc.screen_quick(&filter, limit).await {
        Ok(v) => v,
        Err(e) => return format!("astock screen: {e}"),
    };
    format_screen_reply(&filter, &resp)
}

/// Render the screen-quick JSON tree as IM-friendly markdown.
///
/// Astock's response shape (verified live against quick_rally at
/// 2026-06-11 13:39):
///   {
///     "as_of_ts": null,
///     "computed_at": "2026-06-11T13:39:30+08:00",
///     "cost_credits": 85,
///     "count": 0,
///     "filters_applied": [...],
///     "limit": 50,
///     "results": [...],
///     "total_matched": 0
///   }
///
/// Each entry in `results` is per-filter — we surface code/name when
/// present and otherwise dump the row as compact JSON. Defensive on
/// shape because astock's filters expose different telemetry per
/// type (rally has pct_change, goldcross has ma cross series, ...).
fn format_screen_reply(filter: &str, v: &serde_json::Value) -> String {
    let total = v
        .get("total_matched")
        .and_then(|x| x.as_u64())
        .or_else(|| v.get("count").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let cost = v
        .get("cost_credits")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let computed = v
        .get("computed_at")
        .and_then(|x| x.as_str())
        .unwrap_or("?");
    let results = v
        .get("results")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let mut lines = vec![format!(
        "**{}** · {} 命中 · {} 积分 · {}",
        filter, total, cost, computed
    )];
    if results.is_empty() {
        lines.push("_无命中_".to_owned());
        return lines.join("\n");
    }
    for row in results.iter().take(20) {
        let code = row
            .get("code")
            .and_then(|x| x.as_str())
            .unwrap_or("?");
        let name = row
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        // Best-effort: surface whichever metric column the filter
        // chose to highlight, falling back to a compact JSON dump.
        let metric = row
            .get("pct_change")
            .or_else(|| row.get("change_pct"))
            .or_else(|| row.get("score"))
            .or_else(|| row.get("rally"))
            .and_then(|x| x.as_f64())
            .map(|p| format!("{:+.2}%", p))
            .unwrap_or_else(|| {
                serde_json::to_string(row)
                    .map(|s| {
                        if s.len() > 60 {
                            // CJK-safe truncate — `&s[..60]` can
                            // panic mid-codepoint. See
                            // crate::util::truncate_str.
                            format!("{}…", crate::util::truncate_str(&s, 60))
                        } else {
                            s
                        }
                    })
                    .unwrap_or_default()
            });
        let head = if name.is_empty() {
            code.to_owned()
        } else {
            format!("{} {}", code, name)
        };
        lines.push(format!("- {}  {}", head, metric));
    }
    if results.len() > 20 {
        lines.push(format!("_…剩余 {} 行_", results.len() - 20));
    }
    lines.join("\n")
}

async fn astock_snapshot(
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    args: Vec<String>,
) -> String {
    let mut codes: Vec<String> = args
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let Some(arc) = crate::astock::global_client() else {
        return "_astock 未启用_".to_owned();
    };
    let mut used_watchlist = false;
    if codes.is_empty() {
        // No explicit args → fall back to the caller's watchlist
        // (matches `tool_stock_snapshot`'s `use_watchlist` default).
        if let Some(mem) = crate::agent::memory::global_store() {
            let scope = crate::agent::tools_stock::watchlist_scope(&handle.id, channel, peer_id);
            let store = mem.lock().await;
            codes = store
                .list_active()
                .into_iter()
                .filter(|d| d.scope == scope && d.kind == "watchlist")
                .map(|d| d.text)
                .collect();
            drop(store);
            used_watchlist = !codes.is_empty();
        }
    }
    if codes.is_empty() {
        return "_无代码且自选股为空 — `/astock watchlist add <code>` 先加一只_".to_owned();
    }
    let rows = match arc.snapshot(None, None, &codes, None).await {
        Ok(r) => r,
        Err(e) => return format!("astock snapshot: {e}"),
    };
    crate::agent::tools_stock::render_snapshot_markdown(&rows, used_watchlist, "amount")
}

/// Render a chart and return an `OutboundMessage` with the PNG
/// attached as a base64 data URI. The channel sender (feishu/wechat)
/// already knows how to upload `data:image/png;base64,...` payloads
/// from the `images` field — see `feishu.rs::send_text_with_images`.
async fn astock_chart_outbound(args: Vec<String>) -> OutboundMessage {
    let code = match args.first() {
        Some(c) if !c.is_empty() => c.clone(),
        _ => {
            return OutboundMessage {
                text: "用法: /astock chart <code> [period] [count]".to_owned(),
                ..Default::default()
            };
        }
    };
    let period = args.get(1).cloned().unwrap_or_else(|| "1d".to_owned());
    let count_n = args.get(2).and_then(|s| s.parse::<u32>().ok()).unwrap_or(60);
    let input = crate::agent::tools_stock::ChartRenderInput {
        code: &code,
        period: &period,
        count: count_n,
        adjust: "qfq",
        ma_periods: vec![5, 10, 20, 60],
        name_hint: None,
    };
    let out = match crate::agent::tools_stock::render_chart_for_code(input).await {
        Ok(o) => o,
        Err(crate::agent::tools_stock::ChartRenderError::Dormant) => {
            return OutboundMessage {
                text: "_astock 未启用_".to_owned(),
                ..Default::default()
            };
        }
        Err(crate::agent::tools_stock::ChartRenderError::Soft(msg)) => {
            return OutboundMessage {
                text: msg,
                ..Default::default()
            };
        }
    };
    // PNG → base64 data URI. Same shape as
    // `server::resolve_media_to_image_data` and what the feishu
    // sender expects from `OutboundMessage::images`.
    use base64::Engine;
    let bytes = match tokio::fs::read(&out.path).await {
        Ok(b) => b,
        Err(e) => {
            return OutboundMessage {
                text: format!("chart 渲染成功但读取失败: {e}"),
                ..Default::default()
            };
        }
    };
    let data_uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    OutboundMessage {
        text: out.title,
        images: vec![data_uri],
        ..Default::default()
    }
}

/// Check if a message is a fast preparse command that should bypass the
/// per-user queue. These are local slash commands that execute instantly and
/// should not wait behind slow LLM requests in the queue.
pub(crate) fn is_fast_preparse(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_lowercase();
    // Single-word commands (no args needed)
    matches!(
        lower.as_str(),
        "/ls" | "/status" | "/version" | "/help" | "/?" | "/health" | "/uptime"
            | "/model" | "/models" | "/cron" | "/clear" | "/new" | "/abort" | "/sessions"
            | "/loop" | "/task" | "/watch" | "/plugin" | "/cap" | "/cap-exit" | "/cap-resume"
            | "/astock" | "/goal"
    )
    // Commands with optional/required args
    || lower.starts_with("/ls ")
    || lower.starts_with("/cat ")
    || lower.starts_with("/ss")
    || lower.starts_with("/webshot")
    || lower.starts_with("/remember ")
    || lower.starts_with("/recall ")
    || lower.starts_with("/cron ")
    || lower.starts_with("/skill ")
    || lower.starts_with("/plugin ")
    || lower.starts_with("/model ")
    || lower.starts_with("/run ")
    || lower.starts_with("/sh ")
    || lower.starts_with("/exec ")
    || lower.starts_with("/loop ")
    || lower.starts_with("/watch ")
    || lower.starts_with("/cap ")
    || lower.starts_with("/cap-resume ")
    || lower.starts_with("/astock ")
    || lower.starts_with("/goal ")
    // /task only short-circuits on help variants; non-help forms must NOT
    // bypass the queue (the task queue worker owns the multi-turn flow).
    || lower == "/task -h"
    || lower == "/task --help"
    || lower == "/task help"
    || t.starts_with("! ")
    || t.starts_with("$ ")
}

// ---------------------------------------------------------------------------
// Helpers for /loop
// ---------------------------------------------------------------------------

/// Parse a human-readable interval like "30s", "5m", "1h", "2h30m", "1d".
/// A bare number is interpreted as seconds.
/// Spawn a background task that waits for the actor to capture the
/// agent's native session_id (via `AgentEvent::Ready`), then pushes a
/// follow-up IM message with the `/cap-resume` command the user can
/// copy-paste to resume this exact conversation later. Fires at most
/// once per /cap call; silent if the id never lands (30s timeout).
/// Resolve a user-supplied `/cap <agent> [path]` workspace argument.
///
/// Empty input → fall back to the caller's `default` (the rsclaw
/// configured workspace). Non-empty input must:
///   * tilde-expand (`~/foo` → `$HOME/foo`),
///   * resolve to an existing directory,
///   * canonicalize within the user's home directory.
///
/// The home-only check is a deliberate safety floor — coding agents
/// can spawn writes/exec, and accepting `/`, `/etc`, `/System`,
/// `/var`, or sibling-user dirs from a chat message would be a real
/// foot-gun. Inside `$HOME` the user already has full write authority,
/// so this is the minimum gate that prevents misrouted IM messages
/// from pointing a subagent at system roots.
fn resolve_cap_workspace(
    input: &str,
    default: std::path::PathBuf,
) -> std::result::Result<std::path::PathBuf, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(default);
    }
    let expanded = if let Some(rest) = input.strip_prefix("~/") {
        match dirs_next::home_dir() {
            Some(h) => h.join(rest),
            None => return Err("HOME not available, cannot expand ~".to_string()),
        }
    } else if input == "~" {
        match dirs_next::home_dir() {
            Some(h) => h,
            None => return Err("HOME not available, cannot expand ~".to_string()),
        }
    } else {
        std::path::PathBuf::from(input)
    };
    let canon = std::fs::canonicalize(&expanded)
        .map_err(|e| format!("path not accessible ({e})"))?;
    if !canon.is_dir() {
        return Err("not a directory".to_string());
    }
    let home = dirs_next::home_dir().ok_or_else(|| "HOME not available".to_string())?;
    let home_canon = std::fs::canonicalize(&home).unwrap_or(home);
    if !canon.starts_with(&home_canon) {
        return Err(format!(
            "path must be under {}",
            home_canon.display()
        ));
    }
    Ok(canon)
}

fn spawn_resume_hint_followup(
    manager: std::sync::Arc<crate::cap::CapLiveManager>,
    cap_sid: String,
    kind: crate::cap::AgentKind,
    target_id: String,
    channel: String,
    account: Option<String>,
    lang: &'static str,
) {
    let Some(notif_tx) = manager.notification_tx() else {
        return;
    };
    if target_id.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let Some(nsid) = manager
            .wait_agent_session_id(&cap_sid, std::time::Duration::from_secs(30))
            .await
        else {
            tracing::debug!(
                target: "cap",
                cap_session_id = %cap_sid,
                "resume-hint followup: native id not captured in 30s; skipping"
            );
            return;
        };
        let text = crate::i18n::t_fmt(
            "cap_resume_hint",
            lang,
            &[("agent", kind.as_str()), ("session_id", &nsid)],
        );
        let msg = crate::channel::OutboundMessage {
            target_id,
            // Sticky bindings only exist for DM (per-channel-peer scope),
            // so this is always a 1:1 chat. Group chats don't have a
            // sticky-mode UX today.
            is_group: false,
            text,
            reply_to: None,
            images: vec![],
            files: vec![],
            channel: Some(channel),
            account,
        };
        if let Err(e) = notif_tx.send(msg) {
            tracing::warn!(
                target: "cap",
                error = %e,
                "resume-hint followup: notification_tx send failed"
            );
        }
    });
}

fn parse_interval_ms(s: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut num_buf = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num_buf.push(c);
        } else if c.is_ascii_whitespace() {
            continue;
        } else if c.is_ascii_alphabetic() {
            if num_buf.is_empty() {
                return None;
            }
            let n: u64 = num_buf.parse().ok()?;
            num_buf.clear();
            let mul: u64 = match c.to_ascii_lowercase() {
                's' => 1_000,
                'm' => 60_000,
                'h' => 3_600_000,
                'd' => 86_400_000,
                _ => return None,
            };
            total = total.checked_add(n.checked_mul(mul)?)?;
        } else {
            return None;
        }
    }
    if !num_buf.is_empty() {
        // Bare trailing number → seconds (e.g. "300" → 300s).
        let n: u64 = num_buf.parse().ok()?;
        total = total.checked_add(n.checked_mul(1_000)?)?;
    }
    if total == 0 { None } else { Some(total) }
}

/// Format an interval in milliseconds as a compact human-readable string.
fn format_interval_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let mut parts: Vec<String> = Vec::new();
    if d > 0 {
        parts.push(format!("{d}d"));
    }
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 {
        parts.push(format!("{s}s"));
    }
    if parts.is_empty() {
        "0s".to_owned()
    } else {
        parts.join("")
    }
}

/// Top-level /help text — one-screen overview of all preparse slash
/// commands. Per-command details live in `<cmd> -h` sub-helps where the
/// command supports flags (currently /loop, /task).
fn help_text(lang: &str) -> String {
    if lang == "zh" {
        "RsClaw 命令\n\n\
         状态/版本\n\
         \u{0020}\u{0020}/version  /uptime  /status  /health\n\n\
         会话\n\
         \u{0020}\u{0020}/new      新会话\n\
         \u{0020}\u{0020}/clear    清当前会话历史\n\
         \u{0020}\u{0020}/abort    中止当前/所有运行中的回合\n\
         \u{0020}\u{0020}/sessions 列出会话\n\n\
         模型\n\
         \u{0020}\u{0020}/model           显示当前模型\n\
         \u{0020}\u{0020}/models          列出可用模型\n\
         \u{0020}\u{0020}/model <name>    切换主模型\n\n\
         任务/调度\n\
         \u{0020}\u{0020}/task -h         多轮任务（详见 -h）\n\
         \u{0020}\u{0020}/loop -h         定时循环（详见 -h）\n\
         \u{0020}\u{0020}/goal <cond>     盯一个结果型目标，模型自评 GOAL_ACHIEVED/FAILED 终止\n\
         \u{0020}\u{0020}/cron list       查看定时任务\n\n\
         A股 (astock)\n\
         \u{0020}\u{0020}/astock          A股数据 & 早报调度（详见 /astock help）\n\n\
         文件/截图\n\
         \u{0020}\u{0020}/ls [path]       列出工作区目录\n\
         \u{0020}\u{0020}/cat <file>      查看文件内容\n\
         \u{0020}\u{0020}/ss              桌面截图\n\
         \u{0020}\u{0020}/webshot <url>   网页截图\n\n\
         技能/插件\n\
         \u{0020}\u{0020}/skill list      已安装技能\n\n\
         编程代理直连\n\
         \u{0020}\u{0020}/cap <agent>            绑定本会话直连 cap 子代理\n\
         \u{0020}\u{0020}/cap-resume <ag> <id>   按 ID 恢复磁盘保存的会话\n\
         \u{0020}\u{0020}/cap-exit                释放绑定，恢复主 LLM\n\n\
         其他\n\
         \u{0020}\u{0020}/btw <问题>      旁路一次性提问，不写入会话\n\
         \u{0020}\u{0020}!cmd  /  $cmd    在工作区执行一行 shell 命令\n\
         \u{0020}\u{0020}/help  /  /?     本帮助"
            .to_owned()
    } else {
        "RsClaw commands\n\n\
         Status / version\n\
         \u{0020}\u{0020}/version  /uptime  /status  /health\n\n\
         Session\n\
         \u{0020}\u{0020}/new      start a new session\n\
         \u{0020}\u{0020}/clear    wipe current session history\n\
         \u{0020}\u{0020}/abort    abort the current / all running turns\n\
         \u{0020}\u{0020}/sessions list sessions\n\n\
         Model\n\
         \u{0020}\u{0020}/model           show current model\n\
         \u{0020}\u{0020}/models          list available models\n\
         \u{0020}\u{0020}/model <name>    switch primary model\n\n\
         Task / schedule\n\
         \u{0020}\u{0020}/task -h         multi-turn task (see -h)\n\
         \u{0020}\u{0020}/loop -h         repeat on a schedule (see -h)\n\
         \u{0020}\u{0020}/goal <cond>     result-driven loop; model self-tags GOAL_ACHIEVED/FAILED to stop\n\
         \u{0020}\u{0020}/cron list       view cron jobs\n\n\
         A-shares (astock)\n\
         \u{0020}\u{0020}/astock          A-share data & briefing scheduler (see /astock help)\n\n\
         File / screenshot\n\
         \u{0020}\u{0020}/ls [path]       list workspace directory\n\
         \u{0020}\u{0020}/cat <file>      view file contents\n\
         \u{0020}\u{0020}/ss              desktop screenshot\n\
         \u{0020}\u{0020}/webshot <url>   web-page screenshot\n\n\
         Skills / plugins\n\
         \u{0020}\u{0020}/skill list      installed skills\n\n\
         Coding-agent direct mode\n\
         \u{0020}\u{0020}/cap <agent>            route this chat directly to a cap subagent\n\
         \u{0020}\u{0020}/cap-resume <ag> <id>   resume a saved session by id\n\
         \u{0020}\u{0020}/cap-exit                release binding, resume main LLM\n\n\
         Other\n\
         \u{0020}\u{0020}/btw <q>         side-channel ask, not added to session\n\
         \u{0020}\u{0020}!cmd  /  $cmd    run a one-line shell command in the workspace\n\
         \u{0020}\u{0020}/help  /  /?     this help"
            .to_owned()
    }
}

/// Help text for /loop. Localized en/zh.
fn loop_help_text(lang: &str) -> String {
    if lang == "zh" {
        "/loop <间隔> <提示词或命令>\n\n\
         以指定间隔重复执行：把 <提示词> 当作一个新的消息发送给当前 agent，\n\
         agent 的回复会通过当前渠道返回给你。\n\
         <提示词> 可以是任何 /help 列出的命令，也可以是普通的提示词。\n\n\
         间隔示例：30s, 5m, 1h, 2h30m, 1d（最小 2s）\n\
         例：\n\
         \u{0020}\u{0020}/loop 5m 检查邮箱有没有新邮件\n\
         \u{0020}\u{0020}/loop 1h /status\n\n\
         查看：/cron list   停止：让 agent 调用 /cron remove <id>"
            .to_owned()
    } else {
        "/loop <interval> <prompt-or-command>\n\n\
         Repeat at the given interval: <prompt> is sent as a fresh message to the\n\
         current agent and its reply is delivered back through this channel.\n\
         <prompt> can be any command from /help or plain natural-language text.\n\n\
         Interval examples: 30s, 5m, 1h, 2h30m, 1d (min 2s)\n\
         Examples:\n\
         \u{0020}\u{0020}/loop 5m check for new mail\n\
         \u{0020}\u{0020}/loop 1h /status\n\n\
         List: /cron list    Stop: ask the agent to /cron remove <id>"
            .to_owned()
    }
}

/// Help text for /watch. Localized en/zh. Lists only flags actually
/// implemented in v1 — `--jq`, `--only`, `--tee` are stretch goals
/// (see plan §Stretch); they're documented in the spec but rejected at
/// parse time today.
fn watch_help_text(lang: &str) -> String {
    if lang == "zh" {
        "/watch <源> [flags]      实时把事件推回 chat（不过 agent）\n\n\
         源类型（auto-detect 或显式前缀）：\n\
         \u{0020}\u{0020}/watch /path/to/file.log               跟踪文件（跨平台 tail -f）\n\
         \u{0020}\u{0020}/watch https://api/events              订阅 SSE 流\n\
         \u{0020}\u{0020}/watch shell tail -f x                 原生 shell\n\n\
         Flags：\n\
         \u{0020}\u{0020}--grep <regex>             仅推送匹配的事件\n\
         \u{0020}\u{0020}--event <type>             仅推送指定 SSE event 类型\n\
         \u{0020}\u{0020}--jq <expr>                jq 表达式过滤/转换（支持 `.codes[]` 数组展开）\n\
         \u{0020}\u{0020}--template <tpl>           输出模板：`${{.field}}` 取 JSON 字段\n\
         \u{0020}\u{0020}--rate <ms>                限流（默认 2000；0 = 不限）\n\
         \u{0020}\u{0020}-H 'Header: value'         SSE auth/header；value 可含 ${VAR}\n\n\
         管理：\n\
         \u{0020}\u{0020}/watch list                列出当前活跃 watch\n\
         \u{0020}\u{0020}/watch stop <id>           停一个\n\
         \u{0020}\u{0020}/watch stop all            全停\n\n\
         持久化：本身不持久（重启即清）；要跨重启用 /loop 10m /watch <源>。"
            .to_owned()
    } else {
        "/watch <source> [flags]    Push live events back to chat (no agent involved)\n\n\
         Sources (auto-detected or explicit prefix):\n\
         \u{0020}\u{0020}/watch /path/to/file.log              follow file (cross-platform tail -f)\n\
         \u{0020}\u{0020}/watch https://api/events             subscribe SSE\n\
         \u{0020}\u{0020}/watch shell tail -f x                raw shell\n\n\
         Flags:\n\
         \u{0020}\u{0020}--grep <regex>            push only matching events\n\
         \u{0020}\u{0020}--event <type>            push only the given SSE event type\n\
         \u{0020}\u{0020}--jq <expr>               jq filter/transform (supports `.codes[]` array expansion)\n\
         \u{0020}\u{0020}--template <tpl>          output template: `${{.field}}` interpolates a JSON field\n\
         \u{0020}\u{0020}--rate <ms>               rate limit (default 2000; 0 = unlimited)\n\
         \u{0020}\u{0020}-H 'Header: value'        SSE auth/header; value may contain ${VAR}\n\n\
         Management:\n\
         \u{0020}\u{0020}/watch list               list active watches\n\
         \u{0020}\u{0020}/watch stop <id>          stop one\n\
         \u{0020}\u{0020}/watch stop all           stop everything\n\n\
         Persistence: in-memory only. Cross-restart via /loop 10m /watch <source>."
            .to_owned()
    }
}

/// Help text for /task. Localized en/zh.
fn task_help_text(lang: &str) -> String {
    if lang == "zh" {
        "/task [选项] <任务描述>\n\n\
         在多轮模式下执行一项任务：agent 会反复推理、调用工具，直到任务完成或耗尽预算。\n\n\
         选项：\n\
         \u{0020}\u{0020}-n <N>       最大轮数（默认 10）\n\
         \u{0020}\u{0020}-t <时长>    超时（如 4h、30m，默认 1h）\n\n\
         例：\n\
         \u{0020}\u{0020}/task 修复登录页的 bug\n\
         \u{0020}\u{0020}/task -n 20 重构支付模块\n\
         \u{0020}\u{0020}/task -n 50 -t 4h 完整跑通新功能\n\n\
         查看进度：/status   终止：/abort   配合 /loop 定时触发：/loop 1h /task ..."
            .to_owned()
    } else {
        "/task [options] <description>\n\n\
         Run a task in multi-turn mode: the agent will reason and call tools repeatedly\n\
         until the task is complete or its budget is exhausted.\n\n\
         Options:\n\
         \u{0020}\u{0020}-n <N>       Max turns (default 10)\n\
         \u{0020}\u{0020}-t <dur>     Timeout (e.g. 4h, 30m, default 1h)\n\n\
         Examples:\n\
         \u{0020}\u{0020}/task fix the login bug\n\
         \u{0020}\u{0020}/task -n 20 refactor the payment module\n\
         \u{0020}\u{0020}/task -n 50 -t 4h finish the new feature end-to-end\n\n\
         Progress: /status   Abort: /abort   Combine with /loop: /loop 1h /task ..."
            .to_owned()
    }
}

// ---------------------------------------------------------------------------
// /btw direct LLM call — bypasses agent inbox entirely
// ---------------------------------------------------------------------------

/// Perform a direct LLM call for /btw side queries, bypassing the agent inbox.
/// Reads the agent's live status so the LLM knows what the main agent is doing.
/// Returns the response text, or None on failure.
pub(crate) async fn btw_direct_call(
    question: &str,
    live_status: &tokio::sync::RwLock<LiveStatus>,
    providers: &ProviderRegistry,
    config: &RuntimeConfig,
) -> Option<String> {
    // 1. Read live status.
    let status_block = {
        let status = live_status.read().await;
        if status.state.is_empty() || status.state == "idle" {
            String::new()
        } else {
            let elapsed = status
                .started_at
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            format!(
                "\n<main_agent_status>\nState: {}\nTask: {}\nElapsed: {}s\nRecent tools: {}\nResponse preview: {}\n</main_agent_status>",
                status.state,
                status.current_task,
                elapsed,
                status.tool_history.join(", "),
                status.text_preview,
            )
        }
    };

    // 2. Resolve model name.
    let model = config
        .agents
        .defaults
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .unwrap_or("rsclaw/rsclaw-agent-v1");

    let system = format!(
        "You are answering a quick side question (/btw). Be concise and direct. \
         You have no tools available. Answer from your general knowledge only.{}",
        status_block
    );

    let req = LlmRequest {
            fallback_models: Vec::new(),
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(question.to_owned()),
            rsclaw_hidden: None,
        }],
        tools: vec![],
        system: Some(system),
        max_tokens: Some(500),
        temperature: None,
        frequency_penalty: None,
        thinking_budget: None,
        endpoint: AgentEndpoint::Flash,
        kv_cache_mode: 0,
        session_key: None,
        system_shared: None,
        user_system: None,
        recall: None,
    };

    // 3. Create a simple failover manager (no fallbacks needed for /btw).
    let auth_order = config
        .model
        .auth
        .as_ref()
        .and_then(|a| a.order.clone())
        .unwrap_or_default();
    // /btw uses an ephemeral failover with a fresh health table — its calls
    // are one-shot bypass queries that shouldn't poison the agent's
    // long-lived chain state.
    let mut failover = FailoverManager::new(
        auth_order,
        std::collections::HashMap::new(),
        vec![],
        crate::provider::health::ProviderHealthRegistry::new(),
    );

    let mut stream = match failover.call(req, providers).await {
        Ok(s) => s,
        Err(e) => {
            warn!("/btw direct LLM call failed: {e:#}");
            return None;
        }
    };

    let mut text_buf = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(d)) => text_buf.push_str(&d),
            Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
            Ok(_) => {}
            Err(e) => {
                warn!("/btw stream error: {e:#}");
                break;
            }
        }
    }

    if text_buf.is_empty() {
        None
    } else {
        // Strip any residual <think>...</think> tags
        let cleaned = crate::provider::openai::strip_think_tags_pub(&text_buf);
        Some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_interval_ms_basic_units() {
        assert_eq!(parse_interval_ms("30s"), Some(30_000));
        assert_eq!(parse_interval_ms("5m"), Some(300_000));
        assert_eq!(parse_interval_ms("1h"), Some(3_600_000));
        assert_eq!(parse_interval_ms("1d"), Some(86_400_000));
    }

    #[test]
    fn parse_interval_ms_compound() {
        assert_eq!(parse_interval_ms("2h30m"), Some(9_000_000));
        assert_eq!(parse_interval_ms("1h30m15s"), Some(5_415_000));
        assert_eq!(parse_interval_ms("1d2h"), Some(93_600_000));
    }

    #[test]
    fn parse_interval_ms_bare_number_is_seconds() {
        assert_eq!(parse_interval_ms("300"), Some(300_000));
    }

    #[test]
    fn parse_interval_ms_case_insensitive() {
        assert_eq!(parse_interval_ms("5M"), Some(300_000));
        assert_eq!(parse_interval_ms("1H30M"), Some(5_400_000));
    }

    #[test]
    fn parse_interval_ms_rejects_garbage() {
        assert_eq!(parse_interval_ms(""), None);
        assert_eq!(parse_interval_ms("m"), None);
        assert_eq!(parse_interval_ms("5x"), None);
        assert_eq!(parse_interval_ms("abc"), None);
    }

    #[test]
    fn format_interval_ms_drops_zero_components() {
        assert_eq!(format_interval_ms(300_000), "5m");
        assert_eq!(format_interval_ms(9_000_000), "2h30m");
        assert_eq!(format_interval_ms(86_400_000), "1d");
        assert_eq!(format_interval_ms(0), "0s");
    }

    #[test]
    fn is_fast_preparse_recognizes_loop_and_task_help() {
        assert!(is_fast_preparse("/loop"));
        assert!(is_fast_preparse("/loop 5m foo"));
        assert!(is_fast_preparse("/task"));
        assert!(is_fast_preparse("/task -h"));
        assert!(is_fast_preparse("/task --help"));
        // Real /task usage must NOT bypass the queue — task_queue owns that flow.
        assert!(!is_fast_preparse("/task fix the bug"));
        assert!(!is_fast_preparse("/task --turns 20 do something"));
    }

    #[test]
    fn is_fast_preparse_recognizes_cap_sticky_commands() {
        // Bare /cap, /cap-exit, and `/cap <agent>` must short-circuit
        // through preparse so the binding is registered immediately
        // instead of being queued behind the LLM. Without these
        // entries, the channel inbound code would treat them as plain
        // text and the sticky bind never happens.
        assert!(is_fast_preparse("/cap"));
        assert!(is_fast_preparse("/cap-exit"));
        assert!(is_fast_preparse("/cap claudecode"));
        assert!(is_fast_preparse("/cap codex"));
        assert!(is_fast_preparse("/CAP claudecode")); // case-insensitive
        // Plain text not starting with /cap remains queue-bound; the
        // sticky bypass kicks in inside AgentRuntime::run_turn, not
        // here.
        assert!(!is_fast_preparse("cap something"));
        assert!(!is_fast_preparse("just chatting"));
    }

    // -----------------------------------------------------------------
    // /plugin slash command parser tests (Task 4)
    // -----------------------------------------------------------------

    #[test]
    fn parse_plugin_command_status_bare() {
        assert_eq!(parse_plugin_command("/plugin"), Some(PluginCommand::Status));
        assert_eq!(parse_plugin_command("/plugin "), Some(PluginCommand::Status));
    }

    #[test]
    fn parse_plugin_command_info_single_arg() {
        assert_eq!(
            parse_plugin_command("/plugin douyin"),
            Some(PluginCommand::Info {
                plugin: "douyin".to_owned()
            })
        );
    }

    #[test]
    fn parse_plugin_command_off() {
        assert_eq!(
            parse_plugin_command("/plugin douyin off"),
            Some(PluginCommand::SetState {
                plugin: "douyin".to_owned(),
                action: PluginAction::Off
            })
        );
    }

    #[test]
    fn parse_plugin_command_on_or_default() {
        let expected = Some(PluginCommand::SetState {
            plugin: "douyin".to_owned(),
            action: PluginAction::Default,
        });
        assert_eq!(parse_plugin_command("/plugin douyin on"), expected);
        assert_eq!(parse_plugin_command("/plugin douyin default"), expected);
    }

    #[test]
    fn parse_plugin_command_all() {
        assert_eq!(
            parse_plugin_command("/plugin douyin all"),
            Some(PluginCommand::SetState {
                plugin: "douyin".to_owned(),
                action: PluginAction::All
            })
        );
    }

    #[test]
    fn parse_plugin_command_inject_list() {
        assert_eq!(
            parse_plugin_command("/plugin douyin publish_video,check_comments"),
            Some(PluginCommand::SetState {
                plugin: "douyin".to_owned(),
                action: PluginAction::Inject(vec![
                    "publish_video".into(),
                    "check_comments".into()
                ]),
            })
        );
    }

    #[test]
    fn parse_plugin_command_reset() {
        assert_eq!(parse_plugin_command("/plugin reset"), Some(PluginCommand::Reset));
    }

    #[test]
    fn parse_plugin_command_rejects_malformed() {
        assert_eq!(parse_plugin_command("/plugin douyin ,,, "), None);
        assert_eq!(parse_plugin_command("not-a-plugin-cmd"), None);
    }

    #[test]
    fn parse_plugin_command_list_aliases() {
        assert_eq!(parse_plugin_command("/plugin list"), Some(PluginCommand::Status));
        assert_eq!(parse_plugin_command("/plugin ls"), Some(PluginCommand::Status));
    }

    #[test]
    fn parse_plugin_command_info_aliases() {
        let expected = Some(PluginCommand::Info {
            plugin: "douyin".to_owned(),
        });
        assert_eq!(parse_plugin_command("/plugin info douyin"), expected);
        assert_eq!(parse_plugin_command("/plugin show douyin"), expected);
        // Shorthand still works.
        assert_eq!(parse_plugin_command("/plugin douyin"), expected);
    }

    #[test]
    fn parse_plugin_command_help_returns_none() {
        // `None` here means "caller falls through to plugin_help_text()".
        assert_eq!(parse_plugin_command("/plugin help"), None);
        assert_eq!(parse_plugin_command("/plugin -h"), None);
        assert_eq!(parse_plugin_command("/plugin --help"), None);
        assert_eq!(parse_plugin_command("/plugin ?"), None);
        // `/plugin info` with no name → also help (no plugin to show).
        assert_eq!(parse_plugin_command("/plugin info"), None);
        assert_eq!(parse_plugin_command("/plugin show"), None);
    }

    #[test]
    fn is_fast_preparse_recognizes_plugin() {
        assert!(is_fast_preparse("/plugin"));
        assert!(is_fast_preparse("/plugin douyin"));
        assert!(is_fast_preparse("/plugin douyin off"));
        assert!(is_fast_preparse("/plugin reset"));
    }

    #[test]
    fn parse_plugin_command_pin_canonical_form() {
        assert_eq!(
            parse_plugin_command("/plugin pin douyin__publish"),
            Some(PluginCommand::Pin {
                plugin: "douyin".to_owned(),
                tool: "publish".to_owned(),
            })
        );
    }

    #[test]
    fn parse_plugin_command_pin_legacy_dot_form() {
        // Backward compat with the old `model.plugin_tools` config format.
        assert_eq!(
            parse_plugin_command("/plugin pin douyin.publish"),
            Some(PluginCommand::Pin {
                plugin: "douyin".to_owned(),
                tool: "publish".to_owned(),
            })
        );
    }

    #[test]
    fn parse_plugin_command_unpin() {
        assert_eq!(
            parse_plugin_command("/plugin unpin douyin__delete_video"),
            Some(PluginCommand::Unpin {
                plugin: "douyin".to_owned(),
                tool: "delete_video".to_owned(),
            })
        );
    }

    #[test]
    fn parse_plugin_command_pin_unpin_require_argument() {
        // `/plugin pin` / `/plugin unpin` with no qualified tool → help.
        assert_eq!(parse_plugin_command("/plugin pin"), None);
        assert_eq!(parse_plugin_command("/plugin unpin"), None);
        // Argument without a separator is malformed.
        assert_eq!(parse_plugin_command("/plugin pin publish"), None);
    }

    #[test]
    fn parse_plugin_command_headlines() {
        assert_eq!(
            parse_plugin_command("/plugin headlines douyin"),
            Some(PluginCommand::Headlines {
                plugin: "douyin".to_owned()
            })
        );
        // No plugin → help.
        assert_eq!(parse_plugin_command("/plugin headlines"), None);
    }
}
