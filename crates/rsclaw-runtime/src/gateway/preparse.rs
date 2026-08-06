//! Fast preparse — local slash-command handling that bypasses the agent queue.
//!
//! Functions here handle commands like `/ls`, `/status`, `/version`, `/btw`
//! without going through the LLM agent loop.

use std::sync::Arc;

use futures::StreamExt as _;
use rsclaw_agent::{
    LiveStatus,
    runtime::{AgentRuntime, PluginOverride},
};
use rsclaw_channel::OutboundMessage;
use rsclaw_config::{runtime::RuntimeConfig, schema::DmScope};
use rsclaw_provider::{
    AgentEndpoint, LlmRequest, Message, MessageContent, Role, StreamEvent,
    failover::FailoverManager, registry::ProviderRegistry,
};
use tracing::warn;

use crate::gateway::session::{MessageKind, SessionKeyParams, derive_session_key};

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
    SetState {
        plugin: String,
        action: PluginAction,
    },
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
/// - `/plugin info` (no name), `/plugin help`, `/plugin -h`, `/plugin --help` →
///   None (caller shows help)
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
        let Some((plugin, tool)) = rsclaw_agent::runtime::parse_qualified_tool(qualified) else {
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
    let plugin_dir = rsclaw_config::loader::base_dir()
        .join("plugins")
        .join(plugin);
    if !plugin_dir.exists() {
        return format!("plugin `{plugin}` not installed (no dir at ~/.rsclaw/plugins/{plugin})");
    }
    let manifest = match rsclaw_plugin::manifest::load_manifest(&plugin_dir) {
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
    handle: &rsclaw_agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    account: Option<&str>,
) -> String {
    derive_session_key(&SessionKeyParams {
        agent_id: handle.id.clone(),
        channel: channel.to_owned(),
        peer_id: peer_id.to_owned(),
        kind: MessageKind::DirectMessage {
            account_id: account.map(str::to_owned),
        },
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
    handle: &rsclaw_agent::AgentHandle,
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
    handle: &rsclaw_agent::AgentHandle,
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
        let base = rsclaw_config::loader::base_dir();
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
        return Some(txt(help_text(rsclaw_i18n::default_lang())));
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

    if let Some(reply) = try_plugin_slash(
        t,
        handle,
        channel,
        peer_id,
        account,
        &this_session_key,
        origin,
    )
    .await
    {
        return Some(reply);
    }

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
        return Some(txt(rsclaw_i18n::t(
            "session_cleared",
            rsclaw_i18n::default_lang(),
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
        return Some(txt(rsclaw_i18n::t(
            "session_new",
            rsclaw_i18n::default_lang(),
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
        return Some(txt(rsclaw_i18n::t("cap_help", rsclaw_i18n::default_lang())));
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
        let lang = rsclaw_i18n::default_lang();
        let Some(kind) = rsclaw_cap::AgentKind::from_str(&agent_str.to_lowercase()) else {
            return Some(txt(rsclaw_i18n::t_fmt(
                "cap_unknown_agent",
                lang,
                &[("agent", agent_str)],
            )));
        };
        let Some(manager) = rsclaw_cap::GLOBAL_CAP_LIVE.get() else {
            return Some(txt(rsclaw_i18n::t("cap_not_initialised", lang)));
        };
        let cwd = match resolve_cap_workspace(path_arg, workspace()) {
            Ok(p) => p,
            Err(reason) => {
                return Some(txt(rsclaw_i18n::t_fmt(
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
        // Snapshot the currently-loaded plugins + installed skills so the
        // freshly-written AGENTS.md lists the exact capabilities (names +
        // call syntax) available to the coding agent right now.
        let cap_plugins: Vec<String> = handle
            .wasm_plugins_snapshot()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let cap_skills: Vec<String> = {
            let mut names = Vec::new();
            let base = rsclaw_config::loader::base_dir();
            for dir in [base.join("skills"), workspace().join("skills")] {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for e in entries.flatten() {
                        let p = e.path();
                        if p.is_dir() && p.join("SKILL.md").exists() {
                            if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                                names.push(n.to_owned());
                            }
                        }
                    }
                }
            }
            names.sort();
            names.dedup();
            names
        };
        let _ = rsclaw_cap::identity::write_identity_files(&cwd, &cap_plugins, &cap_skills).await;
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
                let reply = rsclaw_i18n::t_fmt(
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
                    rsclaw_i18n::t_fmt(
                        "cap_install_hint",
                        lang,
                        &[("agent", kind.display_name()), ("cmd", kind.as_str())],
                    )
                } else {
                    rsclaw_i18n::t_fmt("cap_open_failed", lang, &[("err", &err)])
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
        let lang = rsclaw_i18n::default_lang();
        // Parse "<agent> [session_id]" from the original (non-lowered)
        // text because session ids are case-sensitive on opencode.
        // Empty session_id → "continue last" mode (CLI-native flag).
        let rest = t.get("/cap-resume ".len()..).unwrap_or("").trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let agent_str = parts.next().unwrap_or("").trim();
        let session_id = parts.next().unwrap_or("").trim();
        if agent_str.is_empty() {
            return Some(txt(rsclaw_i18n::t("cap_resume_help", lang)));
        }
        let Some(kind) = rsclaw_cap::AgentKind::from_str(&agent_str.to_lowercase()) else {
            return Some(txt(rsclaw_i18n::t_fmt(
                "cap_unknown_agent",
                lang,
                &[("agent", agent_str)],
            )));
        };
        let Some(manager) = rsclaw_cap::GLOBAL_CAP_LIVE.get() else {
            return Some(txt(rsclaw_i18n::t("cap_not_initialised", lang)));
        };
        let cwd = workspace();
        let spawn_result = if session_id.is_empty() {
            manager.open_session_continue_last(kind, cwd).await
        } else {
            manager
                .open_session_resume(kind, cwd, session_id.to_owned())
                .await
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
                return Some(txt(rsclaw_i18n::t_fmt(
                    "cap_resumed",
                    lang,
                    &[("agent", kind.display_name()), ("session_id", label)],
                )));
            }
            Err(e) => {
                let err = e.to_string();
                let is_binary_missing = err.contains("binary not found on PATH");
                let hint = if is_binary_missing {
                    rsclaw_i18n::t_fmt(
                        "cap_install_hint",
                        lang,
                        &[("agent", kind.display_name()), ("cmd", kind.as_str())],
                    )
                } else {
                    rsclaw_i18n::t_fmt("cap_open_failed", lang, &[("err", &err)])
                };
                return Some(txt(hint));
            }
        }
    }
    // /cap-exit — release any sticky binding on this IM session.
    if lower == "/cap-exit" {
        let lang = rsclaw_i18n::default_lang();
        let Some(manager) = rsclaw_cap::GLOBAL_CAP_LIVE.get() else {
            return Some(txt(rsclaw_i18n::t("cap_not_initialised", lang)));
        };
        let Some((sid, kind)) = manager.unbind_sticky(&this_session_key).await else {
            return Some(txt(rsclaw_i18n::t("cap_no_active", lang)));
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
        let closed = rsclaw_i18n::t_fmt(
            "cap_session_closed",
            lang,
            &[("agent", kind.display_name())],
        );
        let body = if let Some(nsid) = native_sid {
            let hint = rsclaw_i18n::t_fmt(
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
        return Some(txt(rsclaw_i18n::t(
            "screenshot_failed",
            rsclaw_i18n::default_lang(),
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
        return Some(txt(rsclaw_i18n::t_fmt(
            "webshot_failed",
            rsclaw_i18n::default_lang(),
            &[("url", &url)],
        )));
    }
    // /skill list — list installed skills (system + agent workspace)
    if lower == "/skill list" {
        let base = rsclaw_config::loader::base_dir();
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
        let jobs_path = rsclaw_config::loader::base_dir().join("cron.json5");
        let jobs = rsclaw_agent::tools_cron::read_cron_jobs(&jobs_path).await;
        let reply = rsclaw_agent::tools_cron::format_cron_jobs(&jobs);
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
        let mut jobs = rsclaw_agent::tools_cron::read_cron_jobs(&cron_path).await;
        let zh = rsclaw_i18n::default_lang() == "zh";
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
        if let Err(e) = rsclaw_agent::tools_cron::write_cron_jobs(&cron_path, &jobs).await {
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
        return Some(txt(loop_help_text(rsclaw_i18n::default_lang())));
    }
    if lower.starts_with("/loop ") {
        let rest = t.get(6..).unwrap_or("").trim();
        let (interval_s, prompt) = match rest.split_once(char::is_whitespace) {
            Some((iv, pr)) => (iv.trim(), pr.trim()),
            None => (rest, ""),
        };
        if interval_s.is_empty() || prompt.is_empty() {
            return Some(txt(loop_help_text(rsclaw_i18n::default_lang())));
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
        // Inherit the originating session so each firing continues the same
        // conversation history instead of starting a blank session.
        let inherited_session_key = preparse_session_key(handle, channel, peer_id, account);
        let job = serde_json::json!({
            "id": id,
            "agentId": handle.id,
            "enabled": true,
            "sessionKey": inherited_session_key,
            "schedule": {"kind": "every", "everyMs": every_ms, "anchorMs": now_ms},
            "payload": {"kind": "agentTurn", "text": prompt},
            "delivery": {"channel": delivery_channel, "to": peer_id, "mode": "always"},
            "createdAtMs": now_ms,
        });
        let cron_path = crate::cron::resolve_cron_store_path();
        let _guard = crate::cron::CRON_FILE_LOCK.lock().await;
        let mut jobs = rsclaw_agent::tools_cron::read_cron_jobs(&cron_path).await;
        jobs.push(job);
        if let Err(e) = rsclaw_agent::tools_cron::write_cron_jobs(&cron_path, &jobs).await {
            return Some(txt(format!("/loop: failed to save jobs: {e}")));
        }
        drop(_guard);
        crate::cron::trigger_reload();
        let zh = rsclaw_i18n::default_lang() == "zh";
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
        return Some(txt(watch_help_text(rsclaw_i18n::default_lang())));
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
        return Some(txt(task_help_text(rsclaw_i18n::default_lang())));
    }
    // /model — show current model; /models — list providers; /model <name> — switch
    if lower == "/model" || lower == "/models" {
        // Single source of truth shared with the agent-queue __MODELS__ path.
        return Some(txt(handle.format_models()));
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
                let resolved: Result<PluginOverride, String> = match action {
                    PluginAction::Off => Ok(PluginOverride {
                        disabled: true,
                        ..Default::default()
                    }),
                    PluginAction::Default => Ok(PluginOverride::default()),
                    PluginAction::All => Ok(PluginOverride {
                        inject_all: true,
                        ..Default::default()
                    }),
                    PluginAction::Inject(tools) => {
                        // v2 toolGroups: `@<group>` entries expand to the
                        // group's member tools from the on-disk manifest
                        // (same source-of-truth rationale as
                        // render_headlines_for_plugin).
                        let mut expanded: Vec<String> = Vec::new();
                        let mut manifest: Option<rsclaw_plugin::manifest::PluginManifest> = None;
                        let mut err: Option<String> = None;
                        for spec in tools {
                            let Some(group) = spec.strip_prefix('@') else {
                                expanded.push(spec);
                                continue;
                            };
                            if manifest.is_none() {
                                let dir = rsclaw_config::loader::base_dir()
                                    .join("plugins")
                                    .join(&plugin);
                                manifest = rsclaw_plugin::manifest::load_manifest(&dir).ok();
                            }
                            match &manifest {
                                Some(m) => {
                                    let members: Vec<String> = m
                                        .tools
                                        .iter()
                                        .filter(|t| t.group.as_deref() == Some(group))
                                        .map(|t| t.name.clone())
                                        .collect();
                                    if members.is_empty() {
                                        err = Some(format!(
                                            "{plugin}: group `@{group}` matches no tools \
                                             (declare via the tool's `group` field in plugin.json5)"
                                        ));
                                        break;
                                    }
                                    expanded.extend(members);
                                }
                                None => {
                                    err = Some(format!(
                                        "{plugin}: cannot resolve `@{group}` — manifest not readable"
                                    ));
                                    break;
                                }
                            }
                        }
                        match err {
                            Some(e) => Err(e),
                            None => Ok(PluginOverride {
                                inject: expanded,
                                ..Default::default()
                            }),
                        }
                    }
                };
                match resolved {
                    Err(msg) => msg,
                    Ok(new_override) => {
                        let summary = if new_override.disabled {
                            format!("{plugin}: OFF")
                        } else if new_override.inject_all {
                            format!("{plugin}: inject ALL (capped at user_tools_cap)")
                        } else if new_override.inject.is_empty() {
                            format!("{plugin}: default")
                        } else {
                            format!("{plugin}: inject [{}]", new_override.inject.join(", "))
                        };
                        AgentRuntime::set_plugin_override(
                            handle,
                            &session_key,
                            &plugin,
                            new_override,
                        );
                        summary
                    }
                }
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
        return Some(txt(goal_set_handler(
            handle, channel, peer_id, account, rest,
        )
        .await));
    }
    None
}

// ---------------------------------------------------------------------------
// /goal handlers — completion-driven turn loop.
// ---------------------------------------------------------------------------
//
// The goal LOOP itself lives in `gateway/startup.rs` (post-`run_turn`
// hook calling `rsclaw_agent::goal::check_after_turn`). The slash
// command here only manages the goal STATE (set / clear / status)
// and kicks off the first turn via `submit_to_queue`.

async fn goal_set_handler(
    handle: &rsclaw_agent::AgentHandle,
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
    let Some(mem) = rsclaw_agent::memory::global_store() else {
        return "_memory 未启用,/goal 无法持久化_".to_owned();
    };
    let session_key = preparse_session_key(handle, channel, peer_id, account);
    if let Err(e) = rsclaw_agent::goal::set(&mem, &session_key, &condition, max_iter).await {
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
    let prompt = rsclaw_agent::goal::build_initial_prompt(&condition, max_iter);
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
    handle: &rsclaw_agent::AgentHandle,
    channel: &str,
    peer_id: &str,
) -> String {
    let Some(mem) = rsclaw_agent::memory::global_store() else {
        return "_memory 未启用_".to_owned();
    };
    let session_key = preparse_session_key(handle, channel, peer_id, None);
    let active = rsclaw_agent::goal::read(&mem, &session_key).await;
    if let Err(e) = rsclaw_agent::goal::clear(&mem, &session_key).await {
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
    handle: &rsclaw_agent::AgentHandle,
    channel: &str,
    peer_id: &str,
) -> String {
    let Some(mem) = rsclaw_agent::memory::global_store() else {
        return "_memory 未启用_".to_owned();
    };
    let session_key = preparse_session_key(handle, channel, peer_id, None);
    match rsclaw_agent::goal::read(&mem, &session_key).await {
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
    let mut max_iter: u32 = rsclaw_agent::goal::DEFAULT_MAX_ITER;
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
            | "/goal"
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
    || lower.starts_with("/goal ")
    || is_installed_plugin_slash_preparse(t)
    // /task only short-circuits on help variants; non-help forms must NOT
    // bypass the queue (the task queue worker owns the multi-turn flow).
    || lower == "/task -h"
    || lower == "/task --help"
    || lower == "/task help"
    || t.starts_with("! ")
    || t.starts_with("$ ")
}

fn is_installed_plugin_slash_preparse(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return false;
    }
    let plugins_dir = rsclaw_config::loader::base_dir().join("plugins");
    installed_plugin_slash_matches_in_dir(&plugins_dir, trimmed)
}

fn installed_plugin_slash_matches_in_dir(plugins_dir: &std::path::Path, text: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(plugins_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            return false;
        };
        if !file_type.is_dir() {
            return false;
        }
        let Ok(manifest) = rsclaw_plugin::manifest::load_manifest(&path) else {
            return false;
        };
        manifest
            .slash_commands
            .iter()
            .any(|command| slash_prefix_matches(text, &command.prefix))
    })
}

fn slash_prefix_matches(text: &str, prefix: &str) -> bool {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return false;
    }
    let text_lower = text.trim().to_lowercase();
    let prefix_lower = prefix.to_lowercase();
    text_lower == prefix_lower || text_lower.starts_with(&format!("{prefix_lower} "))
}

async fn try_plugin_slash(
    text: &str,
    handle: &rsclaw_agent::AgentHandle,
    channel: &str,
    peer_id: &str,
    account: Option<&str>,
    session_key: &str,
    origin: PreparseOrigin,
) -> Option<OutboundMessage> {
    let plugins = handle.wasm_plugins_snapshot();
    if plugins.is_empty() {
        return None;
    }
    let lower = text.trim().to_lowercase();
    for plugin in plugins.iter() {
        for command in &plugin.slash_commands {
            let prefix = command.prefix.trim();
            if prefix.is_empty() {
                continue;
            }
            if !slash_prefix_matches(&lower, prefix) {
                continue;
            }
            let Some(tx) = handle.notification_tx() else {
                return Some(plugin_slash_error(format!(
                    "plugin slash `{prefix}` cannot run: notification host is unavailable"
                )));
            };
            let args = serde_json::json!({
                "text": text,
                "prefix": prefix,
                "args": slash_args_after_prefix(text, prefix),
                "channel": channel,
                "peer_id": peer_id,
                "account": account,
                "session_key": session_key,
                "origin": match origin {
                    PreparseOrigin::User => "user",
                    PreparseOrigin::Cron => "cron",
                },
            });
            let notify_ctx = Some(rsclaw_plugin::wasm_runtime::WasmNotifyCtx {
                tx,
                target_id: peer_id.to_owned(),
                channel: channel.to_owned(),
                agent_id: handle.id.clone(),
                peer_id: peer_id.to_owned(),
                chat_id: peer_id.to_owned(),
                session_key: session_key.to_owned(),
                is_group: false,
                account: account.map(str::to_owned),
            });
            return Some(
                match plugin
                    .call_tool_with_ctx(&command.handler, args, notify_ctx)
                    .await
                {
                    Ok(value) => plugin_slash_outbound(value),
                    Err(e) => plugin_slash_error(format!("plugin slash `{prefix}` failed: {e:#}")),
                },
            );
        }
    }
    None
}

fn slash_args_after_prefix(text: &str, prefix: &str) -> String {
    text.trim()
        .chars()
        .skip(prefix.chars().count())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn plugin_slash_error(text: String) -> OutboundMessage {
    OutboundMessage {
        target_id: String::new(),
        is_group: false,
        text,
        reply_to: None,
        images: vec![],
        files: vec![],
        channel: None,
        account: None,
    }
}

fn plugin_slash_outbound(value: serde_json::Value) -> OutboundMessage {
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        });
    let values_to_strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let values_to_files = || -> Vec<(String, String, String)> {
        value
            .get("files")
            .and_then(serde_json::Value::as_array)
            .map(|arr| arr.iter().filter_map(plugin_file_tuple).collect())
            .unwrap_or_default()
    };
    OutboundMessage {
        target_id: String::new(),
        is_group: false,
        text,
        reply_to: None,
        images: values_to_strings("images"),
        files: values_to_files(),
        channel: None,
        account: None,
    }
}

fn plugin_file_tuple(value: &serde_json::Value) -> Option<(String, String, String)> {
    let (path, filename, mime) = if let Some(path) = value.as_str() {
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin-file")
            .to_owned();
        (
            path.to_owned(),
            filename,
            "application/octet-stream".to_owned(),
        )
    } else {
        let obj = value.as_object()?;
        let path = obj
            .get("path")
            .and_then(serde_json::Value::as_str)?
            .to_owned();
        let filename = obj
            .get("filename")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("plugin-file")
                    .to_owned()
            });
        let mime = obj
            .get("mime")
            .or_else(|| obj.get("mimeType"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_owned();
        (path, filename, mime)
    };
    Some((filename, mime, path))
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
    let canon =
        std::fs::canonicalize(&expanded).map_err(|e| format!("path not accessible ({e})"))?;
    if !canon.is_dir() {
        return Err("not a directory".to_string());
    }
    let home = dirs_next::home_dir().ok_or_else(|| "HOME not available".to_string())?;
    let home_canon = std::fs::canonicalize(&home).unwrap_or(home);
    if !canon.starts_with(&home_canon) {
        return Err(format!("path must be under {}", home_canon.display()));
    }
    Ok(canon)
}

fn spawn_resume_hint_followup(
    manager: std::sync::Arc<rsclaw_cap::CapLiveManager>,
    cap_sid: String,
    kind: rsclaw_cap::AgentKind,
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
        let text = rsclaw_i18n::t_fmt(
            "cap_resume_hint",
            lang,
            &[("agent", kind.as_str()), ("session_id", &nsid)],
        );
        let msg = rsclaw_channel::OutboundMessage {
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
         文件/截图\n\
         \u{0020}\u{0020}/ls [path]       列出工作区目录\n\
         \u{0020}\u{0020}/cat <file>      查看文件内容\n\
         \u{0020}\u{0020}/ss              桌面截图\n\
         \u{0020}\u{0020}/webshot <url>   网页截图\n\n\
         技能/插件\n\
         \u{0020}\u{0020}/skill list      已安装技能\n\n\
         \u{0020}\u{0020}/plugin list     插件状态；插件声明的 slash 命令会自动接管\n\n\
         编程智能体直连\n\
         \u{0020}\u{0020}/cap <agent>            绑定本会话直连 cap 子智能体\n\
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
         File / screenshot\n\
         \u{0020}\u{0020}/ls [path]       list workspace directory\n\
         \u{0020}\u{0020}/cat <file>      view file contents\n\
         \u{0020}\u{0020}/ss              desktop screenshot\n\
         \u{0020}\u{0020}/webshot <url>   web-page screenshot\n\n\
         Skills / plugins\n\
         \u{0020}\u{0020}/skill list      installed skills\n\n\
         \u{0020}\u{0020}/plugin list     plugin status; plugin-declared slash commands auto-hook\n\n\
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
    providers: &Arc<std::sync::RwLock<Arc<ProviderRegistry>>>,
    config: &RuntimeConfig,
) -> Option<String> {
    // Snapshot the provider registry (hot-swappable via rsclaw reload).
    let providers: Arc<ProviderRegistry> = providers
        .read()
        .map(|g| Arc::clone(&g))
        .unwrap_or_else(|_| Arc::new(ProviderRegistry::new()));
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
    let retry_config = config
        .model
        .retry
        .as_ref()
        .map(|r| rsclaw_provider::RetryConfig {
            attempts: r.attempts.unwrap_or(3),
            min_delay_ms: r.min_delay_ms.unwrap_or(400),
            max_delay_ms: r.max_delay_ms.unwrap_or(30_000),
            jitter: r.jitter.unwrap_or(0.1),
        })
        .unwrap_or_default();
    let mut failover = FailoverManager::new(
        auth_order,
        std::collections::HashMap::new(),
        vec![],
        rsclaw_provider::health::ProviderHealthRegistry::new(),
        retry_config,
    );

    let mut stream = match failover.call(req, &providers).await {
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
        let cleaned = rsclaw_provider::openai::strip_think_tags_pub(&text_buf);
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
        assert_eq!(
            parse_plugin_command("/plugin "),
            Some(PluginCommand::Status)
        );
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
                action: PluginAction::Inject(vec!["publish_video".into(), "check_comments".into()]),
            })
        );
    }

    #[test]
    fn parse_plugin_command_reset() {
        assert_eq!(
            parse_plugin_command("/plugin reset"),
            Some(PluginCommand::Reset)
        );
    }

    #[test]
    fn parse_plugin_command_rejects_malformed() {
        assert_eq!(parse_plugin_command("/plugin douyin ,,, "), None);
        assert_eq!(parse_plugin_command("not-a-plugin-cmd"), None);
    }

    #[test]
    fn parse_plugin_command_list_aliases() {
        assert_eq!(
            parse_plugin_command("/plugin list"),
            Some(PluginCommand::Status)
        );
        assert_eq!(
            parse_plugin_command("/plugin ls"),
            Some(PluginCommand::Status)
        );
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
