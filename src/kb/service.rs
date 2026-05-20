//! `KnowledgeService` — gateway-facing facade over the KB store for the
//! desktop `/api/v1/knowledge` API.
//!
//! Collections are a tag veneer over the single KB store (see the project
//! note `kb-desktop-collections`): a collection is metadata here plus a
//! `collection:<id>` tag on its docs. There is no per-collection store or
//! embedder. P1 covers collection metadata CRUD; docs + search grow the
//! service in P2/P3.

use crate::kb::model::{collection_tag, KbCollection};
use crate::kb::store::collections;
use crate::kb::{KbPaths, KbStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Service-level errors the HTTP layer maps to status codes + error envelope.
#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("collection_not_found")]
    CollectionNotFound,
    #[error("duplicate_name")]
    DuplicateName,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type KResult<T> = Result<T, KnowledgeError>;

pub struct KnowledgeService {
    store: Arc<KbStore>,
    kb_root: PathBuf,
}

impl KnowledgeService {
    /// Open (or create) the KB store under `kb_root` (e.g. `<base>/kb`).
    pub fn open(kb_root: PathBuf) -> anyhow::Result<Self> {
        let paths = KbPaths::new(&kb_root);
        paths.ensure_layout()?;
        let store = Arc::new(KbStore::open(&kb_root.join("kb.redb"))?);
        Ok(Self { store, kb_root })
    }

    pub fn kb_root(&self) -> &Path {
        &self.kb_root
    }
    pub fn store(&self) -> &Arc<KbStore> {
        &self.store
    }

    /// All collections, newest first.
    pub fn list_collections(&self) -> KResult<Vec<KbCollection>> {
        let rtx = self.store.begin_read()?;
        let mut v = collections::list(&rtx)?;
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(v)
    }

    pub fn get_collection(&self, id: &str) -> KResult<KbCollection> {
        let rtx = self.store.begin_read()?;
        collections::get(&rtx, id)?.ok_or(KnowledgeError::CollectionNotFound)
    }

    pub fn create_collection(
        &self,
        name: &str,
        description: Option<String>,
        embed_model: Option<String>,
    ) -> KResult<KbCollection> {
        {
            let rtx = self.store.begin_read()?;
            if collections::find_by_name(&rtx, name)?.is_some() {
                return Err(KnowledgeError::DuplicateName);
            }
        }
        let now = now_ms();
        let c = KbCollection {
            id: format!("col_{}", ulid::Ulid::new().to_string().to_lowercase()),
            name: name.to_string(),
            description,
            embed_model,
            created_at: now,
            updated_at: now,
        };
        let wtx = self.store.begin_write()?;
        collections::put(&wtx, &c)?;
        wtx.commit().map_err(anyhow::Error::from)?;
        Ok(c)
    }

    /// Update name/description. `description = None` leaves it unchanged;
    /// `Some(None)` clears it; `Some(Some(x))` sets it. Renames keep the
    /// name index consistent and reject collisions.
    pub fn update_collection(
        &self,
        id: &str,
        name: Option<String>,
        description: Option<Option<String>>,
    ) -> KResult<KbCollection> {
        let mut c = self.get_collection(id)?;
        let old_name = c.name.clone();
        if let Some(new_name) = name {
            if new_name != c.name {
                let rtx = self.store.begin_read()?;
                if let Some(other) = collections::find_by_name(&rtx, &new_name)? {
                    if other != id {
                        return Err(KnowledgeError::DuplicateName);
                    }
                }
                c.name = new_name;
            }
        }
        if let Some(desc) = description {
            c.description = desc;
        }
        c.updated_at = now_ms();
        let wtx = self.store.begin_write()?;
        if c.name != old_name {
            collections::unindex_name(&wtx, &old_name)?;
        }
        collections::put(&wtx, &c)?;
        wtx.commit().map_err(anyhow::Error::from)?;
        Ok(c)
    }

    pub fn delete_collection(&self, id: &str) -> KResult<()> {
        self.get_collection(id)?; // 404 if absent
        let wtx = self.store.begin_write()?;
        collections::delete(&wtx, id)?;
        wtx.commit().map_err(anyhow::Error::from)?;
        // TODO(P2): cascade — remove docs tagged `collection_tag(id)` plus
        // their chunks/vectors, alongside the doc remove path.
        let _ = collection_tag(id);
        Ok(())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn svc() -> (TempDir, KnowledgeService) {
        let tmp = TempDir::new().unwrap();
        let s = KnowledgeService::open(tmp.path().join("kb")).unwrap();
        (tmp, s)
    }

    #[test]
    fn create_then_list_and_get() {
        let (_t, s) = svc();
        let c = s.create_collection("产品手册", Some("v3".into()), None).unwrap();
        assert!(c.id.starts_with("col_"));
        assert_eq!(s.get_collection(&c.id).unwrap().name, "产品手册");
        assert_eq!(s.list_collections().unwrap().len(), 1);
    }

    #[test]
    fn duplicate_name_rejected() {
        let (_t, s) = svc();
        s.create_collection("产品手册", None, None).unwrap();
        assert!(matches!(
            s.create_collection("产品手册", None, None),
            Err(KnowledgeError::DuplicateName)
        ));
    }

    #[test]
    fn rename_frees_old_name() {
        let (_t, s) = svc();
        let c = s.create_collection("旧名", None, None).unwrap();
        s.update_collection(&c.id, Some("新名".into()), None).unwrap();
        // old name is now reusable
        s.create_collection("旧名", None, None).unwrap();
        assert_eq!(s.get_collection(&c.id).unwrap().name, "新名");
    }

    #[test]
    fn delete_then_missing() {
        let (_t, s) = svc();
        let c = s.create_collection("x", None, None).unwrap();
        s.delete_collection(&c.id).unwrap();
        assert!(matches!(
            s.get_collection(&c.id),
            Err(KnowledgeError::CollectionNotFound)
        ));
        assert!(matches!(
            s.delete_collection(&c.id),
            Err(KnowledgeError::CollectionNotFound)
        ));
    }
}
