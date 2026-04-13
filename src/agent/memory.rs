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

use anyhow::{Context, Result};
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
}

impl MemoryDoc {
    /// P2: Weibull stretched-exponential decay replacing simple decay.
    pub fn relevance_score(&self) -> f32 {
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
        let lambda = 0.693_147_2_f32 / effective_hl; // ln(2)
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
    pub fn evaluate_tier_transition(&mut self) {
        let score = self.relevance_score();

        // Promote to Core: access_count >= 10 AND importance >= 0.8
        if self.access_count >= 10 && self.importance >= 0.8 {
            self.tier = MemDocTier::Core;
            return;
        }

        // Demote to Peripheral: relevance_score < 0.15 OR (age > 60 days AND access_count < 3)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let age_days = ((now - self.created_at).max(0) as f32) / 86_400.0;
        if score < 0.15 || (age_days > 60.0 && self.access_count < 3) {
            self.tier = MemDocTier::Peripheral;
            return;
        }

        // Promote to Working: access_count >= 3 AND relevance_score >= 0.4
        if self.access_count >= 3 && score >= 0.4 {
            if self.tier == MemDocTier::Peripheral {
                self.tier = MemDocTier::Working;
            }
        }
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

trait Embedder: Send + Sync {
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
    fn load(model_dir: &Path) -> Result<Self> {
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

        let ids: Vec<u32> = encoding.get_ids().to_vec();
        let type_ids: Vec<u32> = encoding.get_type_ids().to_vec();
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

pub struct MemoryStore {
    db: redb::Database,
    hnsw: Hnsw<'static, f32, DistCosine>,
    /// All docs in insertion order. HNSW DataId == index into this vec.
    docs: Vec<MemoryDoc>,
    embedder: Arc<dyn Embedder>,
    embed_dim: i32,
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
            let _ = write.open_table(REDB_TABLE);
            let _ = write.commit();
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
                "memory: dimension mismatch detected, auto-reindexing"
            );
        }

        if !docs.is_empty() {
            info!(count = docs.len(), "memory store loaded from redb");
        }

        let mut store = Self {
            db,
            hnsw,
            docs,
            embedder,
            embed_dim,
        };

        // Auto-reindex if any docs had mismatched vector dimensions.
        if skipped > 0 {
            match store.reindex().await {
                Ok(n) => info!(count = n, "auto-reindex complete"),
                Err(e) => warn!("auto-reindex failed: {e:#}"),
            }
        }

        Ok(store)
    }

    pub async fn add(&mut self, mut doc: MemoryDoc) -> Result<()> {
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

        doc.vector = self.embedder.embed(&doc.text);

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
