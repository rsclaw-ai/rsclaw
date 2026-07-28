//! Model resolution — stateless helpers for resolving primary / flash / vision
//! model names from per-agent + defaults config.

/// Resolve the primary model name from per-agent + defaults config.
pub fn resolve_primary_model_for(
    per_agent: &rsclaw_config::schema::AgentEntry,
    defaults: &rsclaw_config::schema::AgentDefaults,
) -> Option<String> {
    per_agent
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .or_else(|| defaults.model.as_ref().and_then(|m| m.primary_head()))
        .map(str::to_owned)
}

/// Is the agent's EFFECTIVE primary a `rsclaw/` model? Uses the config's
/// explicit primary head; when the config sets none, the runtime falls back to
/// the built-in `rsclaw/rsclaw-agent-v1` default (which IS rsclaw), so a config
/// that never writes `model.primary` still counts as rsclaw.
pub(crate) fn effective_primary_is_rsclaw(
    per_agent: &rsclaw_config::schema::AgentEntry,
    defaults: &rsclaw_config::schema::AgentDefaults,
) -> bool {
    match per_agent
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .or_else(|| defaults.model.as_ref().and_then(|m| m.primary_head()))
    {
        Some(p) => p.trim().starts_with("rsclaw/"),
        None => true,
    }
}

/// Resolve the flash (cheap/fast) model name from per-agent + defaults config.
///
/// Lookup chain:
///   1. per-agent `model.flash`
///   2. per-agent `flash_model.primary` (legacy)
///   3. `defaults.model.flash`
///   4. `defaults.flash_model.primary` (legacy)
///   5. RsClaw provider inference: if the effective primary lives under the
///      `rsclaw/` namespace, auto-pick [`RSCLAW_DEFAULT_FLASH`].
pub fn resolve_flash_model_for(
    per_agent: &rsclaw_config::schema::AgentEntry,
    defaults: &rsclaw_config::schema::AgentDefaults,
) -> Option<String> {
    let explicit = per_agent
        .model
        .as_ref()
        .and_then(|m| m.flash_head())
        .or_else(|| {
            per_agent
                .flash_model
                .as_ref()
                .and_then(|m| m.primary_head())
        })
        .or_else(|| defaults.model.as_ref().and_then(|m| m.flash_head()))
        .or_else(|| defaults.flash_model.as_ref().and_then(|m| m.primary_head()))
        .map(str::to_owned);
    if explicit.is_some() {
        return explicit;
    }

    if effective_primary_is_rsclaw(per_agent, defaults) {
        return Some(rsclaw_provider::rsclaw::RSCLAW_DEFAULT_FLASH.to_owned());
    }
    None
}

/// Outcome of vision-model resolution.
#[derive(Debug, Clone)]
pub enum VisionResolution {
    /// An explicit `model.vision` was configured (per-agent or in defaults).
    Configured(String),
    /// No `vision` set; falling back to the agent's primary model.
    FallbackToPrimary(String),
    /// Neither `vision` nor `primary` is configured anywhere.
    NoneConfigured,
}

/// Resolve the vision model for `computer_use` (and any other VLM-backed
/// path). Lookup chain:
///
///   1. per-agent `model.vision`
///   2. `defaults.model.vision`
///   3. RsClaw fleet inference (rsclaw primary → dedicated vision slot)
///   4. per-agent `model.primary`
///   5. `defaults.model.primary`
///
/// (1)-(3) return `Configured`. (4)-(5) return `FallbackToPrimary`.
/// Nothing → `NoneConfigured`.
pub fn resolve_vision_model_for(
    per_agent: &rsclaw_config::schema::AgentEntry,
    defaults: &rsclaw_config::schema::AgentDefaults,
) -> VisionResolution {
    if let Some(name) = per_agent
        .model
        .as_ref()
        .and_then(|m| m.vision_head())
        .map(str::to_owned)
    {
        return VisionResolution::Configured(name);
    }
    if let Some(name) = defaults
        .model
        .as_ref()
        .and_then(|m| m.vision_head())
        .map(str::to_owned)
    {
        return VisionResolution::Configured(name);
    }

    if effective_primary_is_rsclaw(per_agent, defaults) {
        return VisionResolution::Configured(
            rsclaw_provider::rsclaw::RSCLAW_DEFAULT_VISION.to_owned(),
        );
    }

    if let Some(name) = per_agent
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .map(str::to_owned)
    {
        return VisionResolution::FallbackToPrimary(name);
    }
    if let Some(name) = defaults
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .map(str::to_owned)
    {
        return VisionResolution::FallbackToPrimary(name);
    }
    VisionResolution::NoneConfigured
}

/// Look up `model_name` in the provider config and return whether its
/// `input` array contains `image`.
///
/// Returns:
///   - `Some(true)` — explicitly declared as image-capable.
///   - `Some(false)` — explicitly declared as text-only.
///   - `None` — no `models[].input` entry found; caller should fall back to the
///     blocklist heuristic.
pub fn model_supports_image_input(
    config: &rsclaw_config::schema::Config,
    model_name: &str,
) -> Option<bool> {
    use rsclaw_config::schema::InputType;

    let models_cfg = config.models.as_ref()?;
    let (prov_name, model_id) = match model_name.split_once('/') {
        Some((p, m)) => (Some(p), m),
        None => (None, model_name),
    };

    let probe = |entries: &Option<Vec<rsclaw_config::schema::ModelDef>>| {
        entries.as_ref().and_then(|defs| {
            defs.iter()
                .find(|d| d.id == model_id)
                .and_then(|d| d.input.as_ref())
                .map(|inputs| inputs.contains(&InputType::Image))
        })
    };

    if let Some(prov) = prov_name {
        if let Some(pc) = models_cfg.providers.get(prov) {
            if let Some(verdict) = probe(&pc.models) {
                return Some(verdict);
            }
        }
    }

    for pc in models_cfg.providers.values() {
        if let Some(verdict) = probe(&pc.models) {
            return Some(verdict);
        }
    }
    None
}

/// Heuristic substring list of model names known to be vision-capable.
/// When the schema-driven check is missing, the resolver falls back to this
/// allow-list. Models NOT in this list are treated as text-only by default.
pub fn is_known_vision_model(model: &str) -> bool {
    let m = model.to_lowercase();
    [
        // -------- universal suffixes
        "-vision",
        "-vl-",
        "-vl/",
        "-vl:",
        "-omni",
        // -------- OpenAI
        "gpt-4o",
        "gpt-4-vision",
        "gpt-4-turbo",
        "gpt-4.1",
        "gpt-5",
        "chatgpt-4o",
        "o1-",
        "o3-",
        "o4-",
        // -------- Anthropic Claude 3+
        "claude-3",
        "claude-sonnet-4",
        "claude-opus-4",
        "claude-haiku-4",
        "claude-4",
        "claude-5",
        // -------- Google Gemini + Gemma 3+
        "gemini-1.5",
        "gemini-2",
        "gemini-3",
        "gemini-pro-vision",
        "gemma-3",
        "gemma-4",
        "paligemma",
        // -------- Meta Llama
        "llama-3.2-11b-vision",
        "llama-3.2-90b-vision",
        "llama-3.2-vision",
        "llama-4",
        // -------- Mistral
        "pixtral",
        "mistral-small-3.1",
        "mistral-small-3.2",
        "mistral-small-4",
        "mistral-medium-3",
        // -------- Cohere
        "aya-vision",
        "command-a-vision",
        // -------- xAI Grok
        "grok-2-vision",
        "grok-1.5-vision",
        "grok-3",
        "grok-4",
        "grok-5",
        // -------- ByteDance Doubao
        "doubao-seed-1.5-vision",
        "doubao-1.5-vision",
        "doubao-1-5-vision",
        "doubao-seed-1.6-vision",
        "doubao-seed-2",
        "doubao-seed-3",
        "doubao-seed-4",
        "doubao-seed-5",
        "doubao-seed-6",
        "doubao-seed-7",
        "doubao-seed-8",
        "doubao-seed-9",
        "doubao-pro-vision",
        "doubao-vision",
        "seedream",
        "seedance",
        // -------- Alibaba Qwen
        "qwen-vl",
        "qwen2-vl",
        "qwen2.5-vl",
        "qwen3-vl",
        "qwen-max-vision",
        "qwen3.5",
        "qwen-3.5",
        "qwen3.6",
        "qwen-3.6",
        "qwen3.7",
        "qwen-3.7",
        "qwen3.8",
        "qwen-3.8",
        "qwen3.9",
        "qwen-3.9",
        "qwen4",
        "qwen-4",
        "qvq",
        // -------- Moonshot Kimi
        "kimi-for-coding",
        "kimi-k2.5",
        "kimi-k2.6",
        "kimi-k2.7",
        "kimi-k2.8",
        "kimi-k2.9",
        "kimi-vl",
        "moonshot-v1-vision",
        // -------- Zhipu GLM
        "glm-4v",
        "glm-4.1v",
        "glm-4.5v",
        "glm-4.6v",
        "glm-5v",
        "cogvlm",
        "cogagent",
        // -------- Baidu ERNIE
        "ernie-vl",
        "ernie-4.5-vl",
        "ernie-5",
        "ernie-vision",
        // -------- SenseTime SenseChat
        "sensechat-vision",
        "sensechat-v",
        "sensenova-v6",
        // -------- 01.AI Yi
        "yi-vl",
        "yi-vision",
        // -------- Baichuan
        "baichuan-omni",
        "baichuan-vl",
        "baichuan2-vl",
        // -------- DeepSeek
        "deepseek-vl",
        "deepseek-vl2",
        "janus",
        // -------- Tencent Hunyuan
        "hunyuan-vision",
        "hunyuan-vl",
        "hunyuanocr",
        // -------- MiniMax
        "minimax-vl",
        "abab-vision",
        "abab6.5-vision",
        // -------- StepFun
        "step-1v",
        "step-1o",
        "step-2-vision",
        "step-3",
        "step-3.5",
        // -------- Open-source major VLMs
        "llava",
        "internvl",
        "mini-internvl",
        "xcomposer",
        "minicpm-v",
        "minicpm-o",
        "minicpm-llama3-v",
        "phi-3-vision",
        "phi-3.5-vision",
        "phi-4-multimodal",
        "idefics",
        "blip",
        "instructblip",
        "xgen-mm",
        "fuyu",
        "kosmos",
        "ferret",
        "openelm-vision",
        "mm1",
        "florence-2",
        "florence-vl",
        "smolvlm",
        "vila",
        "nvila",
        "eagle2",
        "nvlm",
        "nemotron-vl",
        "pali-3",
        // -------- GUI-agent / screen-understanding VLMs
        "ui-tars",
        "showui",
        "os-atlas",
        "seeclick",
        "screenagent",
        "aria-ui",
        "omniparser",
        "mobileagent",
        "appagent",
        "autoui",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// User-facing error message when vision-model resolution fails.
pub fn vision_unavailable_message(reason: &str) -> String {
    let lang = rsclaw_i18n::default_lang();
    rsclaw_i18n::t_fmt("vision_unavailable", lang, &[("reason", reason)])
}
