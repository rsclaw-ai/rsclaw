//! Context management — pruning, budget trimming, compaction helpers.
//!
//! Extracted from `runtime.rs` to reduce file size.

use crate::{
    config::schema::ContextPruningConfig,
    provider::{
        failover::FailoverManager, registry::ProviderRegistry, ContentPart, LlmRequest, Message,
        MessageContent, Role, StreamEvent, ToolDef,
    },
};
use futures::StreamExt as _;
use std::sync::Arc;

/// Estimate token count for mixed-language text.
/// - ASCII/Latin: ~4 chars per token
/// - CJK (Chinese/Japanese/Korean): ~1.5 chars per token
/// - Other Unicode: ~2 chars per token
pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii_chars = 0usize;
    let mut cjk_chars = 0usize;
    let mut other_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3400}'..='\u{4DBF}').contains(&ch)
            || ('\u{3000}'..='\u{303F}').contains(&ch)
            || ('\u{FF00}'..='\u{FFEF}').contains(&ch)
            || ('\u{AC00}'..='\u{D7AF}').contains(&ch)
        {
            cjk_chars += 1;
        } else {
            other_chars += 1;
        }
    }
    ascii_chars / 4 + (cjk_chars * 2 + 1) / 3 + other_chars / 2 + 1
}

/// Prune the session message history in-place according to config.
///
/// Strategy (applied in order):
///   1. Hard-clear: if total chars > threshold, keep only the last user message.
///   2. Soft-trim: if total chars > tail_chars limit, remove old Tool messages.
pub(crate) fn apply_context_pruning(messages: &mut Vec<Message>, cfg: Option<&ContextPruningConfig>) {
    let Some(cfg) = cfg else { return };

    let total: usize = messages.iter().map(msg_chars).sum();

    // Hard clear.
    if let Some(hc) = &cfg.hard_clear
        && hc.enabled.unwrap_or(false)
    {
        let threshold = hc.threshold.unwrap_or(200_000) as usize;
        if total > threshold {
            let last_user = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .cloned();
            messages.clear();
            if let Some(m) = last_user {
                messages.push(m);
            }
            return;
        }
    }

    // Soft trim.
    if let Some(st) = &cfg.soft_trim
        && st.enabled.unwrap_or(false)
    {
        let limit = st.tail_chars.unwrap_or(80_000) as usize;
        let min_prunable = cfg.min_prunable_tool_chars.unwrap_or(500) as usize;

        if total > limit {
            let mut chars_over = total - limit;
            let mut to_remove: Vec<usize> = Vec::new();
            for (i, msg) in messages.iter().enumerate() {
                if chars_over == 0 {
                    break;
                }
                if msg.role == Role::Tool {
                    let c = msg_chars(msg);
                    if c >= min_prunable {
                        to_remove.push(i);
                        chars_over = chars_over.saturating_sub(c);
                    }
                }
            }
            for i in to_remove.into_iter().rev() {
                messages.remove(i);
            }
        }
    }
}

/// Count characters in a message (used by pruning).
pub(crate) fn msg_chars(m: &Message) -> usize {
    match &m.content {
        MessageContent::Text(t) => t.len(),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => text.len(),
                _ => 50,
            })
            .sum(),
    }
}

/// Build a summary Message from the last 10 user/assistant messages (for /clear).
pub(crate) fn build_clear_summary(messages: &[Message]) -> Option<Message> {
    if messages.is_empty() { return None; }
    let recent: Vec<&Message> = messages.iter().rev().take(10).rev().collect();
    let mut parts = Vec::new();
    for m in &recent {
        let role = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            _ => continue,
        };
        let text = match &m.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(ps) => ps.iter().filter_map(|p| {
                if let ContentPart::Text { text } = p { Some(text.as_str()) } else { None }
            }).collect::<Vec<_>>().join(" "),
        };
        if text.is_empty() { continue; }
        let truncated = if text.chars().count() > 200 {
            let idx = text.char_indices().nth(200).map(|(i, _)| i).unwrap_or(text.len());
            format!("{}...", &text[..idx])
        } else { text };
        parts.push(format!("{role}: {truncated}"));
    }
    if parts.is_empty() { return None; }
    Some(Message {
        role: Role::System,
        content: MessageContent::Text(
            format!("[Session summary before /clear]\n{}", parts.join("\n"))
        ),
    })
}

/// CJK-aware token estimate for a message (used by compaction threshold).
pub(crate) fn msg_tokens(m: &Message) -> usize {
    let text = match &m.content {
        MessageContent::Text(t) => t.as_str(),
        MessageContent::Parts(parts) => {
            return parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => estimate_tokens(text),
                    _ => 50,
                })
                .sum();
        }
    };
    estimate_tokens(text)
}

/// Trim session messages from oldest to newest so the total history fits
/// within the model's context budget.
///
/// Budget calculation:
///   reply_reserve  = max(context_budget * 20%, 2000)
///   system_tokens  = system_prompt.len() / 4
///   tools_tokens   = tools JSON size / 4
///   history_budget = context_budget - reply_reserve - system_tokens - tools_tokens
///
/// Always keeps at least the last 3 user-assistant pairs (6 messages).
pub(crate) fn apply_context_budget_trim(
    messages: &mut Vec<Message>,
    context_tokens: usize,
    system_prompt: &str,
    tools: &[ToolDef],
) {
    if messages.len() <= 6 {
        return;
    }

    let reply_reserve = (context_tokens / 5).max(2000);
    let sys_tokens = estimate_tokens(system_prompt);
    let tools_tokens = serde_json::to_string(tools)
        .map(|s| estimate_tokens(&s))
        .unwrap_or(0);

    let history_budget = context_tokens
        .saturating_sub(reply_reserve)
        .saturating_sub(sys_tokens)
        .saturating_sub(tools_tokens);

    let total_tokens: usize = messages.iter().map(msg_tokens).sum();
    if total_tokens <= history_budget {
        return;
    }

    // Trim from the front, keeping at least the last 6 messages.
    let min_keep = 6;
    let max_removable = messages.len().saturating_sub(min_keep);
    let mut removed_tokens: usize = 0;

    let mut remove_count = 0;
    for i in 0..max_removable {
        if total_tokens - removed_tokens <= history_budget {
            break;
        }
        removed_tokens += msg_tokens(&messages[i]);
        remove_count += 1;
    }

    if remove_count > 0 {
        tracing::info!(
            context_tokens,
            history_budget,
            total_tokens,
            removed = remove_count,
            remaining = messages.len() - remove_count,
            "context budget trim: removed {remove_count} oldest messages"
        );
        messages.drain(..remove_count);

        // Insert a system-like marker so the model knows history was truncated.
        messages.insert(0, Message {
            role: Role::User,
            content: MessageContent::Text(
                "[System: earlier conversation history was trimmed to fit context window. Continue naturally from the messages below.]".to_owned()
            ),
        });
        messages.insert(1, Message {
            role: Role::Assistant,
            content: MessageContent::Text("Understood.".to_owned()),
        });
    }
}

/// Compress an image for LLM: resize to max 1024px and convert to JPEG.
/// Uses the `image` crate (pure Rust, cross-platform).
/// Returns data URI or None if compression fails.
pub(crate) fn compress_image_for_llm(data_uri: &str) -> Option<String> {
    let b64 = data_uri
        .strip_prefix("data:image/png;base64,")
        .or_else(|| data_uri.strip_prefix("data:image/jpeg;base64,"))
        .or_else(|| data_uri.strip_prefix("data:image/webp;base64,"))
        .or_else(|| data_uri.strip_prefix("data:image/gif;base64,"))
        .unwrap_or(data_uri);

    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;

    // Skip if already small enough (<20KB) — fits in proxy fast lane.
    if bytes.len() < 20_000 {
        return Some(data_uri.to_owned());
    }

    let img = image::load_from_memory(&bytes).ok()?;

    // Resize so neither dimension exceeds 512px, preserving aspect ratio.
    // 512px is sufficient for vision models to describe image content.
    const MAX_DIM: u32 = 512;
    let (w, h) = (img.width(), img.height());
    let img = if w > MAX_DIM || h > MAX_DIM {
        img.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Encode to JPEG quality 70 — aggressive compression for description only.
    let mut buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 70);
    img.write_with_encoder(encoder).ok()?;
    let compressed = buf.into_inner();

    // If still over 20KB, try even smaller (256px + quality 50).
    let compressed = if compressed.len() > 20_000 {
        const SMALL_DIM: u32 = 256;
        let img = img.resize(SMALL_DIM, SMALL_DIM, image::imageops::FilterType::Lanczos3);
        let mut buf2 = std::io::Cursor::new(Vec::new());
        let encoder2 = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf2, 50);
        img.write_with_encoder(encoder2).ok()?;
        buf2.into_inner()
    } else {
        compressed
    };

    let b64 = base64::engine::general_purpose::STANDARD.encode(&compressed);
    tracing::debug!(
        original = bytes.len(),
        compressed = compressed.len(),
        "image compressed for LLM"
    );
    Some(format!("data:image/jpeg;base64,{b64}"))
}

// ---------------------------------------------------------------------------
// Key entity extraction — for pinned memory writes
// ---------------------------------------------------------------------------

/// An entity detected in text that should be pinned to memory.
pub struct KeyEntity {
    /// Human-readable type label, e.g. "phone_number".
    pub kind: &'static str,
    /// The exact value extracted (e.g. "18674030927").
    pub value: String,
    /// Full sentence to store as memory text.
    pub memory_text: String,
}

/// Extract key entities from text using deterministic char-level scanning.
///
/// Handles high-precision structured patterns:
/// - Chinese mobile phone numbers (11-digit, starts with 1[3-9])
/// - Chinese national ID cards (18-digit, last char may be X)
/// - Email addresses
/// - Chinese addresses (province/city/district/road/number patterns)
///
/// Semantic entities (name, birthday, age, zodiac, lucky number,
/// relationship) are extracted during compaction via the summary prompt.
///
/// Returns one `KeyEntity` per detected value (deduped).
pub(crate) fn extract_key_entities(text: &str) -> Vec<KeyEntity> {
    let mut entities: Vec<KeyEntity> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Collect all digit runs and their positions.
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if chars[i].is_ascii_digit() {
            let start = i;
            // Consume ID-card special case: 17 digits + optional X/x
            while i < n && chars[i].is_ascii_digit() {
                i += 1;
            }
            let run_end = i;
            // Allow trailing X for ID cards
            let trail_x = i < n && (chars[i] == 'X' || chars[i] == 'x');
            let run_len = run_end - start + if trail_x { 1 } else { 0 };
            let run: String = chars[start..run_end].iter().collect();

            // Check boundaries: prev/next char must not be a digit (avoid partial matches)
            let prev_digit = start > 0 && chars[start - 1].is_ascii_digit();
            let next_digit = i < n && !trail_x && chars[i].is_ascii_digit();
            if !prev_digit && !next_digit {
                // Chinese mobile: 11 digits, starts with 1[3-9]
                if run_len == 11 && run.starts_with('1') {
                    let d2 = run.chars().nth(1).unwrap_or('0');
                    if ('3'..='9').contains(&d2) && seen.insert(run.clone()) {
                        entities.push(KeyEntity {
                            kind: "phone_number",
                            memory_text: format!("用户手机号: {run}"),
                            value: run.clone(),
                        });
                    }
                }
                // Chinese national ID: 18 digits (or 17 digits + X)
                // run_len includes the trailing X: 17 digits + X = 18, or 18 pure digits.
                if run_len == 18 {
                    let val = if trail_x {
                        format!("{run}X")
                    } else {
                        run.clone()
                    };
                    if val.len() == 18 && seen.insert(val.clone()) {
                        entities.push(KeyEntity {
                            kind: "id_card",
                            memory_text: format!("用户身份证: {val}"),
                            value: val,
                        });
                    }
                }
            }
        } else {
            i += 1;
        }
    }

    // Email heuristic: find sequences matching word@word.word
    let mut j = 0;
    let bytes = text.as_bytes();
    while j < bytes.len() {
        if bytes[j] == b'@' && j > 0 && j + 1 < bytes.len() {
            // Walk back to find local-part
            let mut local_start = j;
            while local_start > 0
                && (bytes[local_start - 1].is_ascii_alphanumeric()
                    || b"._+-".contains(&bytes[local_start - 1]))
            {
                local_start -= 1;
            }
            // Walk forward to find domain
            let mut domain_end = j + 1;
            while domain_end < bytes.len()
                && (bytes[domain_end].is_ascii_alphanumeric()
                    || b".-".contains(&bytes[domain_end]))
            {
                domain_end += 1;
            }
            if j - local_start >= 1 && domain_end - j >= 4 {
                if let Ok(email) = std::str::from_utf8(&bytes[local_start..domain_end]) {
                    if email.contains('.') && seen.insert(email.to_owned()) {
                        entities.push(KeyEntity {
                            kind: "email",
                            memory_text: format!("用户邮箱: {email}"),
                            value: email.to_owned(),
                        });
                    }
                }
            }
        }
        j += 1;
    }

    // Chinese address detection (inspired by github.com/pupuk/addr).
    // Parses shipping-address style text: "收件人 电话 地址" in one pass.
    // Also detects standalone addresses with province/city/district markers.
    {
        const ADDR_MARKERS: &[&str] = &[
            "省", "市", "区", "县", "镇", "乡", "村",
            "路", "街", "道", "巷", "弄",
            "号", "栋", "楼", "层", "室", "单元",
        ];
        const ADDR_PREFIXES: &[&str] = &[
            "北京", "上海", "天津", "重庆", "广东", "浙江", "江苏", "山东",
            "河南", "河北", "湖北", "湖南", "四川", "福建", "安徽", "江西",
            "辽宁", "吉林", "黑龙江", "陕西", "山西", "云南", "贵州", "广西",
            "海南", "甘肃", "青海", "宁夏", "新疆", "西藏", "内蒙古",
        ];
        // Filter words commonly used as labels in address forms
        const FILTER_WORDS: &[&str] = &[
            "收货人", "收件人", "收货", "所在地区", "详细地址",
            "地址", "邮编", "电话", "手机", "手机号", "手机号码",
            "号码", "身份证号码", "身份证号", "身份证",
        ];

        for segment in text.split(|c: char| c == '\n' || c == '。') {
            let mut seg = segment.trim().to_owned();
            if seg.chars().count() < 5 || seg.chars().count() > 120 {
                continue;
            }

            // Strip filter words (address form labels)
            for fw in FILTER_WORDS {
                seg = seg.replace(fw, " ");
            }
            // Normalize separators
            for sep in &["：", ":", "；", ";", "，", ","] {
                seg = seg.replace(sep, " ");
            }
            // Collapse whitespace
            let parts: Vec<&str> = seg.split_whitespace().filter(|s| !s.is_empty()).collect();
            let joined = parts.join(" ");

            let marker_count = ADDR_MARKERS.iter().filter(|m| joined.contains(*m)).count();
            let has_prefix = ADDR_PREFIXES.iter().any(|p| joined.contains(p));
            // Require a digit in the segment — real addresses almost always have
            // numbers (门牌号, 楼层, 房间号). This filters out narrative text
            // like "车停在了一栋豪华公寓楼下" which has markers but no numbers.
            let has_digit = joined.chars().any(|c| c.is_ascii_digit());

            if marker_count < 2 && !(marker_count >= 1 && has_prefix) {
                continue;
            }
            if !has_digit && !has_prefix {
                // No digits and no province prefix — likely not a real address.
                continue;
            }

            // Found an address segment. Try to separate name/phone/address.
            // Strategy (from pupuk/addr): shortest token is likely the name,
            // 11-digit number is phone, rest is address.
            let mut addr_phone = String::new();
            let mut addr_name = String::new();
            let mut addr_parts = Vec::new();

            for part in &parts {
                let is_digits = part.chars().all(|c| c.is_ascii_digit() || c == '-');
                let digit_count = part.chars().filter(|c| c.is_ascii_digit()).count();
                if is_digits && digit_count >= 7 {
                    addr_phone = part.replace('-', "");
                } else {
                    addr_parts.push(*part);
                }
            }

            // Shortest remaining part is likely the name (2-4 chars Chinese)
            if addr_parts.len() >= 2 {
                let min_idx = addr_parts.iter().enumerate()
                    .min_by_key(|(_, p)| p.chars().count())
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                let candidate = addr_parts[min_idx];
                // Name heuristic: 2-4 CJK chars, no address markers
                let char_count = candidate.chars().count();
                let has_marker = ADDR_MARKERS.iter().any(|m| candidate.contains(m));
                if char_count >= 2 && char_count <= 4 && !has_marker {
                    addr_name = candidate.to_owned();
                    addr_parts.remove(min_idx);
                }
            }

            let addr_text = addr_parts.join("");
            if addr_text.is_empty() {
                continue;
            }

            // Store as composite shipping address if we have name or phone
            if (!addr_name.is_empty() || !addr_phone.is_empty()) && seen.insert(format!("addr:{addr_text}")) {
                let mut full = String::new();
                if !addr_name.is_empty() {
                    full.push_str(&addr_name);
                    full.push(' ');
                }
                if !addr_phone.is_empty() {
                    full.push_str(&addr_phone);
                    full.push(' ');
                }
                full.push_str(&addr_text);
                entities.push(KeyEntity {
                    kind: "address",
                    memory_text: format!("用户收货地址: {full}"),
                    value: full,
                });
            } else if seen.insert(format!("addr:{addr_text}")) {
                // Standalone address without name/phone
                entities.push(KeyEntity {
                    kind: "address",
                    memory_text: format!("用户地址: {addr_text}"),
                    value: addr_text,
                });
            }
        }
    }

    entities
}

/// Write key entities as pinned Core memories, deduplicating against existing entries.
///
/// For each entity:
/// 1. Search memory for an existing entry of the same kind.
/// 2. If found and the new value is a superset (or equal), skip or replace.
/// 3. Otherwise write as pinned=true, tier=Core, importance=0.95.
pub(crate) async fn write_entity_memories(
    mem: &std::sync::Arc<tokio::sync::Mutex<crate::agent::memory::MemoryStore>>,
    scope: &str,
    entities: Vec<KeyEntity>,
) {
    if entities.is_empty() {
        return;
    }
    // Hold lock for the entire search+add pair to avoid TOCTOU races.
    let mut guard = mem.lock().await;
    for entity in entities {
        // Dedup: skip if memory already contains this exact entity value.
        let already_exact = match guard.search(&entity.value, Some(scope), 10).await {
            Ok(results) => results.iter().any(|d| {
                d.kind == "entity" && d.text.contains(&entity.value)
            }),
            Err(_) => false,
        };
        if already_exact {
            tracing::debug!(kind = entity.kind, value = entity.value, "entity already pinned, skipping");
            continue;
        }
        let doc = crate::agent::memory::MemoryDoc {
            id: uuid::Uuid::new_v4().to_string(),
            scope: scope.to_owned(),
            kind: "entity".to_owned(),
            text: entity.memory_text,
            vector: vec![],
            created_at: 0,
            accessed_at: 0,
            access_count: 0,
            importance: 0.95,
            tier: crate::agent::memory::MemDocTier::Core,
            abstract_text: None,
            overview_text: None,
            tags: vec!["pinned".to_owned()],
            pinned: true,
        };
        match guard.add(doc).await {
            Ok(_) => tracing::info!(kind = entity.kind, value = entity.value, "entity pinned to memory"),
            Err(e) => tracing::warn!(kind = entity.kind, value = entity.value, "failed to pin entity: {e:#}"),
        }
    }
}

/// Extract semantic entities via a lightweight LLM call.
///
/// Covers: name, birthday, age, zodiac, lucky_number, address, relationship,
/// phone (with spaces/dashes), date, preference.
///
/// Uses a single user message, no system prompt, no tools, temperature=0.
/// Returns `Vec<KeyEntity>` parsed from the LLM's JSON array response.
pub(crate) async fn extract_entities_via_llm(
    text: &str,
    model: &str,
    failover: &mut FailoverManager,
    providers: &Arc<ProviderRegistry>,
) -> Vec<KeyEntity> {
    // Skip very short text — unlikely to contain personal info worth extracting.
    if text.chars().count() < 6 {
        return vec![];
    }

    let prompt = format!(
        "Extract personal information from the text below.\n\
         Return ONLY a JSON array. If nothing found, return [].\n\
         Format: [{{\"kind\":\"...\",\"value\":\"...\"}}, ...]\n\
         Allowed kinds: name, birthday, age, zodiac, lucky_number, phone, \
         id_card, email, address, relationship, date, preference\n\
         Rules:\n\
         - value must be the exact original text, never translate or reformat\n\
         - phone/id_card: strip spaces/dashes, digits only (plus trailing X for ID)\n\
         - Do NOT extract information about AI assistants, only about the human user\n\n\
         Text: {text}"
    );

    let req = LlmRequest {
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(prompt),
        }],
        tools: vec![],
        system: Some("Extract personal info. JSON array only. No explanation.".to_owned()),
        max_tokens: Some(512),
        temperature: Some(0.0),
        frequency_penalty: None,
        thinking_budget: None,
    };

    let mut stream = match failover.call(req, providers).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("entity LLM extraction call failed: {e:#}");
            return vec![];
        }
    };

    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(d)) => output.push_str(&d),
            Ok(StreamEvent::Done { .. }) => break,
            Ok(StreamEvent::Error(e)) => {
                tracing::debug!("entity LLM extraction stream error event: {e}");
                break;
            }
            Err(e) => {
                tracing::debug!("entity LLM extraction stream error: {e:#}");
                return vec![];
            }
            _ => {}
        }
    }

    parse_llm_entities(&output)
}

/// Parse the JSON array returned by the entity extraction LLM.
fn parse_llm_entities(raw: &str) -> Vec<KeyEntity> {
    // Find the JSON array boundaries — LLM may wrap in markdown fences.
    let start = match raw.find('[') {
        Some(i) => i,
        None => return vec![],
    };
    let end = match raw.rfind(']') {
        Some(i) => i + 1,
        None => return vec![],
    };
    let json_str = &raw[start..end];

    let arr: Vec<serde_json::Value> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("entity LLM JSON parse failed: {e}, raw={raw}");
            return vec![];
        }
    };

    let kind_to_label: &[(&str, &str)] = &[
        ("name", "用户姓名"),
        ("birthday", "用户生日"),
        ("age", "用户年龄"),
        ("zodiac", "用户星座"),
        ("lucky_number", "用户幸运数字"),
        ("phone", "用户手机号"),
        ("id_card", "用户身份证"),
        ("email", "用户邮箱"),
        ("address", "用户地址"),
        ("relationship", "用户关系"),
        ("date", "用户提到日期"),
        ("preference", "用户偏好"),
    ];

    let mut entities = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in arr {
        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let value = item.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if kind.is_empty() || value.is_empty() {
            continue;
        }
        let dedup_key = format!("{kind}:{value}");
        if !seen.insert(dedup_key) {
            continue;
        }
        let label = kind_to_label
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, l)| *l)
            .unwrap_or("用户信息");
        // Map to static kind str for KeyEntity.
        let static_kind: &'static str = match kind {
            "name" => "name",
            "birthday" => "birthday",
            "age" => "age",
            "zodiac" => "zodiac",
            "lucky_number" => "lucky_number",
            "phone" => "phone_number",
            "id_card" => "id_card",
            "email" => "email",
            "address" => "address",
            "relationship" => "relationship",
            "date" => "date",
            "preference" => "preference",
            _ => "other",
        };
        entities.push(KeyEntity {
            kind: static_kind,
            value: value.to_owned(),
            memory_text: format!("{label}: {value}"),
        });
    }
    entities
}

// ---------------------------------------------------------------------------
// Media description — convert images/videos to text for session storage
// ---------------------------------------------------------------------------

/// Describe an image using a vision-capable LLM.
///
/// Sends the image (base64 data URI) to the specified model and returns a
/// short text description. Used to convert images to text before storing in
/// the session, so that:
/// - Non-vision models can still "see" what was in the image
/// - Session history stays text-only (no base64 bloat)
/// - KV cache prefix is not disrupted
pub(crate) async fn describe_image_via_llm(
    image_data_uri: &str,
    model: &str,
    failover: &mut FailoverManager,
    providers: &Arc<ProviderRegistry>,
) -> Option<String> {
    let req = LlmRequest {
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "Describe this image concisely in 2-3 sentences. \
                           Focus on the main subject, key details, and any text visible. \
                           If it's a screenshot, describe the UI/content shown. \
                           Reply in the same language as any text in the image, \
                           or Chinese if no text is visible."
                        .to_owned(),
                },
                ContentPart::Image {
                    url: image_data_uri.to_owned(),
                },
            ]),
        }],
        tools: vec![],
        system: None,
        max_tokens: Some(300),
        temperature: Some(0.0),
        frequency_penalty: None,
        thinking_budget: None,
    };

    let mut stream = match failover.call(req, providers).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("image description LLM call failed: {e:#}");
            return None;
        }
    };

    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(d)) => output.push_str(&d),
            Ok(StreamEvent::Done { .. }) => break,
            Ok(StreamEvent::Error(e)) => {
                tracing::debug!("image description stream error: {e}");
                break;
            }
            Err(e) => {
                tracing::debug!("image description stream error: {e:#}");
                return None;
            }
            _ => {}
        }
    }

    let trimmed = output.trim().to_owned();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

/// Build a text description for a video attachment.
///
/// Returns a formatted string like:
/// - `[视频 12s] 转录: "大家好，今天..."` (if audio transcript available)
/// - `[视频 12s] (无音频内容)` (if no audio)
/// - `[视频] (无法获取时长)` (if duration unknown)
pub(crate) fn describe_video(duration_secs: Option<u64>, transcript: Option<&str>) -> String {
    let dur = match duration_secs {
        Some(s) => format!(" {s}s"),
        None => String::new(),
    };
    match transcript {
        Some(t) if !t.trim().is_empty() => {
            let preview: String = t.chars().take(500).collect();
            let ellipsis = if t.chars().count() > 500 { "..." } else { "" };
            format!("[视频{dur}] 转录: \"{preview}{ellipsis}\"")
        }
        _ => format!("[视频{dur}] (无音频内容)"),
    }
}

/// Compress tool results and tool-call arguments in-place to reduce token
/// count before LLM summarization during compaction.
///
/// For `Role::Tool` messages whose text content exceeds 200 chars, the content
/// is replaced with a one-line summary:
///   `[tool result] {first_line_or_truncated}... ({original_len} chars)`
///
/// For `Role::Assistant` messages containing `ContentPart::ToolUse` with
/// serialized arguments longer than 500 chars, the arguments are truncated
/// to 100 chars.
///
/// The last `preserve_tail` messages are left untouched so that recent
/// context is not degraded.
pub(crate) fn compress_tool_results(messages: &mut Vec<Message>, preserve_tail: usize) {
    if messages.len() <= preserve_tail {
        return;
    }
    let compress_end = messages.len() - preserve_tail;

    for msg in messages[..compress_end].iter_mut() {
        match msg.role {
            Role::Tool => {
                // Compress long tool-result text messages.
                if let MessageContent::Text(ref text) = msg.content {
                    if text.len() > 200 {
                        let original_len = text.len();
                        let first_line = text.lines().next().unwrap_or(text);
                        let summary: String = first_line.chars().take(100).collect();
                        let ellipsis = if first_line.chars().count() > 100 { "..." } else { "" };
                        msg.content = MessageContent::Text(format!(
                            "[tool result] {summary}{ellipsis} ({original_len} chars)"
                        ));
                    }
                }
                // Also handle Parts-based tool results.
                if let MessageContent::Parts(ref mut parts) = msg.content {
                    for part in parts.iter_mut() {
                        if let ContentPart::ToolResult { content, .. } = part {
                            if content.len() > 200 {
                                let original_len = content.len();
                                let first_line = content.lines().next().unwrap_or(content);
                                let summary: String = first_line.chars().take(100).collect();
                                let ellipsis = if first_line.chars().count() > 100 { "..." } else { "" };
                                *content = format!(
                                    "[tool result] {summary}{ellipsis} ({original_len} chars)"
                                );
                            }
                        }
                    }
                }
            }
            Role::Assistant => {
                // Truncate long tool-call arguments.
                if let MessageContent::Parts(ref mut parts) = msg.content {
                    for part in parts.iter_mut() {
                        if let ContentPart::ToolUse { input, .. } = part {
                            let serialized = serde_json::to_string(&input).unwrap_or_default();
                            if serialized.len() > 500 {
                                let truncated: String = serialized.chars().take(100).collect();
                                // Replace with a JSON string value containing the truncated form.
                                *input = serde_json::Value::String(format!("{truncated}..."));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Build a text description for a generic file attachment.
pub(crate) fn describe_file(filename: &str, mime_type: &str) -> String {
    if mime_type.starts_with("audio/") {
        format!("[音频: {filename}]")
    } else if mime_type.starts_with("text/")
        || mime_type.contains("json")
        || mime_type.contains("xml")
        || mime_type.contains("javascript")
    {
        format!("[文件: {filename}] (文本文件，可用 read_file 读取)")
    } else if mime_type.contains("pdf")
        || mime_type.contains("word")
        || mime_type.contains("spreadsheet")
        || mime_type.contains("presentation")
    {
        format!("[文件: {filename}] (文档，可用 doc 工具读取)")
    } else {
        format!("[文件: {filename}] ({mime_type})")
    }
}
