//! Doc-level types: lifecycle status, permission boundary
//! (`KbVisibility` + `CallerScope`), and the `KbDoc` record itself.
//!
//! See spec §1 + §K PermissionScope.

use serde::{Deserialize, Serialize};

use crate::model::{KbSource, KbSourceKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KbStatus {
    Active,
    Tombstoned,
    Updating,
}

/// Permission boundary for a `KbDoc`. See spec §K.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KbVisibility {
    Global,
    Agent { agent_id: String },
    Channel { channel_id: String },
    Private,
}

impl KbVisibility {
    /// Default visibility for a freshly-ingested doc, per source_kind.
    pub fn default_for(kind: KbSourceKind) -> Self {
        match kind {
            KbSourceKind::Doc | KbSourceKind::Url | KbSourceKind::Img => Self::Global,
            KbSourceKind::Mail => Self::Private,
            KbSourceKind::Chat => Self::Private, // narrowed at ingest time
        }
    }

    /// Check visibility against a caller scope. `owner_user_id` is the
    /// doc's `owner_user_id` field — REQUIRED for `Private` to match
    /// (otherwise Private docs would leak to any authenticated caller,
    /// violating spec §K). For non-Private variants, `owner_user_id`
    /// is ignored.
    ///
    /// Prefer `KbDoc::visible_to(scope)` from retrieval code so the
    /// owner argument is always paired with the visibility correctly.
    pub fn visible_to(&self, scope: &CallerScope, owner_user_id: Option<&str>) -> bool {
        match self {
            Self::Global => true,
            Self::Agent { agent_id } => scope.agent_id.as_deref() == Some(agent_id.as_str()),
            Self::Channel { channel_id } => {
                scope.channel_id.as_deref() == Some(channel_id.as_str())
            }
            Self::Private => match (owner_user_id, scope.user_id.as_deref()) {
                (Some(owner), Some(caller)) => owner == caller,
                _ => false,
            },
        }
    }
}

/// Identity of the caller making a retrieval request. Injected by
/// the agent runtime — agent code MUST NOT construct or modify this.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerScope {
    pub agent_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbDoc {
    pub id: String,                // ULID, per-ingest instance
    pub logical_source_id: String, // idempotency key (§I)
    pub source: KbSource,
    pub source_kind: KbSourceKind,
    pub title: String,
    pub mime: String,
    pub raw_sha256: String,
    pub markdown_path: String, // relative to kb_root
    pub markdown_sha256: String,
    pub raw_path: Option<String>,
    pub owner_user_id: Option<String>, // for Private visibility resolution
    pub created_at: i64,
    pub updated_at: i64,
    pub version: u32, // increments per re-ingest (§I)
    pub status: KbStatus,
    pub visibility: KbVisibility,
    pub tags: Vec<String>,
    pub meta: serde_json::Value,
}

impl KbDoc {
    /// Check whether `scope` can see this doc. Pairs `visibility` with
    /// `owner_user_id` so `Private` is matched correctly. Retrieval
    /// code MUST use this instead of calling `KbVisibility::visible_to`
    /// directly — passing the wrong owner is the most likely scope-leak.
    pub fn visible_to(&self, scope: &CallerScope) -> bool {
        self.visibility
            .visible_to(scope, self.owner_user_id.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_default_per_kind() {
        assert!(matches!(
            KbVisibility::default_for(KbSourceKind::Doc),
            KbVisibility::Global
        ));
        assert!(matches!(
            KbVisibility::default_for(KbSourceKind::Url),
            KbVisibility::Global
        ));
        assert!(matches!(
            KbVisibility::default_for(KbSourceKind::Mail),
            KbVisibility::Private
        ));
        assert!(matches!(
            KbVisibility::default_for(KbSourceKind::Chat),
            KbVisibility::Private
        ));
    }

    #[test]
    fn visibility_global_visible_to_anyone() {
        assert!(KbVisibility::Global.visible_to(&CallerScope::default(), None));
    }

    #[test]
    fn visibility_agent_filters_by_agent_id() {
        let v = KbVisibility::Agent {
            agent_id: "a1".into(),
        };
        assert!(v.visible_to(
            &CallerScope {
                agent_id: Some("a1".into()),
                ..Default::default()
            },
            None
        ));
        assert!(!v.visible_to(
            &CallerScope {
                agent_id: Some("a2".into()),
                ..Default::default()
            },
            None
        ));
        assert!(!v.visible_to(&CallerScope::default(), None));
    }

    #[test]
    fn visibility_channel_filters_by_channel_id() {
        let v = KbVisibility::Channel {
            channel_id: "c1".into(),
        };
        assert!(v.visible_to(
            &CallerScope {
                channel_id: Some("c1".into()),
                ..Default::default()
            },
            None
        ));
        assert!(!v.visible_to(
            &CallerScope {
                channel_id: Some("c2".into()),
                ..Default::default()
            },
            None
        ));
    }

    #[test]
    fn visibility_private_requires_matching_owner() {
        // owner=u1, caller=u1 → allowed
        assert!(KbVisibility::Private.visible_to(
            &CallerScope {
                user_id: Some("u1".into()),
                ..Default::default()
            },
            Some("u1"),
        ));
        // owner=u1, caller=u2 → DENIED (regression test for the bug
        // where Private leaked to any authenticated caller)
        assert!(!KbVisibility::Private.visible_to(
            &CallerScope {
                user_id: Some("u2".into()),
                ..Default::default()
            },
            Some("u1"),
        ));
        // No caller user_id → denied
        assert!(!KbVisibility::Private.visible_to(&CallerScope::default(), Some("u1")));
        // No owner → denied (defensive: a Private doc with no owner is unreachable)
        assert!(!KbVisibility::Private.visible_to(
            &CallerScope {
                user_id: Some("u1".into()),
                ..Default::default()
            },
            None,
        ));
    }

    #[test]
    fn kbdoc_visible_to_pairs_owner_with_visibility() {
        let mut d = sample_doc();
        d.visibility = KbVisibility::Private;
        d.owner_user_id = Some("u1".into());
        assert!(d.visible_to(&CallerScope {
            user_id: Some("u1".into()),
            ..Default::default()
        }));
        assert!(!d.visible_to(&CallerScope {
            user_id: Some("u2".into()),
            ..Default::default()
        }));
    }

    fn sample_doc() -> KbDoc {
        KbDoc {
            id: "01HXY".into(),
            logical_source_id: "file:sha256:abc".into(),
            source: KbSource::Doc {
                path: "/tmp/x".into(),
            },
            source_kind: KbSourceKind::Doc,
            title: "T".into(),
            mime: "text/markdown".into(),
            raw_sha256: "abc".into(),
            markdown_path: "md/doc/x--12345678.md".into(),
            markdown_sha256: "def".into(),
            raw_path: None,
            owner_user_id: None,
            created_at: 0,
            updated_at: 0,
            version: 1,
            status: KbStatus::Active,
            visibility: KbVisibility::Global,
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn doc_serde_roundtrip() {
        let d = sample_doc();
        let s = serde_json::to_string(&d).unwrap();
        let back: KbDoc = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }
}
