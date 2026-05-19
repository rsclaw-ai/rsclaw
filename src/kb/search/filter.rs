//! Retrieval filter. Single function `keep_doc` decides whether a
//! raw recall hit (chunk_id) should appear in the final result set.
//! All visibility/status/version logic lives here so retrieval can't
//! accidentally bypass it.

use crate::kb::model::{CallerScope, KbDoc, KbStatus};
use crate::kb::store::docs;
use anyhow::Result;
use redb::ReadTransaction;
use std::collections::HashSet;

#[derive(Clone, Debug, Default)]
pub struct SearchFilter {
    pub tags: Vec<String>,
    pub source_kind: Option<crate::kb::model::KbSourceKind>,
    pub doc_ids: Option<HashSet<String>>,
    pub require_entities: Vec<String>,
}

pub fn keep_doc(doc: &KbDoc, scope: &CallerScope, filter: &SearchFilter) -> bool {
    if !doc.visible_to(scope) {
        return false;
    }
    if doc.status != KbStatus::Active {
        return false;
    }
    if let Some(kind) = filter.source_kind {
        if doc.source_kind != kind {
            return false;
        }
    }
    if !filter.tags.is_empty() {
        let docset: HashSet<&str> = doc.tags.iter().map(String::as_str).collect();
        if !filter.tags.iter().any(|t| docset.contains(t.as_str())) {
            return false;
        }
    }
    if let Some(ids) = &filter.doc_ids {
        if !ids.contains(&doc.id) {
            return false;
        }
    }
    true
}

/// Whether `doc` is the latest version pointed at by `kb_doc_latest_version`.
pub fn is_latest_version(rtx: &ReadTransaction, doc: &KbDoc) -> Result<bool> {
    match docs::latest_version(rtx, &doc.logical_source_id)? {
        Some(ptr) => Ok(ptr.doc_id == doc.id),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::{KbSource, KbSourceKind, KbStatus, KbVisibility, VersionPointer};
    use crate::kb::store::open_db;
    use serde_json::Value;
    use tempfile::TempDir;

    fn sample(id: &str, vis: KbVisibility, status: KbStatus, tags: Vec<String>) -> KbDoc {
        KbDoc {
            id: id.into(),
            logical_source_id: "lsid".into(),
            source: KbSource::Doc { path: "/x".into() },
            source_kind: KbSourceKind::Doc,
            title: "T".into(),
            mime: "text/markdown".into(),
            raw_sha256: "sha".into(),
            markdown_path: "md/doc/x.md".into(),
            markdown_sha256: "md".into(),
            raw_path: None,
            owner_user_id: None,
            created_at: 0,
            updated_at: 0,
            version: 1,
            status,
            visibility: vis,
            tags,
            meta: Value::Null,
        }
    }

    #[test]
    fn keep_doc_global_visible_to_anyone() {
        let d = sample("d1", KbVisibility::Global, KbStatus::Active, vec![]);
        let f = SearchFilter::default();
        assert!(keep_doc(&d, &CallerScope::default(), &f));
    }

    #[test]
    fn keep_doc_tombstoned_filtered() {
        let d = sample("d1", KbVisibility::Global, KbStatus::Tombstoned, vec![]);
        let f = SearchFilter::default();
        assert!(!keep_doc(&d, &CallerScope::default(), &f));
    }

    #[test]
    fn keep_doc_tag_filter() {
        let d = sample("d1", KbVisibility::Global, KbStatus::Active, vec!["work".into()]);
        let mut f = SearchFilter::default();
        f.tags = vec!["work".into()];
        assert!(keep_doc(&d, &CallerScope::default(), &f));
        f.tags = vec!["personal".into()];
        assert!(!keep_doc(&d, &CallerScope::default(), &f));
    }

    #[test]
    fn keep_doc_doc_id_filter() {
        let d = sample("d1", KbVisibility::Global, KbStatus::Active, vec![]);
        let mut f = SearchFilter::default();
        f.doc_ids = Some(["d1".into()].into());
        assert!(keep_doc(&d, &CallerScope::default(), &f));
        f.doc_ids = Some(["other".into()].into());
        assert!(!keep_doc(&d, &CallerScope::default(), &f));
    }

    #[test]
    fn keep_doc_private_requires_owner() {
        let mut d = sample("d1", KbVisibility::Private, KbStatus::Active, vec![]);
        d.owner_user_id = Some("u1".into());
        let scope_match = CallerScope { user_id: Some("u1".into()), ..Default::default() };
        let scope_other = CallerScope { user_id: Some("u2".into()), ..Default::default() };
        assert!(keep_doc(&d, &scope_match, &SearchFilter::default()));
        assert!(!keep_doc(&d, &scope_other, &SearchFilter::default()));
    }

    #[test]
    fn is_latest_version_picks_pointer() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            crate::kb::store::docs::set_latest_version(
                &wtx,
                "lsid",
                &VersionPointer { doc_id: "v2".into(), version: 2 },
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let mut v1 = sample("v1", KbVisibility::Global, KbStatus::Active, vec![]);
        v1.version = 1;
        let mut v2 = sample("v2", KbVisibility::Global, KbStatus::Active, vec![]);
        v2.version = 2;
        assert!(!is_latest_version(&rtx, &v1).unwrap());
        assert!(is_latest_version(&rtx, &v2).unwrap());
    }
}
