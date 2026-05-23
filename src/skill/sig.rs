//! ed25519 verification of the hub's signed `meta.json`.
//!
//! The public key is PINNED here — it is public (not secret), and hardcoding it
//! makes the trust anchor deterministic and tamper-proof at build time. The
//! matching private key lives off-repo in the publish tooling
//! (`~/ai/scripts/hub-rsclaw-dist.py`, read from `RSCLAW_PRIVATE_KEY`).
//!
//! Trust model: a compromised hub/CDN can swap files and rewrite their hashes,
//! but cannot forge a signature without the private key. The client verifies
//! `meta.sig` against `HUB_PUBKEY` and refuses everything if it fails.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Pinned hub signing public key (raw 32-byte ed25519, base64).
const HUB_PUBKEY_B64: &str = "PInrfQ8KL5Mngm+VZOkq2HkMMHnwyfmtkSD+ZfiIHx0=";

fn b64(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .context("base64 decode")
}

/// Canonical signed payload — MUST stay byte-identical to the publisher
/// (`hub-rsclaw-dist.py`):
/// `rsclaw-hub-meta\nv1\n{version}\n{skills}\n{plugins}\n{tools}\n`.
/// A golden-vector test pins this against a signature produced by that script.
fn canonical_payload(version: &str, skills: &str, plugins: &str, tools: &str) -> Vec<u8> {
    format!("rsclaw-hub-meta\nv1\n{version}\n{skills}\n{plugins}\n{tools}\n").into_bytes()
}

/// Verify the hub meta signature against the pinned public key. Fail-closed:
/// an empty or invalid signature is an error (never skipped).
pub fn verify_meta_sig(
    version: &str,
    skills: &str,
    plugins: &str,
    tools: &str,
    sig_b64: &str,
) -> Result<()> {
    if sig_b64.trim().is_empty() {
        bail!("hub meta has no signature — refusing (fail-closed)");
    }
    let pk: [u8; 32] = b64(HUB_PUBKEY_B64)?
        .try_into()
        .map_err(|_| anyhow!("pinned HUB_PUBKEY is not 32 bytes"))?;
    let vk = VerifyingKey::from_bytes(&pk).context("pinned HUB_PUBKEY is invalid")?;
    let sig: [u8; 64] = b64(sig_b64)?
        .try_into()
        .map_err(|_| anyhow!("hub meta signature is not 64 bytes"))?;
    let sig = Signature::from_bytes(&sig);
    vk.verify(&canonical_payload(version, skills, plugins, tools), &sig)
        .context("hub meta signature verification failed")
}

/// Parse a hub `meta.json`, verify its signature against the pinned key, and
/// return the verified `(skills, plugins, tools)` sha256 hashes. Fail-closed.
pub fn verify_signed_meta(meta_json: &str) -> Result<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_str(meta_json).context("parse meta.json")?;
    let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).unwrap_or("");
    let (version, skills, plugins, tools, sig) = (
        s("/version"),
        s("/sha256/skills"),
        s("/sha256/plugins"),
        s("/sha256/tools"),
        s("/sig"),
    );
    verify_meta_sig(version, skills, plugins, tools, sig)?;
    Ok((skills.to_owned(), plugins.to_owned(), tools.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Produced by hub-rsclaw-dist.py (Python + openssl) signing the payload for
    // version=2026-01-01.000000, skills=AAAA, plugins=BBBB, tools=CCCC with the
    // private key matching HUB_PUBKEY. Cross-implementation golden vector: if the
    // Rust canonical_payload drifts by one byte from the Python signer, this fails.
    const GOLD_SIG: &str =
        "DGm3uiJwAeYeyi1km20QNFk8rrUD33QgAJYhgcuPlgF3JOXTUViZBDtkhys5D6wTfSvdBpgyAc7zWRuLdBCiCg==";

    #[test]
    fn verifies_golden_vector() {
        verify_meta_sig("2026-01-01.000000", "AAAA", "BBBB", "CCCC", GOLD_SIG)
            .expect("golden vector must verify");
    }

    #[test]
    fn rejects_tampered_hash() {
        assert!(verify_meta_sig("2026-01-01.000000", "AAAA", "BBBB", "XXXX", GOLD_SIG).is_err());
    }

    #[test]
    fn rejects_tampered_version() {
        assert!(verify_meta_sig("2026-01-01.000001", "AAAA", "BBBB", "CCCC", GOLD_SIG).is_err());
    }

    #[test]
    fn rejects_empty_sig() {
        assert!(verify_meta_sig("2026-01-01.000000", "AAAA", "BBBB", "CCCC", "").is_err());
    }

    #[test]
    fn rejects_garbage_sig() {
        assert!(
            verify_meta_sig("2026-01-01.000000", "AAAA", "BBBB", "CCCC", "not-base64!!").is_err()
        );
    }
}
