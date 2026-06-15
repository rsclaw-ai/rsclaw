//! Encode/decode helpers for redb value bytes. All Week 2 accessors
//! go through these so v2 can swap JSON for a compact binary codec
//! without touching every table accessor.

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).context("kb codec: encode")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).context("kb codec: decode")
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct X {
        a: u32,
        b: String,
    }

    #[test]
    fn roundtrip() {
        let x = X {
            a: 7,
            b: "hi".into(),
        };
        let bytes = encode(&x).unwrap();
        assert_eq!(decode::<X>(&bytes).unwrap(), x);
    }

    #[test]
    fn decode_corrupt_errors() {
        assert!(decode::<X>(b"not json").is_err());
    }
}
