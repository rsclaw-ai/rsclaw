//! Plugin slot system.
//!
//! Slots allow plugins to replace core rsclaw subsystems:
//!   - `memory`         — replaces the built-in LanceDB memory backend
//!   - `context_engine` — replaces the context pruning / compaction logic
//!
//! Only one plugin may occupy each slot at a time.
//! Slot assignment is determined by `plugins.slots` in the config,
//! or by the first loaded plugin that declares the slot.

use std::sync::Arc;

use anyhow::{Result, bail};
use futures::future::BoxFuture;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Slot traits
// ---------------------------------------------------------------------------

/// A plugin filling the `memory` slot must implement this trait.
pub trait MemorySlot: Send + Sync {
    /// Store a memory record.
    fn store<'a>(
        &'a self,
        scope: &'a str,
        content: &'a str,
        metadata: Value,
    ) -> BoxFuture<'a, Result<String>>;
    /// Retrieve memories relevant to a query.
    fn recall<'a>(
        &'a self,
        scope: &'a str,
        query: &'a str,
        top_k: usize,
    ) -> BoxFuture<'a, Result<Vec<MemoryItem>>>;
    /// Delete a memory record by ID.
    fn forget<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// A plugin filling the `context_engine` slot can transform the message
/// list before it is sent to the LLM.
pub trait ContextEngineSlot: Send + Sync {
    /// Called before each LLM invocation.
    /// `messages` is the full conversation history (mutable).
    fn prune<'a>(
        &'a self,
        messages: &'a mut Vec<Value>,
        budget_tokens: u32,
    ) -> BoxFuture<'a, Result<()>>;
}

#[derive(Debug, Clone)]
pub struct MemoryItem {
    pub id: String,
    pub content: String,
    pub score: f32,
    pub metadata: Value,
}

// ---------------------------------------------------------------------------
// SlotRegistry
// ---------------------------------------------------------------------------

/// Holds the active plugin for each slot.
#[derive(Default)]
pub struct SlotRegistry {
    pub memory: Option<Arc<dyn MemorySlot>>,
    pub context_engine: Option<Arc<dyn ContextEngineSlot>>,
}

impl SlotRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the memory slot. Fails if already occupied.
    pub fn set_memory(&mut self, plugin: Arc<dyn MemorySlot>, plugin_name: &str) -> Result<()> {
        if self.memory.is_some() {
            bail!("memory slot already occupied; cannot register plugin `{plugin_name}`");
        }
        self.memory = Some(plugin);
        tracing::info!(plugin = plugin_name, "memory slot registered");
        Ok(())
    }

    /// Register the context_engine slot. Fails if already occupied.
    pub fn set_context_engine(
        &mut self,
        plugin: Arc<dyn ContextEngineSlot>,
        plugin_name: &str,
    ) -> Result<()> {
        if self.context_engine.is_some() {
            bail!("context_engine slot already occupied; cannot register `{plugin_name}`");
        }
        self.context_engine = Some(plugin);
        tracing::info!(plugin = plugin_name, "context_engine slot registered");
        Ok(())
    }

    /// Check whether the memory slot has been filled.
    pub fn has_memory(&self) -> bool {
        self.memory.is_some()
    }

    /// Check whether the context_engine slot has been filled.
    pub fn has_context_engine(&self) -> bool {
        self.context_engine.is_some()
    }
}

// ---------------------------------------------------------------------------
// MemoryStoreSlot — built-in MemorySlot backed by agent::memory::MemoryStore
// ---------------------------------------------------------------------------

/// Wraps the built-in LanceDB `MemoryStore` so it can fill the `memory` slot
/// and be used by external plugins that call through `SlotRegistry`.
pub struct MemoryStoreSlot {
    inner: Arc<tokio::sync::Mutex<crate::agent::memory::MemoryStore>>,
}

impl MemoryStoreSlot {
    pub fn new(store: Arc<tokio::sync::Mutex<crate::agent::memory::MemoryStore>>) -> Self {
        Self { inner: store }
    }
}

impl MemorySlot for MemoryStoreSlot {
    fn store<'a>(
        &'a self,
        scope: &'a str,
        content: &'a str,
        _metadata: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let id = uuid::Uuid::new_v4().to_string();
            let doc = crate::agent::memory::MemoryDoc {
                id: id.clone(),
                scope: scope.to_owned(),
                kind: "note".to_owned(),
                text: content.to_owned(),
                vector: vec![],
                created_at: 0,
                accessed_at: 0,
                access_count: 0,
                importance: 0.5,
                tier: Default::default(),
                abstract_text: None,
                overview_text: None,
                tags: vec![],
                pinned: false,
            };
            self.inner.lock().await.add(doc).await?;
            Ok(id)
        })
    }

    fn recall<'a>(
        &'a self,
        scope: &'a str,
        query: &'a str,
        top_k: usize,
    ) -> BoxFuture<'a, Result<Vec<MemoryItem>>> {
        Box::pin(async move {
            let mut store = self.inner.lock().await;
            let docs = store.search(query, Some(scope), top_k).await?;
            Ok(docs
                .into_iter()
                .map(|d| MemoryItem {
                    id: d.id,
                    content: d.text,
                    score: 1.0,
                    metadata: Value::Null,
                })
                .collect())
        })
    }

    fn forget<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.inner.lock().await.delete(id).await })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyMemory;

    impl MemorySlot for DummyMemory {
        fn store<'a>(
            &'a self,
            _scope: &'a str,
            _content: &'a str,
            _meta: Value,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async move { Ok("id-1".to_owned()) })
        }
        fn recall<'a>(
            &'a self,
            _scope: &'a str,
            _query: &'a str,
            _k: usize,
        ) -> BoxFuture<'a, Result<Vec<MemoryItem>>> {
            Box::pin(async move { Ok(vec![]) })
        }
        fn forget<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    #[test]
    fn register_memory_slot() {
        let mut reg = SlotRegistry::new();
        assert!(!reg.has_memory());
        reg.set_memory(Arc::new(DummyMemory), "dummy")
            .expect("register");
        assert!(reg.has_memory());
    }

    #[test]
    fn double_register_memory_slot_fails() {
        let mut reg = SlotRegistry::new();
        reg.set_memory(Arc::new(DummyMemory), "first")
            .expect("first");
        assert!(reg.set_memory(Arc::new(DummyMemory), "second").is_err());
    }
}
