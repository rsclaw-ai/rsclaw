//! Version pointer for the `kb_doc_latest_version` table:
//! `logical_source_id → VersionPointer`. Lets retrieval find the
//! currently-active doc instance for a logical source. See spec §I.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VersionPointer {
    pub doc_id: String,
    pub version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let v = VersionPointer { doc_id: "01HXY".into(), version: 3 };
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<VersionPointer>(&s).unwrap(), v);
    }
}
