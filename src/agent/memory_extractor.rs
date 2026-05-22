//! L1 memory extractor (docs/memory-extraction-redesign.md, Phase 3).
//!
//! Phase 1 stopped raw-note pollution, but with only deterministic entity
//! extraction (phone/ID/email) it also dropped soft durable signal like
//! "我叫东升" or "我喜欢简洁的回答". This module adds an LLM distillation pass
//! over the user message (a trusted source) that classifies durable knowledge
//! into the fixed kind taxonomy and writes only the salient results.
//!
//! Runs spawned/async so the per-turn flash call never delays the reply, and
//! only on turns that pass the cheap [`salience_gate`] so most chit-chat / task
//! requests never trigger an LLM call. All failures are logged and swallowed —
//! extraction is best-effort and must never break the turn.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::memory::{MemDocTier, MemoryDoc, MemoryStore, add_off_lock};
use crate::provider::registry::ProviderRegistry;
use crate::skill::crystallizer::{acquire_distill_permit, distill_with_llm};

/// Max L1 items written per turn — guards against a runaway model dumping
/// dozens of "facts" from a single message.
const MAX_ITEMS_PER_TURN: usize = 6;
/// Confidence floor. The model scores each item 0..1; drop the unsure ones.
const MIN_CONFIDENCE: f32 = 0.55;

/// Caps concurrent in-flight L1 extractions. A burst of "remember…" messages
/// would otherwise spawn unbounded tasks all queued on the distill permit,
/// each retaining its `user_text`. We `try_acquire` (never wait): over the cap,
/// L1 is skipped for that turn — best-effort by design, deterministic entity
/// capture is unaffected.
static L1_INFLIGHT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// Cheap, deterministic pre-gate. Returns true only when the message looks like
/// a first-person self-description or an explicit remember request — the cases
/// that carry durable soft signal. Questions, greetings, acks, and most task
/// requests fall through with zero LLM cost. Precision over recall by design:
/// deterministic entity extraction still catches phone/ID/email regardless of
/// whether this gate fires.
pub(crate) fn salience_gate(text: &str) -> bool {
    let t = text.trim();
    let n = t.chars().count();
    if n < 4 || n > 4000 {
        return false;
    }
    let lower = t.to_lowercase();
    const SIGNALS: &[&str] = &[
        // Chinese self-description / preference / relationship / imperative-remember.
        "我叫", "我是", "我的", "我家", "我住", "住在", "我用", "我习惯", "喜欢", "讨厌",
        "我想要", "我希望", "我们公司", "我老婆", "我妻子", "我太太", "我老公", "我儿子",
        "我女儿", "我爸", "我妈", "我生日", "记住", "记一下", "记下", "帮我记", "偏好",
        "以后每次", "每次都",
        // English.
        "my name", "my wife", "my husband", "i live", "i'm from", "i am ", "i'm ", "i like",
        "i prefer", "i hate", "i use ", "i work", "call me", "remember that", "remember to",
    ];
    SIGNALS.iter().any(|s| lower.contains(s))
}

/// Allowed L1 kinds → (tier, importance, pinned). Mirrors §2/§3 of the redesign
/// doc. An L1 `entity` (e.g. a name) is pinned Core like the deterministic ones;
/// the rest are Working. `note` is intentionally absent — L1 never produces it.
fn kind_policy(kind: &str) -> Option<(MemDocTier, f32, bool)> {
    match kind {
        "entity" => Some((MemDocTier::Core, 0.9, true)),
        "preference" => Some((MemDocTier::Working, 0.72, false)),
        "procedure" => Some((MemDocTier::Working, 0.72, false)),
        "fact" => Some((MemDocTier::Working, 0.65, false)),
        "project_state" => Some((MemDocTier::Working, 0.65, false)),
        "relationship" => Some((MemDocTier::Working, 0.65, false)),
        _ => None,
    }
}

const EXTRACTION_PROMPT: &str = "Extract durable, long-term-worthy information from the user message below. Capture only stable, reusable knowledge: identity, contact details, preferences, stable facts, project state, relationships between people/orgs, and reusable procedures. Ignore greetings, questions, one-off task requests, and emotional venting.\nOutput a JSON array; each element: {\"kind\":\"<kind>\",\"text\":\"<concise third-person statement>\",\"confidence\":<number 0..1>}\nkind must be exactly one of: entity, preference, fact, project_state, relationship, procedure.\nWrite text in the third person and self-contained, preserving the user's original language for the content itself (e.g. \"User's name is 东升\", \"User prefers concise, direct answers\", \"User's release flow: cargo test, then check UI, then commit\").\nEvery item must include a numeric confidence in [0,1]. If nothing is worth remembering long-term, output an empty array []. Output ONLY JSON — no explanation, no code fences.\n\nUser message:\n";

/// Spawn-friendly L1 extraction. Resolves the flash model, distills the user
/// message into structured candidates, and writes the salient ones (deduped via
/// `find_exact`).
pub(crate) async fn extract_l1(
    mem: Arc<Mutex<MemoryStore>>,
    providers: Arc<ProviderRegistry>,
    flash_model: String,
    scope: String,
    user_text: String,
) {
    // Bound in-flight L1 work. Over the cap we skip rather than queue, so a
    // burst of messages can't pile up tasks or retained text.
    let Ok(_inflight) = L1_INFLIGHT.try_acquire() else {
        tracing::debug!("L1 extract: at concurrency cap, skipping turn");
        return;
    };
    let (provider_name, model_id) = providers.resolve_model(&flash_model);
    let provider_arc = match providers.get(provider_name) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(provider = provider_name, "L1 extract: provider not registered: {e:#}");
            return;
        }
    };
    let _permit = match acquire_distill_permit().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("L1 extract: permit acquire failed: {e:#}");
            return;
        }
    };
    let prompt = format!("{EXTRACTION_PROMPT}{user_text}");
    let raw = match distill_with_llm(&prompt, provider_arc, model_id.to_owned()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("L1 extract: LLM call failed: {e:#}");
            return;
        }
    };

    let items = parse_items(&raw);
    if items.is_empty() {
        tracing::debug!(scope = %scope, "L1 extract: nothing durable");
        return;
    }

    let mut written = 0usize;
    for item in items.into_iter().take(MAX_ITEMS_PER_TURN) {
        let Some((tier, importance, pinned)) = kind_policy(&item.kind) else {
            continue;
        };
        if item.confidence < MIN_CONFIDENCE {
            continue;
        }
        let text = item.text.trim().to_owned();
        if text.chars().count() < 3 {
            continue;
        }
        // Exact-text dedup against the same scope+kind (Phase 4 adds semantic
        // merge). Cheap insurance against the model re-emitting a known fact.
        let dup = {
            let guard = mem.lock().await;
            guard.find_exact(&scope, &item.kind, &text).is_some()
        };
        if dup {
            continue;
        }
        let tags = if pinned { vec!["pinned".to_owned()] } else { vec![] };
        let doc = MemoryDoc {
            id: uuid::Uuid::new_v4().to_string(),
            scope: scope.clone(),
            kind: item.kind.clone(),
            text,
            vector: vec![],
            created_at: 0,
            accessed_at: 0,
            access_count: 0,
            importance,
            tier,
            abstract_text: None,
            overview_text: None,
            tags,
            pinned,
        };
        match add_off_lock(&mem, doc).await {
            Ok(_) => written += 1,
            Err(e) => tracing::warn!(kind = %item.kind, "L1 extract: add failed: {e:#}"),
        }
    }
    if written > 0 {
        tracing::info!(scope = %scope, written, "L1 memories extracted");
    }
}

struct Item {
    kind: String,
    text: String,
    confidence: f32,
}

/// Parse model output into items. Tolerates code fences and either a JSON array
/// or one-object-per-line (JSON Lines). Unparseable content is skipped, never
/// fatal.
fn parse_items(raw: &str) -> Vec<Item> {
    let cleaned = strip_fences(raw);
    let mut out = Vec::new();
    // Try a single JSON array/value first.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&cleaned) {
        collect_value(&v, &mut out);
        if !out.is_empty() {
            return out;
        }
    }
    // Fall back to line-by-line JSON objects.
    for line in cleaned.lines() {
        let line = line.trim().trim_end_matches(',');
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            collect_value(&v, &mut out);
        }
    }
    out
}

fn collect_value(v: &serde_json::Value, out: &mut Vec<Item>) {
    match v {
        serde_json::Value::Array(arr) => {
            for el in arr {
                collect_value(el, out);
            }
        }
        serde_json::Value::Object(obj) => {
            let kind = obj.get("kind").and_then(|x| x.as_str()).unwrap_or("").to_owned();
            let text = obj.get("text").and_then(|x| x.as_str()).unwrap_or("").to_owned();
            // Require a real numeric confidence in [0,1]. Missing/garbage
            // confidence → 0.0, which fails MIN_CONFIDENCE and is dropped, so
            // adversarial output can't slip in unscored items.
            let confidence = match obj.get("confidence").and_then(|x| x.as_f64()) {
                Some(c) if c.is_finite() && (0.0..=1.0).contains(&c) => c as f32,
                _ => 0.0,
            };
            if !kind.is_empty() && !text.is_empty() {
                out.push(Item { kind, text, confidence });
            }
        }
        _ => {}
    }
}

fn strip_fences(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salience_gate_drops_chitchat_and_questions() {
        assert!(!salience_gate("在吗？"));
        assert!(!salience_gate("这个怎么搞啊"));
        assert!(!salience_gate("hi"));
        assert!(!salience_gate("帮我查一下今天天气")); // task request, no self-info
    }

    #[test]
    fn salience_gate_fires_on_self_description() {
        assert!(salience_gate("我叫东升"));
        assert!(salience_gate("我比较喜欢简洁直接的回答，别啰嗦"));
        assert!(salience_gate("记一下我的发布流程：先 cargo test"));
        assert!(salience_gate("My name is Dongsheng"));
    }

    #[test]
    fn parse_handles_array_fences_and_lines() {
        let arr = r#"[{"kind":"preference","text":"用户偏好简洁","confidence":0.8}]"#;
        let v = parse_items(arr);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, "preference");

        let fenced = "```json\n[{\"kind\":\"entity\",\"text\":\"用户叫东升\",\"confidence\":0.9}]\n```";
        assert_eq!(parse_items(fenced).len(), 1);

        let lines = "{\"kind\":\"fact\",\"text\":\"a\",\"confidence\":0.7}\n{\"kind\":\"preference\",\"text\":\"b\",\"confidence\":0.6}";
        assert_eq!(parse_items(lines).len(), 2);

        assert_eq!(parse_items("[]").len(), 0);
    }

    #[test]
    fn missing_or_bad_confidence_scores_zero_and_drops() {
        // No confidence field → 0.0 (will fail MIN_CONFIDENCE downstream).
        let no_conf = r#"[{"kind":"fact","text":"x"}]"#;
        let v = parse_items(no_conf);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].confidence, 0.0);

        // Out-of-range / non-numeric confidence → 0.0.
        let bad = r#"[{"kind":"fact","text":"x","confidence":"high"},{"kind":"fact","text":"y","confidence":5}]"#;
        for item in parse_items(bad) {
            assert_eq!(item.confidence, 0.0);
        }
    }

    #[test]
    fn kind_policy_rejects_note_and_unknown() {
        assert!(kind_policy("note").is_none());
        assert!(kind_policy("summary").is_none());
        assert!(kind_policy("garbage").is_none());
        assert!(kind_policy("preference").is_some());
    }
}
