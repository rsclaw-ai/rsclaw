//! `/model <name>`: switch the model of one conversation at runtime.
//!
//! The switch belongs to the session it was typed in, so trying a model in
//! one chat never moves an agent's other chats (customer-facing ones
//! included). It lives in memory only: a restart, or `/model default`,
//! returns the session to the configured model.

use std::collections::HashMap;

use rsclaw_config::schema::ModelAlias;
use rsclaw_provider::registry::ProviderRegistry;

use crate::AgentHandle;

/// What `/model <input>` asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum ModelChoice {
    /// Use this `provider/model` key for the session.
    Set(String),
    /// Go back to the configured model.
    Reset,
}

/// Resolves what the user typed after `/model`: `default`/`reset`, a
/// configured alias (`agents.defaults.models.<key>.alias`), a full
/// `provider/model` key whose provider is registered, or a bare model id
/// that exactly one configured model has. Errors name what was not found.
pub fn resolve_model_choice(
    input: &str,
    aliases: Option<&HashMap<String, ModelAlias>>,
    providers: &ProviderRegistry,
) -> Result<ModelChoice, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("usage: /model <name>  (/model default to switch back)".to_owned());
    }
    if matches!(input.to_ascii_lowercase().as_str(), "default" | "reset" | "clear") {
        return Ok(ModelChoice::Reset);
    }
    let configured = aliases.into_iter().flatten();
    let key = if let Some((key, _)) = configured
        .clone()
        .find(|(_, a)| a.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(input)))
    {
        key.clone()
    } else if input.contains('/') {
        input.to_owned()
    } else {
        let matching: Vec<&String> = configured
            .map(|(key, _)| key)
            .filter(|key| key.split_once('/').is_some_and(|(_, id)| id.eq_ignore_ascii_case(input)))
            .collect();
        match matching.as_slice() {
            [key] => (*key).clone(),
            [] => return Err(format!("unknown model '{input}'; use an alias or provider/model")),
            several => {
                let names: Vec<&str> = several.iter().map(|k| k.as_str()).collect();
                return Err(format!("'{input}' is ambiguous: {}", names.join(", ")));
            }
        }
    };
    let (provider, _) = providers.resolve_model(&key);
    providers
        .get(provider)
        .map_err(|e| format!("cannot switch to {key}: {e}"))?;
    Ok(ModelChoice::Set(key))
}

/// Handles `/model <input>` for `session_key` and returns the reply.
pub fn switch_session_model(handle: &AgentHandle, session_key: &str, input: &str) -> String {
    let zh = rsclaw_i18n::default_lang() == "zh";
    let cfg = rsclaw_config::load_cached().ok();
    let aliases = cfg.as_ref().and_then(|c| c.agents.defaults.models.as_ref());
    match resolve_model_choice(input, aliases, &handle.providers_snapshot()) {
        Ok(ModelChoice::Set(model)) => {
            handle.set_session_model(session_key, Some(model.clone()));
            if zh {
                format!("本会话已切换到 {model}（只影响当前会话，重启后恢复；/model default 切回默认）")
            } else {
                format!(
                    "This chat now uses {model} (this chat only, until restart; /model default switches back)"
                )
            }
        }
        Ok(ModelChoice::Reset) => {
            handle.set_session_model(session_key, None);
            let model = &handle.effective_model;
            if zh {
                format!("本会话已恢复默认模型 {model}")
            } else {
                format!("This chat is back on the default model {model}")
            }
        }
        Err(e) => {
            if zh {
                format!("未切换：{e}")
            } else {
                format!("Model not switched: {e}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> HashMap<String, ModelAlias> {
        let alias = |a: &str| ModelAlias { model: None, alias: Some(a.to_owned()) };
        HashMap::from([
            ("aiart/rsclaw-agent-v2".to_owned(), alias("aiart")),
            ("aiart/rsclaw-agent-v1".to_owned(), alias("aiart-v1")),
            ("rsclaw/rsclaw-agent-v2".to_owned(), alias("agent-v2")),
            ("kimi/kimi-for-coding".to_owned(), alias("kimi")),
        ])
    }

    struct Idle;

    impl rsclaw_provider::LlmProvider for Idle {
        fn name(&self) -> &str {
            "idle"
        }

        fn stream(
            &self,
            _req: rsclaw_provider::LlmRequest,
        ) -> futures::future::BoxFuture<'_, anyhow::Result<rsclaw_provider::LlmStream>> {
            Box::pin(async { Err(anyhow::anyhow!("idle")) })
        }
    }

    fn providers() -> ProviderRegistry {
        let mut p = ProviderRegistry::new();
        p.register("aiart", std::sync::Arc::new(Idle));
        p.disable("kimi", "test stand-in");
        p
    }

    #[test]
    fn model_switch_resolves_aliases_and_bare_ids() {
        let a = aliases();
        let p = providers();
        let set = |m: &str| Ok(ModelChoice::Set(m.to_owned()));
        assert_eq!(resolve_model_choice("aiart", Some(&a), &p), set("aiart/rsclaw-agent-v2"));
        assert_eq!(resolve_model_choice("AIART-V1", Some(&a), &p), set("aiart/rsclaw-agent-v1"));
        assert_eq!(resolve_model_choice("aiart/rsclaw-agent-v2", None, &p), set("aiart/rsclaw-agent-v2"));
        assert_eq!(resolve_model_choice("default", Some(&a), &p), Ok(ModelChoice::Reset));
        assert!(resolve_model_choice("rsclaw-agent-v2", Some(&a), &p).unwrap_err().contains("ambiguous"));
        assert!(resolve_model_choice("nope", Some(&a), &p).unwrap_err().contains("unknown"));
        // A provider that isn't registered (or was disabled) is refused
        // with the registry's reason instead of a fake "switched".
        let err = resolve_model_choice("kimi", Some(&a), &p).unwrap_err();
        assert!(err.contains("kimi/kimi-for-coding") && err.contains("disabled"), "{err}");
        assert!(resolve_model_choice("agent-v2", Some(&a), &p).unwrap_err().contains("not registered"));
    }
}
