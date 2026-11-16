//! How user-facing text names the managed cloud and its models.
//!
//! The stock brand shows the real ids (`rsclaw`, `rsclaw/rsclaw-agent-v2`):
//! its people call the API by them. Any other brand never shows the RsClaw
//! name, so the same ids read as the brand's cloud name and neutral model
//! names (「智能体模型 v2」, "Agent model v2"), the way the brand's app and
//! console name them. Only text is affected: config keys, provider ids, model
//! ids, headers and protocol strings keep their values.

use std::borrow::Cow;

/// What a build names the cloud: `stock` builds keep the ids.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Names {
    pub stock: bool,
    pub cloud_en: &'static str,
    pub cloud_zh: &'static str,
}

impl Names {
    fn current() -> Self {
        Self {
            stock: crate::ID == "rsclaw",
            cloud_en: crate::CLOUD_NAME_EN.unwrap_or("Cloud"),
            cloud_zh: crate::CLOUD_NAME_ZH.unwrap_or("云端模型"),
        }
    }

    fn cloud(&self, zh: bool) -> &'static str {
        if self.stock {
            "rsclaw"
        } else if zh {
            self.cloud_zh
        } else {
            self.cloud_en
        }
    }
}

/// True for the stock RsClaw brand.
pub fn is_stock() -> bool {
    Names::current().stock
}

/// The managed cloud in English prose (error prefixes, tool messages):
/// `rsclaw` in the stock brand, as that text has always read; otherwise the
/// brand's cloud name (`name_en` of the `rsclaw` provider in its defaults
/// override, else "Cloud").
pub fn cloud_name() -> &'static str {
    Names::current().cloud(false)
}

/// [`cloud_name`] for Chinese text (`name_zh`, else 「云端模型」).
pub fn cloud_name_zh() -> &'static str {
    Names::current().cloud(true)
}

/// A provider id as text shows it: unchanged in the stock brand; the `rsclaw`
/// provider reads as the brand's cloud name.
pub fn display_provider(name: &str, zh: bool) -> Cow<'_, str> {
    display_provider_with(name, zh, Names::current())
}

/// A model ref (`provider/model`) or bare model id as text shows it:
/// unchanged in the stock brand; otherwise `rsclaw/rsclaw-agent-v2` reads
/// 「智能体模型 v2」 / "Agent model v2", and any other RsClaw name gives way to
/// the brand's cloud name.
pub fn display_model(model: &str, zh: bool) -> Cow<'_, str> {
    display_model_with(model, zh, Names::current())
}

/// `text` with every RsClaw name taken out, for output a person reads that
/// may quote ids (tool cards, job failure reasons): unchanged in the stock
/// brand. Each word holding the name reads as [`display_model`] does; an API
/// key value (`sk-rsclaw-…`) and host names / paths (`hub.rsclaw.ai`,
/// `~/.rsclaw`) are left alone, since they must stay usable as written.
pub fn scrub(text: &str, zh: bool) -> Cow<'_, str> {
    scrub_with(text, zh, Names::current())
}

fn contains_name(s: &str) -> bool {
    s.as_bytes()
        .windows(NAME.len())
        .any(|w| w.eq_ignore_ascii_case(NAME.as_bytes()))
}

const NAME: &str = "rsclaw";

pub(crate) fn display_provider_with(name: &str, zh: bool, names: Names) -> Cow<'_, str> {
    if names.stock || !contains_name(name) {
        return Cow::Borrowed(name);
    }
    if name.trim().eq_ignore_ascii_case(NAME) {
        return Cow::Borrowed(names.cloud(zh));
    }
    Cow::Owned(strip_name(name, zh, names))
}

pub(crate) fn display_model_with(model: &str, zh: bool, names: Names) -> Cow<'_, str> {
    if names.stock || !contains_name(model) {
        return Cow::Borrowed(model);
    }
    let Some((provider, id)) = model.split_once('/') else {
        return Cow::Owned(model_id(model, zh, names));
    };
    let ours = provider.eq_ignore_ascii_case(NAME);
    Cow::Owned(match (ours, contains_name(id)) {
        (true, true) => model_id(id, zh, names),
        (true, false) => format!("{}/{id}", names.cloud(zh)),
        (false, true) => format!("{}/{}", display_provider_with(provider, zh, names), model_id(id, zh, names)),
        (false, false) => format!("{}/{id}", display_provider_with(provider, zh, names)),
    })
}

/// A bare model id holding the name: `rsclaw-<family>[-…]-v<N>` reads as the
/// family's neutral name plus the version; the service's own tags
/// (`rsclaw_native`, `rsclaw-legacy`) read as the cloud; anything else
/// loses the name.
fn model_id(id: &str, zh: bool, names: Names) -> String {
    let lower = id.trim().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix(NAME) else {
        return strip_name(id, zh, names);
    };
    let rest = rest.trim_start_matches(['-', '_']);
    if matches!(rest, "" | "native" | "legacy" | "image" | "gen") {
        return names.cloud(zh).to_owned();
    }
    let (family, version) = match rest.rsplit_once('-') {
        Some((family, v))
            if v.len() > 1
                && v.starts_with('v')
                && v[1..].bytes().all(|b| b.is_ascii_digit() || b == b'.') =>
        {
            (family, Some(v))
        }
        _ => (rest, None),
    };
    let label = match family.split('-').next().unwrap_or(family) {
        "agent" => ("智能体模型", "Agent model"),
        "flash" => ("快速模型", "Fast model"),
        "vision" => ("视觉模型", "Vision model"),
        "ocr" => ("文字识别模型", "Text recognition model"),
        "asr" => ("语音识别模型", "Speech recognition model"),
        "tts" | "audio" | "voice" => ("语音合成模型", "Speech model"),
        "embedding" => ("向量模型", "Embedding model"),
        "reranker" => ("重排序模型", "Reranking model"),
        "image" => ("图片模型", "Image model"),
        "video" => ("视频模型", "Video model"),
        "music" => ("音乐模型", "Music model"),
        "mv" => ("MV 模型", "MV model"),
        _ => return strip_name(id, zh, names),
    };
    let label = if zh { label.0 } else { label.1 };
    match version {
        Some(v) => format!("{label} {v}"),
        None => label.to_owned(),
    }
}

/// `text` without the name (any case); what is left of a word made only of
/// it reads as the cloud.
fn strip_name(text: &str, zh: bool, names: Names) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.to_ascii_lowercase().find(NAME) {
        out.push_str(&rest[..at]);
        rest = &rest[at + NAME.len()..];
    }
    out.push_str(rest);
    let out = out.trim_matches(|c: char| matches!(c, '-' | '_' | ':' | '/') || c.is_whitespace());
    if out.is_empty() {
        names.cloud(zh).to_owned()
    } else {
        out.to_owned()
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')
}

pub(crate) fn scrub_with(text: &str, zh: bool, names: Names) -> Cow<'_, str> {
    if names.stock || !contains_name(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    while !rest.is_empty() {
        let start = rest.find(is_word_char).unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
        let word = &rest[..end];
        rest = &rest[end..];
        // A sentence's full stop is not part of the word.
        let core = word.trim_end_matches('.');
        let tail = &word[core.len()..];
        let lower = core.to_ascii_lowercase();
        let keep = !contains_name(core)
            || lower.starts_with("sk-rsclaw")
            || lower.contains(".rsclaw")
            || lower.contains("rsclaw.");
        if keep {
            out.push_str(word);
        } else {
            out.push_str(&display_model_with(core, zh, names));
            out.push_str(tail);
            changed = true;
        }
    }
    if changed { Cow::Owned(out) } else { Cow::Borrowed(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOCK: Names = Names { stock: true, cloud_en: "Cloud", cloud_zh: "云端模型" };
    const QISHU: Names = Names { stock: false, cloud_en: "Qishu Cloud", cloud_zh: "栖枢云端模型" };
    const BARE: Names = Names { stock: false, cloud_en: "Cloud", cloud_zh: "云端模型" };

    #[test]
    fn stock_brand_keeps_every_id() {
        for s in ["rsclaw", "rsclaw/rsclaw-agent-v2", "rsclaw-video-v3", "rsclaw API 500: x"] {
            assert!(matches!(display_model_with(s, true, STOCK), Cow::Borrowed(b) if b == s));
            assert!(matches!(display_provider_with(s, false, STOCK), Cow::Borrowed(b) if b == s));
            assert!(matches!(scrub_with(s, true, STOCK), Cow::Borrowed(b) if b == s));
        }
        assert_eq!(STOCK.cloud(true), "rsclaw");
        assert_eq!(STOCK.cloud(false), "rsclaw");
    }

    #[test]
    fn model_refs_read_as_neutral_names() {
        for (model, zh, en) in [
            ("rsclaw/rsclaw-agent-v2", "智能体模型 v2", "Agent model v2"),
            ("rsclaw/rsclaw-flash-v2", "快速模型 v2", "Fast model v2"),
            ("RsClaw/RsClaw-Agent-V1", "智能体模型 v1", "Agent model v1"),
            ("rsclaw-vision-v1", "视觉模型 v1", "Vision model v1"),
            ("rsclaw-video-v3", "视频模型 v3", "Video model v3"),
            ("rsclaw-agent", "智能体模型", "Agent model"),
            ("rsclaw/deepseek-chat", "栖枢云端模型/deepseek-chat", "Qishu Cloud/deepseek-chat"),
            ("aiart/rsclaw-agent-v2", "aiart/智能体模型 v2", "aiart/Agent model v2"),
            ("rsclaw-mystery-v9", "mystery-v9", "mystery-v9"),
            ("rsclaw", "栖枢云端模型", "Qishu Cloud"),
            ("doubao/doubao-pro", "doubao/doubao-pro", "doubao/doubao-pro"),
        ] {
            assert_eq!(display_model_with(model, true, QISHU), zh, "{model}");
            assert_eq!(display_model_with(model, false, QISHU), en, "{model}");
        }
    }

    #[test]
    fn providers_read_as_the_cloud_name() {
        assert_eq!(display_provider_with("rsclaw", true, QISHU), "栖枢云端模型");
        assert_eq!(display_provider_with("rsclaw", false, QISHU), "Qishu Cloud");
        assert_eq!(display_provider_with("rsclaw", false, BARE), "Cloud");
        assert_eq!(display_provider_with("kimi", false, QISHU), "kimi");
        assert_eq!(display_provider_with("rsclaw_backup", false, QISHU), "backup");
    }

    #[test]
    fn scrub_removes_the_name_but_keeps_keys_and_hosts() {
        let s = "poll: rsclaw-native: status 500: queued on rsclaw/rsclaw-video-v3. See RSCLAW_API_KEY.";
        assert_eq!(
            scrub_with(s, false, QISHU),
            "poll: Qishu Cloud: status 500: queued on Video model v3. See API_KEY."
        );
        let keep = "key sk-rsclaw-abc123 at https://hub.rsclaw.ai/x and ~/.rsclaw/tools";
        assert!(matches!(scrub_with(keep, false, QISHU), Cow::Borrowed(_)));
        assert_eq!(scrub_with("rsclaw: no API key configured", true, QISHU), "栖枢云端模型: no API key configured");
        let none = "nothing to see";
        assert!(matches!(scrub_with(none, false, QISHU), Cow::Borrowed(_)));
        assert!(!scrub_with("RsClaw 测试群, rsclaw_legacy", true, QISHU).to_lowercase().contains("rsclaw"));
    }
}
