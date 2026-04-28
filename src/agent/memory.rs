//! Agent memory subsystem -- semantic search over workspace memory files.
//!
//! Storage: redb (metadata) + hnsw_rs (vector index) under `data_dir/memory/`.
//! Embeddings: Configurable via `MemorySearchConfig`:
//!   - "local" (default): BGE-Small-en-v1.5 via candle-transformers (mmap).
//!     Falls back to deterministic pseudo-embedding (FNV hash projection) when
//!     model weights are unavailable.
//!   - "openai": OpenAI text-embedding-3-small/large via REST API.
//!   - "ollama": Ollama nomic-embed-text (or custom) via local REST API.
//!
//! Schema: id(str), scope(str), kind(str), text(str), vector(f32 x DIM).
//! DIM varies by provider: BGE-Small=384, OpenAI-small=1536, etc.

use std::{path::Path, sync::Arc};

use anyhow::{anyhow, Context, Result};
use hnsw_rs::{hnsw::Hnsw, prelude::DistCosine};
use tracing::{debug, info, warn};

use crate::{MemoryTier, config::schema::MemorySearchConfig};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_EMBED_DIM: i32 = 384; // fallback when no model loaded
const HNSW_MAX_NB_CONN: usize = 16; // M parameter
const HNSW_EF_CONSTRUCTION: usize = 200;

// Default models per provider.
const OPENAI_DEFAULT_MODEL: &str = "text-embedding-3-small";
const OLLAMA_DEFAULT_MODEL: &str = "nomic-embed-text";
const OLLAMA_DEFAULT_URL: &str = "http://localhost:11434";

// redb table for memory docs metadata.
const REDB_TABLE: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("memory_docs");

// ---------------------------------------------------------------------------
// MemDocTier -- 3-tier memory classification (P1)
// ---------------------------------------------------------------------------

/// Memory document tier for decay and priority control.
/// Named `MemDocTier` to avoid conflict with `crate::MemoryTier` (system RAM).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum MemDocTier {
    /// Identity-level facts, decay floor = 0.9
    Core,
    /// Active context, decay floor = 0.3
    Working,
    /// Low-priority, decay floor = 0.1
    Peripheral,
}

impl Default for MemDocTier {
    fn default() -> Self {
        Self::Working
    }
}

// ---------------------------------------------------------------------------
// MemoryDoc
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryDoc {
    pub id: String,
    /// Logical scope, e.g. agent ID or "global".
    pub scope: String,
    /// Document kind: "note", "summary", "fact", "session".
    pub kind: String,
    /// Raw text content.
    pub text: String,
    /// Embedding vector (populated by MemoryStore::add, not serialized to
    /// JSON).
    #[serde(skip, default)]
    pub vector: Vec<f32>,
    /// Unix timestamp (secs) when this doc was first stored.
    pub created_at: i64,
    /// Unix timestamp (secs) of last retrieval.
    pub accessed_at: i64,
    /// Number of times this document has been returned by search.
    pub access_count: i64,
    /// Explicit importance score 0.0-1.0. Defaults to 0.5.
    pub importance: f32,
    /// P1: 3-tier classification (Core/Working/Peripheral).
    #[serde(default)]
    pub tier: MemDocTier,
    /// P3/L0: One-sentence abstract of the text.
    #[serde(default)]
    pub abstract_text: Option<String>,
    /// P3/L1: Key-points overview (2-3 lines).
    #[serde(default)]
    pub overview_text: Option<String>,
    /// Freeform tags for lifecycle tracking (e.g. "crystallized", "merged").
    #[serde(default)]
    pub tags: Vec<String>,
    /// Pinned memories never decay and are immune to tier demotion.
    /// Use for user-stated facts: phone numbers, IDs, credentials, names.
    #[serde(default)]
    pub pinned: bool,
}

impl MemoryDoc {
    /// P2: Weibull stretched-exponential decay replacing simple decay.
    ///
    /// Pinned documents always return 1.0 — they never decay.
    pub fn relevance_score(&self) -> f32 {
        if self.pinned {
            return 1.0;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let age_days = ((now - self.created_at).max(0) as f32) / 86_400.0;

        // Weibull decay with tier-specific beta
        let beta = match self.tier {
            MemDocTier::Core => 0.8_f32,   // sub-exponential, slower decay
            MemDocTier::Working => 1.0,    // standard exponential
            MemDocTier::Peripheral => 1.3, // super-exponential, faster decay
        };
        let base_half_life: f32 = 30.0;
        let importance_mod: f32 = 1.5;
        let effective_hl = base_half_life * (importance_mod * self.importance).exp().min(10.0);
        let lambda = std::f32::consts::LN_2 / effective_hl;
        let recency = (-lambda * age_days.powf(beta)).exp();

        // Frequency component
        let freq_base = 1.0 - (-self.access_count as f32 / 5.0).exp();
        let frequency = freq_base;

        // Composite score
        let intrinsic = self.importance;
        let composite = 0.4 * recency + 0.3 * frequency + 0.3 * intrinsic;

        // Apply tier decay floor
        let floor = match self.tier {
            MemDocTier::Core => 0.9,
            MemDocTier::Working => 0.3,
            MemDocTier::Peripheral => 0.1,
        };
        composite.max(floor).clamp(0.01, 1.0)
    }

    /// Legacy alias kept for callers that used `decay_multiplier`.
    pub fn decay_multiplier(&self) -> f32 {
        self.relevance_score()
    }

    /// P1: Evaluate whether this doc should be promoted/demoted between tiers.
    ///
    /// Returns `true` if the doc was just promoted to [`MemDocTier::Core`]
    /// (used by the crystallization loop to detect skill candidates).
    /// Pinned documents are immune to demotion.
    pub fn evaluate_tier_transition(&mut self) -> bool {
        if self.pinned {
            self.tier = MemDocTier::Core;
            return false;
        }
        let was_core = self.tier == MemDocTier::Core;
        let score = self.relevance_score();

        // Promote to Core via three independent paths (thresholds from the
        // live evolution config):
        //   1. access_only         — sheer recall frequency
        //   2. importance_only     — strong positive feedback alone
        //   3. both_*              — both signals decent
        //
        // The previous AND-gate (>=10 AND >=0.8) made promotion compound-rare,
        // which combined with cluster_size >= 3 effectively starved the
        // crystallization pipeline.
        let promo = &crate::agent::evolution::evolution_config().promotion;
        if self.access_count >= promo.access_only
            || self.importance >= promo.importance_only
            || (self.access_count >= promo.both_access
                && self.importance >= promo.both_importance)
        {
            self.tier = MemDocTier::Core;
            return !was_core;
        }

        // Demote to Peripheral: relevance_score < 0.15 OR (age > 60 days AND access_count < 3)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let age_days = ((now - self.created_at).max(0) as f32) / 86_400.0;
        if score < 0.15 || (age_days > 60.0 && self.access_count < 3) {
            self.tier = MemDocTier::Peripheral;
            return false;
        }

        // Promote to Working: access_count >= 3 AND relevance_score >= 0.4
        if self.access_count >= 3 && score >= 0.4 {
            if self.tier == MemDocTier::Peripheral {
                self.tier = MemDocTier::Working;
            }
        }
        false
    }

    /// Touch this doc on access: update timestamp, bump count, evaluate tier.
    pub fn touch(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.accessed_at = now;
        self.access_count += 1;
        self.evaluate_tier_transition();
    }

    /// P3: Return the overview (L1) text if available, otherwise the full text.
    pub fn display_text(&self) -> &str {
        self.overview_text.as_deref().unwrap_or(&self.text)
    }
}

// ---------------------------------------------------------------------------
// P3: L0/L1 text extraction helpers
// ---------------------------------------------------------------------------

/// Extract the first sentence as L0 abstract.
fn extract_abstract(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    // Split on ". " or Chinese period
    let end = text
        .find(". ")
        .map(|i| i + 1) // include the period
        .or_else(|| text.find('\u{3002}').map(|i| i + '\u{3002}'.len_utf8()))
        .unwrap_or_else(|| {
            text.char_indices()
                .nth(150)
                .map(|(i, _)| i)
                .unwrap_or(text.len())
        });
    let sentence = text[..end].trim();
    if sentence.is_empty() {
        None
    } else {
        Some(sentence.to_string())
    }
}

/// Extract first 3 sentences or first 200 chars as L1 overview.
fn extract_overview(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    // Use char_indices to avoid UTF-8 boundary issues with CJK text.
    let mut count = 0;
    let mut end = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if count >= 3 {
            break;
        }
        if ch == '.' {
            if let Some(&(_, next_ch)) = chars.peek() {
                if next_ch == ' ' {
                    count += 1;
                    end = i + 1;
                }
            }
        } else if ch == '\u{3002}' {
            count += 1;
            end = i + ch.len_utf8();
        }
    }
    // If fewer than 1 sentence found, use char limit
    if count == 0 {
        end = text
            .char_indices()
            .nth(200)
            .map(|(i, _)| i)
            .unwrap_or(text.len());
    }
    let overview = text[..end].trim();
    if overview.is_empty() {
        None
    } else {
        Some(overview.to_string())
    }
}

// ---------------------------------------------------------------------------
// Embedder trait
// ---------------------------------------------------------------------------

/// Pluggable text → vector backend. Implementors are interchangeable at
/// runtime — see `MemoryStore::begin_swap` for the hot-migration path used
/// to upgrade from FNV → BGE (or BGE-small → BGE-base) without restart.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Vec<f32>;
    fn dimension(&self) -> i32;
    /// Count tokens precisely (when tokenizer is available) or estimate.
    fn count_tokens(&self, text: &str) -> usize {
        // Default: heuristic estimation (ASCII/4 + CJK*1.5)
        crate::agent::runtime::estimate_tokens(text)
    }
}

// ---------------------------------------------------------------------------
// EmbedderBackend
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
pub enum EmbedderBackend {
    Local(LocalBgeEmbedder),
    Fnv(FnvEmbedder),
    OpenAi(OpenAiEmbedder),
    Ollama(OllamaEmbedder),
}

impl Embedder for EmbedderBackend {
    fn embed(&self, text: &str) -> Vec<f32> {
        match self {
            Self::Local(e) => e.embed(text),
            Self::Fnv(e) => e.embed(text),
            Self::OpenAi(e) => e.embed(text),
            Self::Ollama(e) => e.embed(text),
        }
    }

    fn dimension(&self) -> i32 {
        match self {
            Self::Local(e) => e.dimension(),
            Self::Fnv(e) => e.dimension(),
            Self::OpenAi(e) => e.dimension(),
            Self::Ollama(e) => e.dimension(),
        }
    }
}

// ---------------------------------------------------------------------------
// FnvEmbedder
// ---------------------------------------------------------------------------

pub struct FnvEmbedder {
    dim: i32,
}

impl FnvEmbedder {
    fn new(dim: i32) -> Self {
        Self { dim }
    }
}

impl Embedder for FnvEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let dim = self.dim as usize;
        let mut v = vec![0.0_f32; dim];
        let bytes = text.as_bytes();
        for (i, chunk) in bytes.chunks(4).enumerate() {
            let mut h: u32 = 2_166_136_261;
            for &b in chunk {
                h ^= u32::from(b);
                h = h.wrapping_mul(16_777_619);
            }
            v[i % dim] += f32::from_bits(0x3F80_0000 | (h & 0x007F_FFFF)) - 1.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }

    fn dimension(&self) -> i32 {
        self.dim
    }
}

// ---------------------------------------------------------------------------
// LocalBgeEmbedder
// ---------------------------------------------------------------------------

pub struct LocalBgeEmbedder {
    tokenizer: tokenizers::Tokenizer,
    model: candle_transformers::models::bert::BertModel,
    device: candle_core::Device,
    hidden_size: usize,
}

impl LocalBgeEmbedder {
    /// Load BGE weights, tokenizer, and config from a model directory.
    /// Expects `config.json`, `model.safetensors`, and `tokenizer.json`.
    pub fn load(model_dir: &Path) -> Result<Self> {
        use candle_core::{DType, Device};
        use candle_nn::VarBuilder;
        use candle_transformers::models::bert::{BertModel, Config as BertConfig};

        let device = Device::Cpu;
        let config_path = model_dir.join("config.json");
        let weights_path = model_dir.join("model.safetensors");
        let tokenizer_path = model_dir.join("tokenizer.json");

        let config_str = std::fs::read_to_string(&config_path)
            .with_context(|| format!("missing {}", config_path.display()))?;
        let config: BertConfig =
            serde_json::from_str(&config_str).context("invalid BGE config.json")?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)
                .context("failed to mmap BGE model weights")?
        };

        let model = BertModel::load(vb, &config).context("failed to load BertModel")?;
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("tokenizer load failed: {e}"))?;

        let hidden_size = config.hidden_size;
        Ok(Self {
            tokenizer,
            model,
            device,
            hidden_size,
        })
    }
}

impl Embedder for LocalBgeEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        use candle_core::Tensor;

        let dim = self.hidden_size;

        let encoding = match self.tokenizer.encode(text, true) {
            Ok(e) => e,
            Err(e) => {
                warn!("tokenizer error: {e}");
                return vec![0.0; dim];
            }
        };

        // BERT max_position_embeddings is 512 — truncate to avoid
        // "index-select invalid index 512" panics.
        const MAX_SEQ: usize = 512;
        let ids: Vec<u32> = encoding.get_ids().iter().take(MAX_SEQ).copied().collect();
        let type_ids: Vec<u32> = encoding.get_type_ids().iter().take(MAX_SEQ).copied().collect();
        let len = ids.len();

        let make_tensor = |data: Vec<u32>| -> Result<Tensor, candle_core::Error> {
            Tensor::from_iter(data.into_iter().map(|x| x as i64), &self.device)?.reshape((1, len))
        };

        let input_ids = match make_tensor(ids) {
            Ok(t) => t,
            Err(e) => {
                warn!("tensor error: {e}");
                return vec![0.0; dim];
            }
        };
        let type_ids_t = match make_tensor(type_ids) {
            Ok(t) => t,
            Err(e) => {
                warn!("tensor error: {e}");
                return vec![0.0; dim];
            }
        };
        let attention_mask =
            match Tensor::ones((1_usize, len), candle_core::DType::I64, &self.device) {
                Ok(t) => t,
                Err(e) => {
                    warn!("tensor error: {e}");
                    return vec![0.0; dim];
                }
            };

        let output = match self
            .model
            .forward(&input_ids, &type_ids_t, Some(&attention_mask))
        {
            Ok(o) => o,
            Err(e) => {
                warn!("bert forward error: {e}");
                return vec![0.0; dim];
            }
        };

        let pooled = match output.mean(1) {
            Ok(p) => p,
            Err(e) => {
                warn!("mean-pool error: {e}");
                return vec![0.0; dim];
            }
        };

        let flat = match pooled.flatten_all().and_then(|t| t.to_vec1::<f32>()) {
            Ok(v) => v,
            Err(e) => {
                warn!("flatten error: {e}");
                return vec![0.0; dim];
            }
        };

        let norm = flat.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
        flat.into_iter().map(|x| x / norm).collect()
    }

    fn dimension(&self) -> i32 {
        self.hidden_size as i32
    }

    fn count_tokens(&self, text: &str) -> usize {
        self.tokenizer
            .encode(text, false)
            .map(|e| e.get_ids().len())
            .unwrap_or_else(|_| crate::agent::runtime::estimate_tokens(text))
    }
}

// ---------------------------------------------------------------------------
// OpenAiEmbedder
// ---------------------------------------------------------------------------

pub struct OpenAiEmbedder {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dim: i32,
}

impl OpenAiEmbedder {
    fn new(api_key: String, model: Option<String>, base_url: Option<String>) -> Self {
        let model = model.unwrap_or_else(|| OPENAI_DEFAULT_MODEL.to_owned());
        let dim = openai_model_dim(&model);
        let _ = base_url;
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            dim,
        }
    }

    fn embed_blocking(&self, text: &str) -> Result<Vec<f32>> {
        let url = "https://api.openai.com/v1/embeddings";
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });

        let rt = tokio::runtime::Handle::try_current();
        let response_text = match rt {
            Ok(handle) => tokio::task::block_in_place(|| {
                handle.block_on(async {
                    self.client
                        .post(url)
                        .header("Authorization", format!("Bearer {}", self.api_key))
                        .json(&body)
                        .send()
                        .await?
                        .text()
                        .await
                })
            })
            .context("OpenAI embeddings request failed")?,
            Err(_) => {
                let tmp_rt = tokio::runtime::Runtime::new()
                    .context("failed to create temp runtime for OpenAI embed")?;
                tmp_rt
                    .block_on(async {
                        self.client
                            .post(url)
                            .header("Authorization", format!("Bearer {}", self.api_key))
                            .json(&body)
                            .send()
                            .await?
                            .text()
                            .await
                    })
                    .context("OpenAI embeddings request failed")?
            }
        };

        let parsed: serde_json::Value = serde_json::from_str(&response_text)
            .context("OpenAI embeddings: invalid JSON response")?;
        let embedding = parsed["data"][0]["embedding"]
            .as_array()
            .context("OpenAI embeddings: missing data[0].embedding")?;
        Ok(embedding
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect())
    }
}

impl Embedder for OpenAiEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        match self.embed_blocking(text) {
            Ok(v) => {
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
                v.into_iter().map(|x| x / norm).collect()
            }
            Err(e) => {
                warn!("OpenAI embedding failed: {e:#}");
                vec![0.0; self.dim as usize]
            }
        }
    }

    fn dimension(&self) -> i32 {
        self.dim
    }
}

fn openai_model_dim(model: &str) -> i32 {
    match model {
        "text-embedding-3-large" => 3072,
        "text-embedding-3-small" | "text-embedding-ada-002" => 1536,
        _ => 1536,
    }
}

// ---------------------------------------------------------------------------
// OllamaEmbedder
// ---------------------------------------------------------------------------

pub struct OllamaEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
    dim: std::sync::Mutex<Option<i32>>,
    default_dim: i32,
}

impl OllamaEmbedder {
    fn new(model: Option<String>, base_url: Option<String>) -> Self {
        let model = model.unwrap_or_else(|| OLLAMA_DEFAULT_MODEL.to_owned());
        let base_url = base_url.unwrap_or_else(|| OLLAMA_DEFAULT_URL.to_owned());
        let default_dim = ollama_model_dim(&model);
        Self {
            client: reqwest::Client::new(),
            base_url,
            model,
            dim: std::sync::Mutex::new(None),
            default_dim,
        }
    }

    fn embed_blocking(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/api/embed", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });

        let rt = tokio::runtime::Handle::try_current();
        let response_text = match rt {
            Ok(handle) => tokio::task::block_in_place(|| {
                handle.block_on(async {
                    self.client
                        .post(&url)
                        .json(&body)
                        .send()
                        .await?
                        .text()
                        .await
                })
            })
            .context("Ollama embed request failed")?,
            Err(_) => {
                let tmp_rt = tokio::runtime::Runtime::new()
                    .context("failed to create temp runtime for Ollama embed")?;
                tmp_rt
                    .block_on(async {
                        self.client
                            .post(&url)
                            .json(&body)
                            .send()
                            .await?
                            .text()
                            .await
                    })
                    .context("Ollama embed request failed")?
            }
        };

        let parsed: serde_json::Value =
            serde_json::from_str(&response_text).context("Ollama embed: invalid JSON response")?;
        let embedding = parsed["embeddings"][0]
            .as_array()
            .context("Ollama embed: missing embeddings[0]")?;
        let vec: Vec<f32> = embedding
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();

        if !vec.is_empty()
            && let Ok(mut dim) = self.dim.lock()
        {
            *dim = Some(vec.len() as i32);
        }

        Ok(vec)
    }
}

impl Embedder for OllamaEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        match self.embed_blocking(text) {
            Ok(v) => {
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
                v.into_iter().map(|x| x / norm).collect()
            }
            Err(e) => {
                warn!("Ollama embedding failed: {e:#}");
                vec![0.0; self.default_dim as usize]
            }
        }
    }

    fn dimension(&self) -> i32 {
        self.dim
            .lock()
            .ok()
            .and_then(|d| *d)
            .unwrap_or(self.default_dim)
    }
}

fn ollama_model_dim(model: &str) -> i32 {
    match model {
        "nomic-embed-text" => 768,
        "mxbai-embed-large" => 1024,
        "all-minilm" => 384,
        "snowflake-arctic-embed" => 1024,
        _ => 768,
    }
}

// ---------------------------------------------------------------------------
// MemoryStore -- hnsw_rs + redb
// ---------------------------------------------------------------------------

/// In-flight embedder migration. The primary index keeps serving reads while
/// the secondary is built up doc-by-doc off-lock; `add()` dual-writes so the
/// new index never falls behind. `commit_swap` atomically replaces primary
/// with secondary and persists the new vectors to redb in one transaction.
pub struct MigrationCtx {
    new_embedder: Arc<dyn Embedder>,
    new_hnsw: Hnsw<'static, f32, DistCosine>,
    new_embed_dim: i32,
    /// Per-doc-index → new vector. Indices are positions in `MemoryStore::docs`.
    new_vectors: std::collections::HashMap<usize, Vec<f32>>,
}

pub struct MemoryStore {
    db: redb::Database,
    hnsw: Hnsw<'static, f32, DistCosine>,
    /// All docs in insertion order. HNSW DataId == index into this vec.
    docs: Vec<MemoryDoc>,
    embedder: Arc<dyn Embedder>,
    embed_dim: i32,
    /// Active hot-swap migration, if any. While this is `Some`, reads use the
    /// primary embedder/hnsw and writes go to both primary and secondary.
    swap: Option<MigrationCtx>,
    /// Count of docs from redb whose stored vector dimension didn't match the
    /// active embedder at load time. Non-zero means an embedder upgrade is in
    /// progress — startup should kick off a background migration so search
    /// becomes available without blocking gateway boot.
    pending_migration: usize,
}

impl MemoryStore {
    /// Open memory store with exclusive write access (for gateway).
    pub async fn open(
        data_dir: &Path,
        model_dir: Option<&Path>,
        _tier: MemoryTier,
        search_cfg: Option<&MemorySearchConfig>,
    ) -> Result<Self> {
        let mem_dir = data_dir.join("memory");
        std::fs::create_dir_all(&mem_dir)?;
        let db_path = mem_dir.join("memory.redb");
        let db = redb::Database::create(&db_path).context("open memory redb")?;
        Self::open_with_db(db, model_dir, search_cfg).await
    }

    /// Open memory store in read-only mode (for CLI, won't conflict with running gateway).
    pub async fn open_readonly(
        data_dir: &Path,
        model_dir: Option<&Path>,
        search_cfg: Option<&MemorySearchConfig>,
    ) -> Result<Self> {
        let db_path = data_dir.join("memory/memory.redb");
        if !db_path.exists() {
            anyhow::bail!("memory database not found at {}", db_path.display());
        }
        let db = redb::Database::open(&db_path).context("open memory redb (readonly)")?;
        Self::open_with_db(db, model_dir, search_cfg).await
    }

    async fn open_with_db(
        db: redb::Database,
        model_dir: Option<&Path>,
        search_cfg: Option<&MemorySearchConfig>,
    ) -> Result<Self> {
        let embedder: Arc<dyn Embedder> = choose_embedder(search_cfg, model_dir);
        let embed_dim = embedder.dimension();

        // Ensure table exists (skip if readonly — open() would have failed anyway).
        if let Ok(write) = db.begin_write() {
            if let Err(e) = write.open_table(REDB_TABLE) {
                tracing::warn!(error = %e, "memory: failed to create table");
            }
            if let Err(e) = write.commit() {
                tracing::warn!(error = %e, "memory: failed to commit table creation");
            }
        }

        // Load existing docs and rebuild HNSW index.
        let mut docs = Vec::new();
        {
            let read = db.begin_read()?;
            let table = read.open_table(REDB_TABLE)?;
            let range = table.range::<&str>(..)?;
            for entry in range {
                let (_, value) = entry?;
                let raw = value.value();
                // Stored as: [vector_bytes...][json_bytes...]
                // Format: 4 bytes vec_len (LE u32), then vec_len*4 bytes f32s, then JSON.
                if raw.len() < 4 {
                    continue;
                }
                let vec_count = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
                let vec_bytes = 4 + vec_count * 4;
                if raw.len() < vec_bytes {
                    continue;
                }
                let vector: Vec<f32> = (0..vec_count)
                    .map(|i| {
                        let off = 4 + i * 4;
                        f32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]])
                    })
                    .collect();
                if let Ok(mut doc) = serde_json::from_slice::<MemoryDoc>(&raw[vec_bytes..]) {
                    doc.vector = vector;
                    docs.push(doc);
                }
            }
        }

        let max_elements = docs.len().max(1024);
        let hnsw = Hnsw::<'static, f32, DistCosine>::new(
            HNSW_MAX_NB_CONN,
            max_elements,
            16,
            HNSW_EF_CONSTRUCTION,
            DistCosine,
        );

        // Re-insert all loaded docs (skip dimension mismatches from model changes).
        let expected_dim = embed_dim as usize;
        let mut skipped = 0usize;
        for (i, doc) in docs.iter().enumerate() {
            if doc.vector.len() == expected_dim {
                hnsw.insert((&doc.vector, i));
            } else {
                skipped += 1;
            }
        }
        if skipped > 0 {
            info!(
                skipped,
                expected_dim,
                "memory: dimension mismatch detected; caller will drive background re-embed"
            );
        }

        if !docs.is_empty() {
            info!(count = docs.len(), "memory store loaded from redb");
        }

        // Skipped docs (dim mismatch from a model upgrade) trigger a deferred
        // background migration — caller checks `pending_migration_count()`
        // post-open and runs `run_embedder_migration` off-lock so startup
        // isn't blocked by re-embedding thousands of docs.
        let store = Self {
            db,
            hnsw,
            docs,
            embedder,
            embed_dim,
            swap: None,
            pending_migration: skipped,
        };

        Ok(store)
    }

    /// Number of docs whose stored vector dimension doesn't match the
    /// currently-loaded embedder. Non-zero immediately after `open` when the
    /// user has upgraded to a different-sized embedding model — the caller
    /// should drive `begin_swap` + the standard migration loop in the
    /// background to backfill them.
    pub fn pending_migration_count(&self) -> usize {
        self.pending_migration
    }

    /// Cheap clone of the active embedder Arc — used by the migration driver
    /// to embed off-lock without holding the store mutex.
    pub fn embedder_arc(&self) -> Arc<dyn Embedder> {
        Arc::clone(&self.embedder)
    }

    /// Snapshot the embedders an off-lock `add` needs to compute vectors for.
    /// Returns `(primary, Some(secondary))` only when a swap is in progress
    /// AND the secondary differs from primary; otherwise `(primary, None)`
    /// signals the caller to skip the second embed and let `add_pre_embedded`
    /// reuse the primary vector for the dual-write path.
    pub fn embedders_for_dual_write(&self) -> (Arc<dyn Embedder>, Option<Arc<dyn Embedder>>) {
        let primary = Arc::clone(&self.embedder);
        let secondary = self.swap.as_ref().and_then(|ctx| {
            if Arc::ptr_eq(&primary, &ctx.new_embedder) {
                None
            } else {
                Some(Arc::clone(&ctx.new_embedder))
            }
        });
        (primary, secondary)
    }

    pub async fn add(&mut self, doc: MemoryDoc) -> Result<()> {
        // Embed under lock — backward-compatible path. Hot callers that hold
        // a shared `Mutex<MemoryStore>` should use `add_off_lock` instead so
        // the BERT inference doesn't stall every concurrent read/write.
        let primary_vec = self.embedder.embed(&doc.text);
        let secondary_vec = self.swap.as_ref().and_then(|ctx| {
            if Arc::ptr_eq(&self.embedder, &ctx.new_embedder) {
                None
            } else {
                Some(ctx.new_embedder.embed(&doc.text))
            }
        });
        self.add_pre_embedded(doc, primary_vec, secondary_vec).await
    }

    /// Insert a doc whose vector(s) have already been computed off-lock.
    /// `secondary_vec` is required when a swap is in progress AND the swap
    /// target is a different embedder than primary; pass `None` when the
    /// caller's `embedders_for_dual_write` returned a single embedder so we
    /// safely reuse the primary vector.
    pub async fn add_pre_embedded(
        &mut self,
        mut doc: MemoryDoc,
        primary_vec: Vec<f32>,
        secondary_vec: Option<Vec<f32>>,
    ) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if doc.created_at == 0 {
            doc.created_at = now;
        }
        if doc.accessed_at == 0 {
            doc.accessed_at = now;
        }
        if doc.importance == 0.0 {
            doc.importance = 0.5;
        }

        // P3: Generate L0/L1 text summaries if not already set.
        if doc.abstract_text.is_none() {
            doc.abstract_text = extract_abstract(&doc.text);
        }
        if doc.overview_text.is_none() {
            doc.overview_text = extract_overview(&doc.text);
        }

        // Sanity: primary vector must match active embedder's dim.
        if primary_vec.len() != self.embed_dim as usize {
            anyhow::bail!(
                "add_pre_embedded: primary vector dim {} != active embedder dim {}",
                primary_vec.len(),
                self.embed_dim
            );
        }
        doc.vector = primary_vec;

        // Persist to redb.
        let serialized = serialize_doc(&doc)?;
        {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(REDB_TABLE)?;
                table.insert(doc.id.as_str(), serialized.as_slice())?;
            }
            write.commit()?;
        }

        // Insert into HNSW index.
        let idx = self.docs.len();
        self.hnsw.insert((&doc.vector, idx));

        // Dual-write to the migration secondary so new docs added during a
        // swap don't get left out. When the swap target is the SAME embedder
        // as primary, reuse the primary vector — caller would have passed
        // None for `secondary_vec` in that case.
        if let Some(ctx) = self.swap.as_mut() {
            let new_vec = match secondary_vec {
                Some(v) => v,
                None => doc.vector.clone(),
            };
            if new_vec.len() == ctx.new_embed_dim as usize {
                ctx.new_hnsw.insert((&new_vec, idx));
                ctx.new_vectors.insert(idx, new_vec);
            } else {
                tracing::warn!(
                    got = new_vec.len(),
                    expected = ctx.new_embed_dim,
                    "add_pre_embedded: secondary vector dim mismatch — secondary index will miss this doc until next swap"
                );
            }
        }

        self.docs.push(doc);

        debug!(idx, "memory doc stored");
        Ok(())
    }

    pub async fn delete(&mut self, id: &str) -> Result<()> {
        // Remove from redb.
        {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(REDB_TABLE)?;
                table.remove(id)?;
            }
            write.commit()?;
        }

        // Mark doc as deleted (clear text). HNSW doesn't support deletion
        // natively, but we filter deleted docs in search results.
        if let Some(doc) = self.docs.iter_mut().find(|d| d.id == id) {
            doc.id.clear();
            doc.text.clear();
        }

        Ok(())
    }

    pub async fn search(
        &mut self,
        query: &str,
        scope: Option<&str>,
        top_k: usize,
    ) -> Result<Vec<MemoryDoc>> {
        if self.docs.is_empty() {
            return Ok(vec![]);
        }

        let q_vec = self.embedder.embed(query);
        // Search more than top_k to account for filtered/deleted docs.
        let ef_search = (top_k * 4).max(32);
        let neighbours = self.hnsw.search(&q_vec, top_k + 10, ef_search);

        let mut result_indices = Vec::new();
        for n in neighbours {
            let idx = n.d_id;
            if idx >= self.docs.len() {
                continue;
            }
            if self.docs[idx].id.is_empty() {
                continue; // deleted
            }
            if let Some(s) = scope
                && self.docs[idx].scope != s
            {
                continue;
            }
            result_indices.push(idx);
            if result_indices.len() >= top_k {
                break;
            }
        }

        // Touch each matched doc (updates access stats & tier).
        let mut results = Vec::with_capacity(result_indices.len());
        for idx in result_indices {
            self.docs[idx].touch();
            results.push(self.docs[idx].clone());
        }

        Ok(results)
    }

    pub async fn get(&self, id: &str) -> Result<Option<MemoryDoc>> {
        Ok(self.docs.iter().find(|d| d.id == id).cloned())
    }

    /// Whether a hot-swap migration is currently in progress.
    pub fn is_migrating(&self) -> bool {
        self.swap.is_some()
    }

    /// Begin a hot-swap migration to a new embedder. Creates the secondary
    /// HNSW index; primary keeps serving reads. Caller drives migration via
    /// `swap_pending` + `swap_apply_batch` + `commit_swap` (typically from a
    /// dedicated background task that does the heavy embedding work off-lock).
    ///
    /// When `new_embedder` is the SAME instance as the current primary
    /// (the dim-mismatch re-embed pattern: caller passes the active embedder
    /// to repopulate HNSW from existing in-memory vectors), already-correct
    /// vectors are seeded into the secondary index up front so `swap_pending`
    /// only returns the docs that genuinely need re-embedding. Without that
    /// seed `commit_swap` would replace primary HNSW with a secondary that
    /// dropped the matching-dim docs — a silent recall regression.
    pub fn begin_swap(&mut self, new_embedder: Arc<dyn Embedder>) -> Result<()> {
        if self.swap.is_some() {
            anyhow::bail!("memory: a swap is already in progress");
        }
        let new_embed_dim = new_embedder.dimension();
        let max_elements = self.docs.len().max(1024);
        let new_hnsw = Hnsw::<'static, f32, DistCosine>::new(
            HNSW_MAX_NB_CONN,
            max_elements,
            16,
            HNSW_EF_CONSTRUCTION,
            DistCosine,
        );
        let mut new_vectors = std::collections::HashMap::new();
        if Arc::ptr_eq(&self.embedder, &new_embedder) {
            let expected = new_embed_dim as usize;
            for (i, doc) in self.docs.iter().enumerate() {
                if !doc.id.is_empty() && doc.vector.len() == expected {
                    new_hnsw.insert((&doc.vector, i));
                    new_vectors.insert(i, doc.vector.clone());
                }
            }
        }
        self.swap = Some(MigrationCtx {
            new_embedder,
            new_hnsw,
            new_embed_dim,
            new_vectors,
        });
        Ok(())
    }

    /// Snapshot the next batch of doc indices that still need re-embedding.
    /// Returns `(idx, text)` pairs the caller can embed off-lock.
    ///
    /// When the swap target embedder is the SAME instance as the primary
    /// (the dim-mismatch re-embed pattern), only docs whose stored vector
    /// dim doesn't match the current embedder are returned — re-embedding
    /// already-correct vectors would be wasted CPU and produce identical
    /// output. For a true embedder swap (different model), every doc still
    /// needs re-embedding, so the dim filter is skipped.
    pub fn swap_pending(&self, max: usize) -> Vec<(usize, String)> {
        let Some(ctx) = self.swap.as_ref() else {
            return Vec::new();
        };
        let same_embedder = Arc::ptr_eq(&self.embedder, &ctx.new_embedder);
        let expected = ctx.new_embed_dim as usize;
        self.docs
            .iter()
            .enumerate()
            .filter(|(i, d)| {
                if d.id.is_empty() || ctx.new_vectors.contains_key(i) {
                    return false;
                }
                if same_embedder && d.vector.len() == expected {
                    // Already-correct vector under the same embedder; skip.
                    return false;
                }
                true
            })
            .take(max)
            .map(|(i, d)| (i, d.text.clone()))
            .collect()
    }

    /// Apply a batch of `(doc_idx, new_vector)` pairs to the secondary index.
    /// Skips entries whose dimension doesn't match the new embedder.
    pub fn swap_apply_batch(&mut self, batch: Vec<(usize, Vec<f32>)>) -> Result<usize> {
        let Some(ctx) = self.swap.as_mut() else {
            anyhow::bail!("memory: no swap in progress");
        };
        let expected = ctx.new_embed_dim as usize;
        let mut applied = 0usize;
        for (idx, vector) in batch {
            if vector.len() != expected {
                tracing::warn!(idx, got = vector.len(), expected, "swap_apply_batch: dim mismatch, skipping");
                continue;
            }
            ctx.new_hnsw.insert((&vector, idx));
            ctx.new_vectors.insert(idx, vector);
            applied += 1;
        }
        Ok(applied)
    }

    /// Atomically replace primary with secondary and persist new vectors to
    /// redb in a single transaction. Returns the number of docs migrated.
    pub fn commit_swap(&mut self) -> Result<usize> {
        let ctx = self
            .swap
            .take()
            .context("memory: no swap in progress")?;
        let MigrationCtx {
            new_embedder,
            new_hnsw,
            new_embed_dim,
            new_vectors,
        } = ctx;

        // Update doc.vector for each migrated doc and persist atomically.
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(REDB_TABLE)?;
            for (idx, vector) in &new_vectors {
                if *idx >= self.docs.len() {
                    continue;
                }
                let doc = &mut self.docs[*idx];
                if doc.id.is_empty() {
                    continue;
                }
                doc.vector = vector.clone();
                let serialized = serialize_doc(doc)?;
                table.insert(doc.id.as_str(), serialized.as_slice())?;
            }
        }
        write.commit()?;

        let migrated = new_vectors.len();
        self.embedder = new_embedder;
        self.embed_dim = new_embed_dim;
        self.hnsw = new_hnsw;
        self.pending_migration = 0;

        info!(migrated, "memory: swap committed");
        Ok(migrated)
    }

    /// Drop an in-progress swap without applying any changes. The primary
    /// index is left untouched and the secondary state is discarded.
    pub fn abort_swap(&mut self) {
        if self.swap.take().is_some() {
            tracing::warn!("memory: swap aborted");
        }
    }

    pub async fn reindex(&mut self) -> Result<usize> {
        let active_docs: Vec<MemoryDoc> = self
            .docs
            .iter()
            .filter(|d| !d.id.is_empty())
            .cloned()
            .collect();
        let count = active_docs.len();
        if count == 0 {
            return Ok(0);
        }

        // Re-embed all docs.
        self.docs.clear();
        let max_elements = count.max(1024);
        self.hnsw = Hnsw::<'static, f32, DistCosine>::new(
            HNSW_MAX_NB_CONN,
            max_elements,
            16,
            HNSW_EF_CONSTRUCTION,
            DistCosine,
        );

        // Clear redb by deleting and recreating the table.
        {
            let write = self.db.begin_write()?;
            write.delete_table(REDB_TABLE)?;
            let _ = write.open_table(REDB_TABLE)?;
            write.commit()?;
        }

        for mut doc in active_docs {
            doc.vector = self.embedder.embed(&doc.text);
            let serialized = serialize_doc(&doc)?;
            {
                let write = self.db.begin_write()?;
                {
                    let mut table = write.open_table(REDB_TABLE)?;
                    table.insert(doc.id.as_str(), serialized.as_slice())?;
                }
                write.commit()?;
            }
            let idx = self.docs.len();
            self.hnsw.insert((&doc.vector, idx));
            self.docs.push(doc);
        }

        info!(count, "memory reindex complete");
        Ok(count)
    }

    /// Count tokens precisely using the loaded tokenizer (or heuristic fallback).
    pub fn count_tokens(&self, text: &str) -> usize {
        self.embedder.count_tokens(text)
    }

    pub async fn count(&self) -> Result<usize> {
        Ok(self.docs.iter().filter(|d| !d.id.is_empty()).count())
    }

    pub fn embed_dim(&self) -> i32 {
        self.embed_dim
    }

    // -----------------------------------------------------------------------
    // Organic-evolution helpers
    // -----------------------------------------------------------------------

    /// Synchronous lookup by ID (in-memory only, no I/O).
    pub fn get_sync(&self, id: &str) -> Option<&MemoryDoc> {
        self.docs.iter().find(|d| !d.id.is_empty() && d.id == id)
    }

    /// Adjust the importance score of a memory document by `delta`, clamping
    /// to \[0.01, 1.0\].  Persists the change to redb and re-evaluates tier.
    ///
    /// Returns the new importance value, or `None` if the doc was not found.
    pub async fn adjust_importance(&mut self, id: &str, delta: f32) -> Result<Option<f32>> {
        let idx = match self.docs.iter().position(|d| d.id == id) {
            Some(i) => i,
            None => return Ok(None),
        };
        let doc = &mut self.docs[idx];
        doc.importance = (doc.importance + delta).clamp(0.01, 1.0);
        doc.evaluate_tier_transition();
        self.persist_doc(idx)?;
        Ok(Some(self.docs[idx].importance))
    }

    /// Find memory documents whose cosine similarity to `doc_id` exceeds
    /// `threshold`.  Returns pairs of `(MemoryDoc, similarity)` sorted by
    /// similarity descending, excluding the source doc itself and deleted docs.
    pub fn find_near_duplicates(
        &self,
        doc_id: &str,
        scope: Option<&str>,
        threshold: f32,
    ) -> Result<Vec<(MemoryDoc, f32)>> {
        let src_idx = self
            .docs
            .iter()
            .position(|d| d.id == doc_id)
            .ok_or_else(|| anyhow!("doc not found: {doc_id}"))?;
        let src_vec = &self.docs[src_idx].vector;
        if src_vec.is_empty() {
            return Ok(vec![]);
        }

        let neighbours = self.hnsw.search(src_vec, 50, 64);
        let mut pairs: Vec<(MemoryDoc, f32)> = Vec::new();
        for n in neighbours {
            let idx = n.d_id;
            if idx >= self.docs.len() || idx == src_idx {
                continue;
            }
            let doc = &self.docs[idx];
            if doc.id.is_empty() {
                continue;
            }
            if let Some(s) = scope {
                if doc.scope != s {
                    continue;
                }
            }
            let sim = cosine_similarity(src_vec, &doc.vector);
            if sim >= threshold {
                pairs.push((doc.clone(), sim));
            }
        }
        pairs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(pairs)
    }

    /// Return all non-deleted docs in the given tier and scope.
    pub fn find_by_tier(&self, tier: &MemDocTier, scope: Option<&str>) -> Vec<&MemoryDoc> {
        self.docs
            .iter()
            .filter(|d| {
                !d.id.is_empty()
                    && d.tier == *tier
                    && scope.map_or(true, |s| d.scope == s)
            })
            .collect()
    }

    /// Add a tag to a memory document (idempotent). Persists to redb.
    ///
    /// Returns `true` if the tag was newly added, `false` if already present
    /// or the doc was not found.
    pub async fn tag_doc(&mut self, id: &str, tag: &str) -> Result<bool> {
        let idx = match self.docs.iter().position(|d| d.id == id) {
            Some(i) => i,
            None => return Ok(false),
        };
        let doc = &mut self.docs[idx];
        let tag_owned = tag.to_owned();
        if doc.tags.contains(&tag_owned) {
            return Ok(false);
        }
        doc.tags.push(tag_owned);
        self.persist_doc(idx)?;
        Ok(true)
    }

    /// Persist in-memory changes to an existing doc back to redb.
    fn persist_doc(&self, idx: usize) -> Result<()> {
        let doc = &self.docs[idx];
        let serialized = serialize_doc(doc)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(REDB_TABLE)?;
            table.insert(doc.id.as_str(), serialized.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Vector math helpers
// ---------------------------------------------------------------------------

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

fn serialize_doc(doc: &MemoryDoc) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(doc).context("serialize memory doc")?;
    let vec_count = doc.vector.len() as u32;
    let mut buf = Vec::with_capacity(4 + doc.vector.len() * 4 + json.len());
    buf.extend_from_slice(&vec_count.to_le_bytes());
    for &f in &doc.vector {
        buf.extend_from_slice(&f.to_le_bytes());
    }
    buf.extend_from_slice(&json);
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Embedder selection
// ---------------------------------------------------------------------------

/// Add a doc to a shared `MemoryStore` without stalling other readers /
/// writers on the BERT inference. The expensive `embedder.embed` calls
/// happen between two brief lock windows: one to grab the embedder Arcs,
/// one to commit the persisted doc + HNSW insert.
///
/// Use this from any path that holds an `Arc<Mutex<MemoryStore>>` shared
/// across tasks. Single-owner stores (CLI tools that opened a fresh store
/// for one shot) can keep using `MemoryStore::add` directly.
pub async fn add_off_lock(
    mem_arc: &Arc<tokio::sync::Mutex<MemoryStore>>,
    doc: MemoryDoc,
) -> Result<()> {
    let (primary_embedder, secondary_embedder) = {
        let mem = mem_arc.lock().await;
        mem.embedders_for_dual_write()
    };
    let primary_vec = primary_embedder.embed(&doc.text);
    let secondary_vec = secondary_embedder.map(|e| e.embed(&doc.text));
    let mut mem = mem_arc.lock().await;
    mem.add_pre_embedded(doc, primary_vec, secondary_vec).await
}

fn choose_embedder(
    cfg: Option<&MemorySearchConfig>,
    model_dir: Option<&Path>,
) -> Arc<dyn Embedder> {
    let provider = cfg.and_then(|c| c.provider.as_deref()).unwrap_or("local");

    match provider {
        "openai" => {
            let api_key = cfg
                .and_then(|c| c.api_key.as_ref())
                .and_then(|s| s.resolve_early())
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());

            match api_key {
                Some(key) => {
                    let model = cfg.and_then(|c| c.model.clone());
                    let base_url = cfg.and_then(|c| c.base_url.clone());
                    info!(
                        model = model.as_deref().unwrap_or(OPENAI_DEFAULT_MODEL),
                        "using OpenAI embedding provider"
                    );
                    Arc::new(EmbedderBackend::OpenAi(OpenAiEmbedder::new(
                        key, model, base_url,
                    )))
                }
                None => {
                    warn!(
                        "openai embedding provider selected but no API key found; falling back to FNV"
                    );
                    Arc::new(EmbedderBackend::Fnv(FnvEmbedder::new(DEFAULT_EMBED_DIM)))
                }
            }
        }

        "ollama" => {
            let model = cfg.and_then(|c| c.model.clone());
            let base_url = cfg.and_then(|c| c.base_url.clone());
            info!(
                model = model.as_deref().unwrap_or(OLLAMA_DEFAULT_MODEL),
                base_url = base_url.as_deref().unwrap_or(OLLAMA_DEFAULT_URL),
                "using Ollama embedding provider"
            );
            Arc::new(EmbedderBackend::Ollama(OllamaEmbedder::new(
                model, base_url,
            )))
        }

        _ => {
            if let Some(dir) = model_dir {
                if dir.join("config.json").exists() {
                    match LocalBgeEmbedder::load(dir) {
                        Ok(e) => {
                            info!("BGE-Small embedder loaded from {}", dir.display());
                            return Arc::new(EmbedderBackend::Local(e));
                        }
                        Err(e) => {
                            warn!("BGE-Small load failed ({e:#}), using FNV fallback");
                        }
                    }
                } else {
                    warn!(
                        "model dir {} not found, using FNV fallback (semantic search disabled)",
                        dir.display()
                    );
                }
            } else {
                debug!("no model dir configured, using FNV pseudo-embedding");
            }
            Arc::new(EmbedderBackend::Fnv(FnvEmbedder::new(DEFAULT_EMBED_DIM)))
        }
    }
}

#[cfg(test)]
mod swap_tests {
    use super::*;

    fn doc(id: &str, text: &str) -> MemoryDoc {
        MemoryDoc {
            id: id.into(),
            scope: "test".into(),
            kind: "note".into(),
            text: text.into(),
            vector: Vec::new(),
            created_at: 0,
            accessed_at: 0,
            access_count: 0,
            importance: 0.5,
            tier: MemDocTier::default(),
            abstract_text: None,
            overview_text: None,
            tags: Vec::new(),
            pinned: false,
        }
    }

    /// Hand-rolled embedder so tests don't depend on BGE/FNV: each text → a
    /// fixed-dim vector seeded by the first byte (deterministic & cheap).
    struct StubEmbedder { dim: i32, seed_bias: f32 }
    impl Embedder for StubEmbedder {
        fn embed(&self, text: &str) -> Vec<f32> {
            let bias = text.bytes().next().map(|b| b as f32 / 255.0).unwrap_or(0.0)
                + self.seed_bias;
            vec![bias; self.dim as usize]
        }
        fn dimension(&self) -> i32 { self.dim }
    }

    async fn open_temp_store() -> (MemoryStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = MemoryStore::open(tmp.path(), None, MemoryTier::High, None)
            .await
            .expect("open");
        (store, tmp)
    }

    #[tokio::test]
    async fn full_swap_lifecycle() {
        let (mut store, _tmp) = open_temp_store().await;
        for (i, text) in ["alpha", "beta", "gamma"].iter().enumerate() {
            let mut d = doc(&format!("d{i}"), text);
            d.created_at = 1;
            store.add(d).await.expect("add");
        }
        assert_eq!(store.is_migrating(), false);

        let new_emb: Arc<dyn Embedder> = Arc::new(StubEmbedder { dim: 8, seed_bias: 0.1 });
        store.begin_swap(Arc::clone(&new_emb)).expect("begin_swap");
        assert!(store.is_migrating());

        // Drain pending in batches; tests the snapshot/apply flow.
        loop {
            let pending = store.swap_pending(2);
            if pending.is_empty() { break; }
            let batch: Vec<_> = pending
                .into_iter()
                .map(|(i, t)| (i, new_emb.embed(&t)))
                .collect();
            store.swap_apply_batch(batch).expect("apply");
        }

        let migrated = store.commit_swap().expect("commit");
        assert_eq!(migrated, 3);
        assert!(!store.is_migrating());
        assert_eq!(store.embed_dim(), 8);
        // After commit, every doc carries the new vector.
        for d in &store.docs {
            assert_eq!(d.vector.len(), 8);
        }
    }

    #[tokio::test]
    async fn add_during_migration_dual_writes() {
        let (mut store, _tmp) = open_temp_store().await;
        store.add(doc("d0", "first")).await.unwrap();

        let new_emb: Arc<dyn Embedder> = Arc::new(StubEmbedder { dim: 8, seed_bias: 0.0 });
        store.begin_swap(Arc::clone(&new_emb)).unwrap();
        // Drain initial pending.
        let pending = store.swap_pending(10);
        let batch: Vec<_> = pending.into_iter().map(|(i, t)| (i, new_emb.embed(&t))).collect();
        store.swap_apply_batch(batch).unwrap();

        // Now add a doc mid-migration — it should auto-populate both indexes.
        store.add(doc("d1", "second")).await.unwrap();
        assert!(store.swap_pending(10).is_empty(), "dual-write should leave nothing pending");

        let migrated = store.commit_swap().unwrap();
        assert_eq!(migrated, 2);
    }

    /// `add_off_lock` is the production hot path — it's the only thing that
    /// keeps a shared `Mutex<MemoryStore>` from stalling every concurrent
    /// reader on a multi-second BERT inference. Verify it produces the same
    /// final state as the in-line `add()` for both the no-swap and the
    /// in-flight-migration cases.
    #[tokio::test]
    async fn add_off_lock_matches_add() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = MemoryStore::open(tmp.path(), None, MemoryTier::High, None)
            .await
            .expect("open");
        let mem = std::sync::Arc::new(tokio::sync::Mutex::new(store));

        // Plain add path (no swap).
        super::add_off_lock(&mem, doc("d0", "alpha")).await.unwrap();
        super::add_off_lock(&mem, doc("d1", "beta")).await.unwrap();
        {
            let m = mem.lock().await;
            assert_eq!(m.docs.len(), 2);
            assert_eq!(m.docs[0].vector.len(), m.embed_dim() as usize);
            assert!(!m.docs[1].vector.iter().all(|x| *x == 0.0));
        }

        // Mid-swap dual-write path: begin_swap, then add through the off-lock
        // helper. Secondary index must pick up the new doc the same way the
        // in-line add does.
        let new_emb: Arc<dyn Embedder> = Arc::new(StubEmbedder { dim: 16, seed_bias: 0.5 });
        {
            let mut m = mem.lock().await;
            m.begin_swap(Arc::clone(&new_emb)).unwrap();
            // Drain pre-existing docs through the migration loop.
            loop {
                let pending = m.swap_pending(10);
                if pending.is_empty() {
                    break;
                }
                let batch: Vec<_> =
                    pending.into_iter().map(|(i, t)| (i, new_emb.embed(&t))).collect();
                m.swap_apply_batch(batch).unwrap();
            }
        }
        super::add_off_lock(&mem, doc("d2", "gamma")).await.unwrap();
        {
            let m = mem.lock().await;
            assert!(
                m.swap_pending(10).is_empty(),
                "off-lock add must dual-write — secondary should have nothing pending after"
            );
        }
        let migrated = mem.lock().await.commit_swap().unwrap();
        assert_eq!(migrated, 3, "all three docs end up in the new primary");
    }

    /// Stress test: 100 concurrent `add_off_lock` tasks against the same
    /// shared store. Verifies no doc is lost (all 100 land), no doc is
    /// double-counted (HashSet of IDs has 100 unique), and the lock-around-
    /// embed-around-lock dance doesn't deadlock or panic.
    #[tokio::test]
    async fn add_off_lock_concurrent_no_loss_no_dupe() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = MemoryStore::open(tmp.path(), None, MemoryTier::High, None)
            .await
            .expect("open");
        let mem = std::sync::Arc::new(tokio::sync::Mutex::new(store));

        const N: usize = 100;
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let mem = std::sync::Arc::clone(&mem);
            handles.push(tokio::spawn(async move {
                let d = doc(&format!("c{i}"), &format!("concurrent doc {i}"));
                super::add_off_lock(&mem, d).await
            }));
        }
        let mut errors = 0usize;
        for h in handles {
            match h.await.expect("task panicked") {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("add_off_lock returned error: {e:#}");
                    errors += 1;
                }
            }
        }
        assert_eq!(errors, 0, "no add_off_lock call should fail under concurrency");

        let m = mem.lock().await;
        let active: Vec<&MemoryDoc> = m.docs.iter().filter(|d| !d.id.is_empty()).collect();
        assert_eq!(active.len(), N, "all {N} concurrent adds must land");
        let unique_ids: std::collections::HashSet<&str> =
            active.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(unique_ids.len(), N, "no doc may be duplicated");
        for d in active {
            assert_eq!(
                d.vector.len(),
                m.embed_dim() as usize,
                "every persisted vector must match the active embedder dim"
            );
        }
    }

    #[tokio::test]
    async fn abort_swap_leaves_primary_untouched() {
        let (mut store, _tmp) = open_temp_store().await;
        store.add(doc("d0", "x")).await.unwrap();
        let original_dim = store.embed_dim();

        let new_emb: Arc<dyn Embedder> = Arc::new(StubEmbedder { dim: 16, seed_bias: 0.0 });
        store.begin_swap(new_emb).unwrap();
        store.abort_swap();
        assert!(!store.is_migrating());
        assert_eq!(store.embed_dim(), original_dim);
        assert_eq!(store.docs[0].vector.len(), original_dim as usize);
    }

    #[tokio::test]
    async fn double_begin_swap_errors() {
        let (mut store, _tmp) = open_temp_store().await;
        let new_emb: Arc<dyn Embedder> = Arc::new(StubEmbedder { dim: 4, seed_bias: 0.0 });
        store.begin_swap(Arc::clone(&new_emb)).unwrap();
        let err = store.begin_swap(new_emb).expect_err("second begin should fail");
        assert!(err.to_string().contains("swap is already in progress"));
    }

    /// Models the bge-small → bge-base upgrade path: docs persisted under the
    /// old embedder show up as `pending_migration_count` after re-open with a
    /// different-dim embedder. Driving the standard swap loop drains the
    /// counter and makes search return them again.
    #[tokio::test]
    async fn dim_mismatch_triggers_pending_migration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Round 1: open + insert under FNV (default 384-dim).
        {
            let mut store = MemoryStore::open(tmp.path(), None, MemoryTier::High, None)
                .await
                .expect("open");
            assert_eq!(store.pending_migration_count(), 0);
            store.add(doc("d0", "alpha")).await.unwrap();
            store.add(doc("d1", "beta")).await.unwrap();
        }
        // Round 2: re-open + migrate the store to a different embedder dim
        // (16) so the persisted vectors no longer match FNV's default 384.
        {
            let mut store = MemoryStore::open(tmp.path(), None, MemoryTier::High, None)
                .await
                .expect("reopen for migration");
            assert_eq!(store.pending_migration_count(), 0);
            let new_emb: Arc<dyn Embedder> =
                Arc::new(StubEmbedder { dim: 16, seed_bias: 0.0 });
            store.begin_swap(Arc::clone(&new_emb)).unwrap();
            loop {
                let pending = store.swap_pending(10);
                if pending.is_empty() {
                    break;
                }
                let batch: Vec<_> = pending
                    .into_iter()
                    .map(|(i, t)| (i, new_emb.embed(&t)))
                    .collect();
                store.swap_apply_batch(batch).unwrap();
            }
            assert_eq!(store.commit_swap().unwrap(), 2);
            assert_eq!(store.embed_dim(), 16);
            assert_eq!(store.pending_migration_count(), 0);
        }

        // Round 3: reopen with default FNV (384-dim) — docs now stored with
        // 16-dim vectors → pending_migration_count should flag both.
        let store = MemoryStore::open(tmp.path(), None, MemoryTier::High, None)
            .await
            .expect("reopen after dim change");
        assert_eq!(
            store.pending_migration_count(),
            2,
            "expected 2 docs flagged for migration after dim change 16 -> 384"
        );
    }
}
