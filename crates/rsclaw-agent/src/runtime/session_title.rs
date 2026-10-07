//! Session titles: auto title from the first user message, optional flash-LLM
//! refinement after the first reply, and the channel peer display name.
//!
//! Only session METADATA is written here; persisted message text (with its
//! injected context prefix) is never touched.

use std::{sync::Arc, time::Duration};

use rsclaw_provider::registry::ProviderRegistry;
use rsclaw_store::redb_store::RedbStore;
use rsclaw_util::session_text::{derive_title, sanitize_llm_title, truncate_chars};
use tracing::{debug, warn};

use super::{AgentRuntime, is_internal_session};

/// Upper bound for the whole background title request.
const LLM_TITLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Chars of the user message / reply fed to the title model.
const LLM_TITLE_INPUT_CHARS: usize = 1000;

impl AgentRuntime {
    /// Record session metadata right after the user message is persisted:
    /// the auto title on the first user message of a conversation and the
    /// channel peer's display name. `user_text` is the ORIGINAL user text
    /// (before the context prefix is added).
    pub(crate) fn record_session_metadata(
        &self,
        session_key: &str,
        user_text: &str,
        first_user_msg: bool,
        channel: &str,
        peer_id: &str,
    ) {
        if is_internal_session(session_key) {
            return;
        }
        if first_user_msg && let Some(title) = derive_title(user_text) {
            if let Err(e) = self.store.db.set_auto_title_if_absent(session_key, &title) {
                warn!(session = %session_key, "failed to set auto session title: {e:#}");
            }
        }
        // Group sessions are shared by many senders: no single peer name.
        if !session_key.contains(":group:")
            && let Some(name) = rsclaw_channel::peer_names::peer_name(channel, peer_id)
            && let Err(e) = self.store.db.set_peer_name_if_changed(session_key, &name)
        {
            warn!(session = %session_key, "failed to record peer name: {e:#}");
        }
    }

    /// After the first reply of a conversation, refine the auto title with
    /// the flash model in the background. No-op for internal sessions, when
    /// disabled via `agents.defaults.sessionTitles.llm`, or when the user
    /// has already named the session. Never blocks the turn.
    pub(crate) async fn spawn_llm_session_title(
        &self,
        session_key: &str,
        user_text: &str,
        reply_text: &str,
    ) {
        if is_internal_session(session_key) || reply_text.trim().is_empty() {
            return;
        }
        let enabled = self
            .live
            .agents
            .read()
            .await
            .defaults
            .session_titles
            .as_ref()
            .is_none_or(|c| c.llm);
        if !enabled {
            return;
        }
        let generation = match self.store.db.get_session_meta(session_key) {
            Ok(Some(meta)) if !meta.has_user_title() => meta.generation,
            Ok(_) => return,
            Err(e) => {
                warn!(session = %session_key, "session title: meta read failed: {e:#}");
                return;
            }
        };
        let flash_model = self.resolve_flash_model_name();
        if flash_model.is_empty() {
            return;
        }
        let providers = Arc::clone(&self.providers);
        let db = Arc::clone(&self.store.db);
        let session_key = session_key.to_owned();
        let user_text = truncate_chars(user_text, LLM_TITLE_INPUT_CHARS).to_owned();
        let reply_text = truncate_chars(reply_text, LLM_TITLE_INPUT_CHARS).to_owned();
        tokio::spawn(async move {
            let fut = llm_title(&providers, &flash_model, &user_text, &reply_text);
            let title = match tokio::time::timeout(LLM_TITLE_TIMEOUT, fut).await {
                Ok(Some(t)) => t,
                Ok(None) => return,
                Err(_) => {
                    warn!(session = %session_key, "session title: flash call timed out");
                    return;
                }
            };
            store_llm_title(&db, &session_key, &title, generation);
        });
    }
}

fn store_llm_title(db: &RedbStore, session_key: &str, title: &str, generation: u32) {
    match db.set_auto_title_for_generation(session_key, title, generation) {
        Ok(true) => debug!(session = %session_key, title, "session title refined"),
        Ok(false) => {}
        Err(e) => warn!(session = %session_key, "session title: store failed: {e:#}"),
    }
}

/// One-shot flash completion producing a short conversation title.
async fn llm_title(
    providers: &ProviderRegistry,
    flash_model: &str,
    user_text: &str,
    reply_text: &str,
) -> Option<String> {
    use futures::StreamExt;
    use rsclaw_provider::{AgentEndpoint, LlmRequest, Message, MessageContent, Role, StreamEvent};

    let (provider_name, model_id) = providers.resolve_model(flash_model);
    let provider = match providers.get(provider_name) {
        Ok(p) => p,
        Err(e) => {
            warn!("session title: flash provider unavailable: {e:#}");
            return None;
        }
    };
    let req = LlmRequest {
        fallback_models: Vec::new(),
        model: model_id.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(format!(
                "Write a concise title for the conversation below.\n\
                 Rules:\n\
                 - Use the same language as the user's message.\n\
                 - At most 20 characters for Chinese/Japanese/Korean, otherwise at most 8 words.\n\
                 - No quotes, no trailing punctuation, no prefix like \"Title:\".\n\
                 - Output the title only.\n\n\
                 <user>\n{user_text}\n</user>\n\n<assistant>\n{reply_text}\n</assistant>"
            )),
            rsclaw_hidden: None,
        }],
        tools: vec![],
        system: Some("You write short, specific titles for chat conversations.".to_owned()),
        max_tokens: Some(64),
        temperature: Some(0.2),
        frequency_penalty: None,
        thinking_budget: None,
        endpoint: AgentEndpoint::Flash,
        kv_cache_mode: 0,
        session_key: None,
        system_shared: None,
        user_system: None,
        recall: None,
    };
    let mut stream = match provider.stream(req).await {
        Ok(s) => s,
        Err(e) => {
            warn!("session title: flash call failed: {e:#}");
            return None;
        }
    };
    let mut buf = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(StreamEvent::TextDelta(d)) => buf.push_str(&d),
            Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
            Ok(_) => {}
            Err(e) => {
                warn!("session title: stream error: {e:#}");
                break;
            }
        }
    }
    sanitize_llm_title(&buf)
}
