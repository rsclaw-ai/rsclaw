//! Shared text-embedding stack — used by both the agent memory
//! subsystem (`src/agent/memory.rs`) and the knowledge base
//! (`src/kb/`). Extracted from `agent/memory.rs` so the two don't
//! each ship their own embedder.
//!
//! Backends (all behind the `Embedder` trait, hot-swappable):
//!   - `FnvEmbedder`     — deterministic hash projection, no model.
//!   - `LocalBgeEmbedder`— candle BGE (BERT) from a model dir.
//!   - `OpenAiEmbedder`  — OpenAI-compatible `/v1/embeddings` REST; point `base_url` at a GPU fleet running Qwen3-Embedding for remote acceleration.
//!   - `OllamaEmbedder`  — Ollama local REST API.
//!
//! `agent::memory` re-exports these names for backward compatibility,
//! so existing `crate::agent::memory::EmbedderBackend` paths keep
//! working.

use anyhow::{Context, Result};
use std::path::Path;
use tracing::warn;

/// Fallback embedding dimension when no model is loaded (FNV).
pub const DEFAULT_EMBED_DIM: i32 = 384;
/// Default OpenAI-compatible embedding model name.
pub const OPENAI_DEFAULT_MODEL: &str = "text-embedding-3-small";
/// Default OpenAI-compatible API root. Override to point at a GPU fleet
/// serving `/v1/embeddings` (vLLM / SGLang / TEI / infinity).
pub const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Default Ollama embedding model.
pub const OLLAMA_DEFAULT_MODEL: &str = "nomic-embed-text";
/// Default Ollama REST base URL.
pub const OLLAMA_DEFAULT_URL: &str = "http://localhost:11434";

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
    pub fn new(dim: i32) -> Self {
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
    /// API root, e.g. `https://api.openai.com/v1` or a GPU fleet's
    /// OpenAI-compatible endpoint. `/embeddings` is appended at request time.
    base_url: String,
}

impl OpenAiEmbedder {
    /// `base_url` points at any OpenAI-compatible API root (defaults to
    /// OpenAI's). `dim_override` sets the output dimension for models
    /// `openai_model_dim` doesn't know (e.g. Qwen3-Embedding = 1024);
    /// when `None`, the dimension is derived from the model name.
    pub fn new(
        api_key: String,
        model: Option<String>,
        base_url: Option<String>,
        dim_override: Option<i32>,
    ) -> Self {
        let model = model.unwrap_or_else(|| OPENAI_DEFAULT_MODEL.to_owned());
        let dim = dim_override.unwrap_or_else(|| openai_model_dim(&model));
        let base_url = base_url.unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.to_owned());
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            dim,
            base_url,
        }
    }

    fn embed_blocking(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });

        let rt = tokio::runtime::Handle::try_current();
        let response_text = match rt {
            Ok(handle) => tokio::task::block_in_place(|| {
                handle.block_on(async {
                    self.client
                        .post(url.as_str())
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
                            .post(url.as_str())
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

/// Best-effort output dimension for a known OpenAI-compatible model name.
/// Callers should prefer an explicit `memorySearch.dimensions` when the
/// model isn't in this table.
pub fn openai_model_dim(model: &str) -> i32 {
    match model {
        "text-embedding-3-large" => 3072,
        "text-embedding-3-small" | "text-embedding-ada-002" => 1536,
        // Qwen3-Embedding family (served on the GPU fleet). Set
        // `memorySearch.dimensions` explicitly to be safe; this is a
        // best-effort default for the common 0.6B/4B 1024-dim case.
        m if m.starts_with("Qwen3-Embedding") || m.starts_with("qwen3-embedding") => 1024,
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
    pub fn new(model: Option<String>, base_url: Option<String>) -> Self {
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

/// Build the text to embed for a retrieval *query*. With no instruction
/// (or an empty one) the query is embedded as-is, preserving symmetric
/// behaviour. When an instruction is set, wrap it in the Qwen3-Embedding
/// query format (`Instruct: {task}\nQuery: {query}`), which materially
/// improves asymmetric retrieval quality. Document embeddings never use this.
pub fn format_query(instruction: Option<&str>, query: &str) -> String {
    match instruction {
        Some(task) if !task.is_empty() => format!("Instruct: {task}\nQuery: {query}"),
        _ => query.to_string(),
    }
}

#[cfg(test)]
mod query_instruction_tests {
    use super::*;

    #[test]
    fn none_returns_query_unchanged() {
        assert_eq!(format_query(None, "梯度下降"), "梯度下降");
    }

    #[test]
    fn some_wraps_in_qwen3_instruct_format() {
        assert_eq!(
            format_query(Some("Given a query, retrieve relevant passages"), "梯度下降"),
            "Instruct: Given a query, retrieve relevant passages\nQuery: 梯度下降"
        );
    }

    #[test]
    fn empty_instruction_string_is_treated_as_no_instruction() {
        // A configured-but-empty instruction must not produce a malformed
        // "Instruct: \nQuery: ..." prefix that pollutes the embedding.
        assert_eq!(format_query(Some(""), "梯度下降"), "梯度下降");
    }
}

