//! Shared, public catalog of provider / channel / search-engine definitions
//! loaded from `defaults.toml`.
//!
//! These mirror the `[[providers]]`, `[[channels]]`, and `[[search_engines]]`
//! tables in `defaults.toml`, including the decorative localization fields the
//! desktop onboarding/settings UI renders (name_zh, tag_en, icon, order_*, …).
//!
//! ## Serialization contract
//!
//! Every field is `#[serde(default)]` so old or partial `defaults.toml` files
//! still parse. The structs derive both `Deserialize` (toml in) and
//! `Serialize` (JSON out) and keep their **snake_case** field names verbatim:
//! the toml keys are snake_case, and the HTTP/Tauri layers serialize the same
//! struct straight to JSON, so the JSON the frontend receives is **also
//! snake_case** (e.g. `name_zh`, `key_placeholder`, `has_base_url`,
//! `search_engines`). No camelCase rename layer is applied.
//!
//! The one pre-existing exception is channel field keys (`appId`, `appSecret`,
//! …) which live *inside the toml values themselves* and are therefore passed
//! through unchanged.

use serde::{Deserialize, Serialize};

/// One `[[providers]]` entry from `defaults.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderDef {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub env_var: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub needs_key: bool,
    /// API protocol identifier (mirrors `ApiFormat` serde names:
    /// `openai`, `openai-responses`, `anthropic`, `gemini`, `ollama`,
    /// `rsclaw`). Defaults to `"openai"` when omitted, matching the
    /// gateway's catch-all.
    #[serde(default = "default_api_type")]
    pub api_type: String,

    // --- Decorative / localization fields (UI only) ---
    #[serde(default)]
    pub name_zh: String,
    #[serde(default)]
    pub name_en: String,
    #[serde(default)]
    pub tag_zh: String,
    #[serde(default)]
    pub tag_en: String,
    #[serde(default)]
    pub key_label: String,
    #[serde(default)]
    pub key_placeholder: String,
    /// The "key" field is actually a base URL (Ollama / custom / codingplan).
    #[serde(default)]
    pub is_url: bool,
    /// Whether the UI should expose a separate base-URL input.
    #[serde(default)]
    pub has_base_url: bool,
    #[serde(default)]
    pub order_zh: i32,
    #[serde(default)]
    pub order_en: i32,
}

fn default_api_type() -> String {
    "openai".to_owned()
}

/// One field of a channel's credential form (`fields = [...]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelFieldDef {
    pub key: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub placeholder: String,
}

/// One `[[channels]]` entry from `defaults.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelDef {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub fields: Vec<ChannelFieldDef>,
    /// If true, run the `channels login` flow (QR/OAuth) instead of
    /// prompting fields.
    #[serde(default)]
    pub login: bool,
    /// If true, the gateway/editor exposes add/edit/remove account UI.
    #[serde(default)]
    pub multi_account: bool,

    // --- Decorative / localization fields (UI only) ---
    #[serde(default)]
    pub name_zh: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub has_qr: bool,
    #[serde(default)]
    pub qr_label_zh: String,
    #[serde(default)]
    pub qr_label_en: String,
    #[serde(default)]
    pub order_zh: i32,
    #[serde(default)]
    pub order_en: i32,
}

/// One `[[search_engines]]` entry from `defaults.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchEngineDef {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub env_var: String,

    // --- Decorative / localization fields (UI only) ---
    #[serde(default)]
    pub label_zh: String,
    #[serde(default)]
    pub label_en: String,
    #[serde(default)]
    pub needs_key: bool,
    #[serde(default)]
    pub order: i32,
}

/// The parsed catalog: providers, channels, and search engines.
///
/// Serializes to `{ "providers": [...], "channels": [...],
/// "search_engines": [...] }` (snake_case top-level keys).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Catalog {
    #[serde(default)]
    pub providers: Vec<ProviderDef>,
    #[serde(default)]
    pub channels: Vec<ChannelDef>,
    #[serde(default)]
    pub search_engines: Vec<SearchEngineDef>,
}

impl Catalog {
    /// Parse a catalog from a `defaults.toml` string.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

/// Load the catalog using the standard defaults-resolution order: the
/// user-editable `$base_dir/defaults.toml` if present, otherwise the
/// compiled-in fallback. Mirrors [`crate::loader::load_defaults_toml`].
///
/// Returns an error only if the resolved toml fails to parse; the embedded
/// `defaults.toml` is always valid, so in practice this only fails when a
/// user has hand-corrupted their external file.
pub fn load_catalog() -> Result<Catalog, toml::de::Error> {
    Catalog::from_toml_str(&crate::loader::load_defaults_toml())
}

/// Load the catalog from the compiled-in `defaults.toml` only (never reads the
/// user's external file). Useful for fresh-install paths.
pub fn embedded_catalog() -> Catalog {
    Catalog::from_toml_str(crate::loader::embedded_defaults_toml())
        .expect("embedded defaults.toml is always valid")
}
