//! Facts that must survive compaction verbatim.
//!
//! A compaction summary is written by a model, and a model drops things: a
//! case number in one summary, a credit code in the next, and an
//! iterative summary never gets back what the previous one lost. So the
//! exact identifiers in the turns being compacted (and in the previous
//! summary) are pulled out by rule, and any the new summary does not carry
//! are appended to it verbatim. Which kinds count is set per scenario
//! (`agents.defaults.compaction.keyData.kinds`): a lawyer's agent keeps
//! parties, amounts and dates, a coding agent would only drown in them.

use std::sync::LazyLock;

use regex::Regex;

/// Kinds kept when the config names none: identifiers that are never
/// noise in any conversation.
pub const DEFAULT_KINDS: &[&str] = &["phone", "id_card", "email", "credit_code", "case_no"];

/// Cap on appended items, so a document-heavy session can't bloat the
/// summary.
pub const DEFAULT_MAX_ITEMS: usize = 60;

/// Heading of the block the missing items are appended under.
pub const KEY_DATA_HEADING: &str = "## Key Data (verbatim from the compacted turns)";

/// One extracted fact, with the words around it: a bare "赵磊" or
/// "027-87654321" doesn't say whose it is, and a model handed bare values
/// gives the defendant's phone to the plaintiff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyDatum {
    pub kind: &'static str,
    pub value: String,
    pub context: String,
}

/// Longest sentence kept whole as a fact's context.
const CONTEXT_MAX_CHARS: usize = 160;
/// For a longer sentence: its head (who it is about) and this much before
/// and after the fact.
const CONTEXT_HEAD_CHARS: usize = 40;
const CONTEXT_BEFORE_CHARS: usize = 40;
const CONTEXT_AFTER_CHARS: usize = 30;

/// The sentence holding `text[start..end]`, or for a long one its head
/// (who it is about) plus the stretch around the fact.
fn context_of(text: &str, start: usize, end: usize) -> String {
    let is_stop = |c: char| c == '\n' || c == '。';
    let s0 = text[..start]
        .char_indices()
        .rev()
        .find(|&(_, c)| is_stop(c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let e0 = text[end..].find(is_stop).map_or(text.len(), |i| end + i);
    let fact = &text[start..end];
    let lead: Vec<char> = text[s0..start].chars().collect();
    let tail: Vec<char> = text[end..e0].chars().collect();
    if lead.len() + fact.chars().count() + tail.len() <= CONTEXT_MAX_CHARS {
        return text[s0..e0].trim().to_owned();
    }
    let tail: String = tail.iter().take(CONTEXT_AFTER_CHARS).collect();
    if lead.len() <= CONTEXT_HEAD_CHARS + CONTEXT_BEFORE_CHARS {
        let lead: String = lead.iter().collect();
        return format!("{lead}{fact}{tail}").trim().to_owned();
    }
    let head: String = lead[..CONTEXT_HEAD_CHARS].iter().collect();
    let before: String = lead[lead.len() - CONTEXT_BEFORE_CHARS..].iter().collect();
    format!("{}…{before}{fact}{tail}", head.trim()).trim().to_owned()
}

struct Kind {
    name: &'static str,
    re: &'static LazyLock<Regex>,
    /// The match must not touch an ASCII letter or digit on either side.
    bounded: bool,
    /// Capture group holding the value (0 = whole match).
    group: usize,
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect("valid regex"));
    };
}

re!(PHONE, r"1[3-9]\d{9}");
re!(LANDLINE, r"0\d{2,3}-\d{7,8}");
re!(ID_CARD, r"\d{17}[\dXx]");
re!(EMAIL, r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}");
// 统一社会信用代码: 18 chars from the code's alphabet; at least one letter
// (an all-digit run is an ID number).
re!(CREDIT_CODE, r"[0-9A-HJ-NPQRTUWXY]{2}\d{6}[0-9A-HJ-NPQRTUWXY]{10}");
// （2024）鄂0116民初1234号
re!(CASE_NO, r"[（(]\d{4}[）)]\p{Han}{1,4}\d{0,6}\p{Han}{1,6}\d{1,7}号");
re!(AMOUNT, r"\d{1,3}(?:,\d{3})+(?:\.\d{1,2})?\s*(?:万元|元)|\d+(?:\.\d{1,2})?\s*(?:万元|元)");
re!(DATE, r"\d{4}年\d{1,2}月\d{1,2}日|\d{4}-\d{1,2}-\d{1,2}");
re!(
    ORG,
    r"[\p{Han}（）()]{2,40}?(?:集团股份有限公司|集团有限公司|股份有限公司|有限责任公司|有限公司|律师事务所|人民法院|人民检察院|仲裁委员会)"
);
re!(LEGAL_REP, r"(?:法定代表人|负责人)[：:\s]*(\p{Han}{2,4})");
re!(TIME_AFTER, r"^[\sT]\d{1,2}:\d{2}");

static KINDS: &[Kind] = &[
    Kind { name: "phone", re: &PHONE, bounded: true, group: 0 },
    Kind { name: "landline", re: &LANDLINE, bounded: true, group: 0 },
    Kind { name: "id_card", re: &ID_CARD, bounded: true, group: 0 },
    Kind { name: "email", re: &EMAIL, bounded: false, group: 0 },
    Kind { name: "credit_code", re: &CREDIT_CODE, bounded: true, group: 0 },
    Kind { name: "case_no", re: &CASE_NO, bounded: false, group: 0 },
    Kind { name: "amount", re: &AMOUNT, bounded: true, group: 0 },
    Kind { name: "date", re: &DATE, bounded: true, group: 0 },
    Kind { name: "org", re: &ORG, bounded: false, group: 0 },
    Kind { name: "legal_rep", re: &LEGAL_REP, bounded: false, group: 1 },
];

/// Words that open a party line and get swept into an organisation match
/// ("原告武汉…有限公司").
const ROLE_PREFIXES: &[&str] = &[
    "被答辩人", "答辩人", "被上诉人", "上诉人", "被申请人", "申请人", "第三人", "委托人",
    "原告", "被告一", "被告二", "被告三", "被告", "单位名称", "名称", "受理法院", "管辖法院",
    "代理律师", "致", "与", "和", "及",
];

/// An organisation "name" holding one of these is a generic reference
/// ("工程所在地人民法院"), not a named party or court.
const GENERIC_ORG_WORDS: &[&str] = &["所在地", "住所地", "有管辖权", "当地", "相关", "该"];

/// Whether `name` is a kind this module knows.
pub fn is_known_kind(name: &str) -> bool {
    KINDS.iter().any(|k| k.name == name)
}

/// Facts of the given kinds in `text`, deduplicated, kinds in the order
/// given and values in order of first appearance. Unknown kinds are
/// ignored.
pub fn extract(text: &str, kinds: &[String]) -> Vec<KeyDatum> {
    let mut out: Vec<KeyDatum> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for wanted in kinds {
        let Some(kind) = KINDS.iter().find(|k| k.name == wanted.as_str()) else {
            continue;
        };
        for caps in kind.re.captures_iter(text) {
            let Some(m) = caps.get(kind.group) else { continue };
            let whole = caps.get(0).expect("group 0");
            if kind.bounded && !ascii_bounded(text, whole.start(), whole.end()) {
                continue;
            }
            let value = clean(kind.name, m.as_str());
            if value.is_empty() || !plausible(kind.name, &value) {
                continue;
            }
            // "2026-10-17 13:25" is a message timestamp, not a date in the case.
            if kind.name == "date" && TIME_AFTER.is_match(&text[whole.end()..]) {
                continue;
            }
            if seen.insert((kind.name, normalize(&value))) {
                let context = context_of(text, whole.start(), whole.end());
                out.push(KeyDatum { kind: kind.name, value, context });
            }
        }
    }
    out
}

fn ascii_bounded(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    !before.is_some_and(|c| c.is_ascii_alphanumeric()) && !after.is_some_and(|c| c.is_ascii_alphanumeric())
}

fn clean(kind: &str, value: &str) -> String {
    let mut v = value.trim().to_owned();
    if kind == "org" {
        // Strip party-role words swept in from the left, repeatedly
        // ("被告一（华建公司）与原告" leaves several).
        loop {
            let before = v.clone();
            for p in ROLE_PREFIXES {
                if let Some(rest) = v.strip_prefix(p) {
                    v = rest.trim_start_matches(['：', ':', '（', '(', '）', ')']).to_owned();
                }
            }
            if v == before {
                break;
            }
        }
    }
    v
}

fn plausible(kind: &str, value: &str) -> bool {
    match kind {
        "credit_code" => value.chars().any(|c| c.is_ascii_uppercase()),
        // An org needs a name in front of its suffix.
        "org" => value.chars().count() >= 6 && !GENERIC_ORG_WORDS.iter().any(|w| value.contains(w)),
        _ => true,
    }
}

/// Form used to compare a fact with a summary: no whitespace or thousands
/// separators, half-width brackets.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != ',' && *c != '，')
        .map(|c| match c {
            '（' => '(',
            '）' => ')',
            '：' => ':',
            c => c,
        })
        .collect()
}

/// `data` the summary does not carry verbatim.
pub fn missing_from<'a>(summary: &str, data: &'a [KeyDatum]) -> Vec<&'a KeyDatum> {
    let haystack = normalize(summary);
    data.iter().filter(|d| !haystack.contains(&normalize(&d.value))).collect()
}

/// `summary` without a block an earlier `ensure_in_summary` appended (the
/// model may copy it, anywhere, from the previous summary). Only that
/// section goes; sections after it stay.
fn without_key_data_block(summary: &str) -> String {
    let Some(start) = summary.find(KEY_DATA_HEADING) else {
        return summary.trim_end().to_owned();
    };
    let after = start + KEY_DATA_HEADING.len();
    let end = summary[after..].find("\n## ").map_or(summary.len(), |i| after + i + 1);
    format!("{}\n\n{}", summary[..start].trim_end(), &summary[end..])
        .trim()
        .to_owned()
}

/// `summary` with whatever of `data` it dropped appended verbatim, at most
/// `max_items`. The appended block replaces one a previous compaction left,
/// so it never stacks up.
pub fn ensure_in_summary(summary: &str, data: &[KeyDatum], max_items: usize) -> String {
    let body = without_key_data_block(summary);
    let missing = missing_from(&body, data);
    if missing.is_empty() {
        return body;
    }
    let kept: Vec<KeyDatum> = missing.into_iter().take(max_items).cloned().collect();
    format!("{body}\n\n{KEY_DATA_HEADING}\n{}", render(&kept))
}

/// Store key of a session's ledger: the facts its turns' tool results
/// showed. A turn's tool results are dropped when it ends (only the reply is
/// kept), so without this a later turn can't see the case number an
/// earlier one read.
pub fn ledger_kv_key(session_key: &str) -> String {
    format!("key_data:{session_key}")
}

/// `ledger` with `new` facts added at the end; past `max_items` the oldest
/// go.
pub fn merge(mut ledger: Vec<KeyDatum>, new: Vec<KeyDatum>, max_items: usize) -> Vec<KeyDatum> {
    for d in new {
        let n = normalize(&d.value);
        if !ledger.iter().any(|l| l.kind == d.kind && normalize(&l.value) == n) {
            ledger.push(d);
        }
    }
    let excess = ledger.len().saturating_sub(max_items);
    ledger.drain(..excess);
    ledger
}

/// The ledger as stored: `[[kind, value, context], …]`.
pub fn to_json(ledger: &[KeyDatum]) -> String {
    let rows: Vec<[&str; 3]> =
        ledger.iter().map(|d| [d.kind, d.value.as_str(), d.context.as_str()]).collect();
    serde_json::to_string(&rows).unwrap_or_default()
}

/// A stored ledger; kinds this build doesn't know are dropped.
pub fn from_json(raw: &str) -> Vec<KeyDatum> {
    let rows: Vec<Vec<String>> = serde_json::from_str(raw).unwrap_or_default();
    rows.into_iter()
        .filter_map(|row| {
            let mut row = row.into_iter();
            let kind = row.next()?;
            let kind = KINDS.iter().find(|k| k.name == kind)?.name;
            let value = row.next()?;
            let context = row.next().unwrap_or_else(|| value.clone());
            Some(KeyDatum { kind, value, context })
        })
        .collect()
}

/// A safe overestimate of `text`'s token count: a token per non-ASCII
/// character, one per two ASCII characters.
pub fn token_upper_bound(text: &str) -> usize {
    let (wide, ascii) = text.chars().fold((0usize, 0usize), |(w, a), c| {
        if c.is_ascii() { (w, a + 1) } else { (w + 1, a) }
    });
    wide + ascii.div_ceil(2)
}

/// `render`, keeping only the newest lines that fit in `budget` tokens
/// (by [`token_upper_bound`]), in their original order. `None` when not
/// even one fits.
pub fn render_within(ledger: &[KeyDatum], budget: usize) -> Option<String> {
    let rendered = render(ledger);
    let lines: Vec<&str> = rendered.lines().collect();
    let mut used = 0usize;
    let mut keep = 0usize;
    for line in lines.iter().rev() {
        let cost = token_upper_bound(line) + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        keep += 1;
    }
    (keep > 0).then(|| lines[lines.len() - keep..].join("\n"))
}

/// The facts for the model: each one's context once, as `- ` lines (one
/// line usually carries several facts of the same party).
pub fn render(ledger: &[KeyDatum]) -> String {
    let mut seen = std::collections::HashSet::new();
    ledger
        .iter()
        .filter(|d| seen.insert(d.context.as_str()))
        .map(|d| format!("- {}", d.context))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| (*s).to_owned()).collect()
    }

    const PETITION: &str = "原告：武汉恒远建筑装饰工程有限公司，统一社会信用代码：91420112MA4L12345X，\
        法定代表人：李明远。13800138000。被告一：华建工程集团有限公司，法定代表人：王立新，\
        联系方式：023-61234567。\n被告二：武汉云栖北岸置业有限公司，住所地：黄陂区滠水湖经济开发区长丰路以东、\
        云阳路以北、和平路以西科创中心（一期）企业总部A座/栋/单元3层8室，统一社会信用代码：\
        91420116MA4K7RTB09，法定代表人：赵磊，联系方式：027-87654321。\n\
        一、被告一、被告二向原告支付工程款123,456.78元；自2023年1月18日起算。\
        案号（2024）鄂0116民初1234号，武汉市黄陂区人民法院。身份证 420106198503150418。";

    const ALL: &[&str] = &[
        "credit_code", "id_card", "phone", "landline", "case_no", "amount", "date", "org", "legal_rep",
    ];

    #[test]
    fn key_data_pulls_legal_facts() {
        let got: Vec<(String, String)> = extract(PETITION, &kinds(ALL))
            .into_iter()
            .map(|d| (d.kind.to_owned(), d.value))
            .collect();
        let has = |k: &str, v: &str| got.iter().any(|(gk, gv)| gk == k && gv == v);
        assert!(has("credit_code", "91420112MA4L12345X"), "{got:?}");
        assert!(has("credit_code", "91420116MA4K7RTB09"), "{got:?}");
        assert!(has("id_card", "420106198503150418"), "{got:?}");
        assert!(has("phone", "13800138000"), "{got:?}");
        assert!(has("landline", "023-61234567"), "{got:?}");
        assert!(has("case_no", "（2024）鄂0116民初1234号"), "{got:?}");
        assert!(has("amount", "123,456.78元"), "{got:?}");
        assert!(has("date", "2023年1月18日"), "{got:?}");
        assert!(has("org", "武汉恒远建筑装饰工程有限公司"), "{got:?}");
        assert!(has("org", "华建工程集团有限公司"), "{got:?}");
        assert!(has("org", "武汉市黄陂区人民法院"), "{got:?}");
        assert!(has("legal_rep", "李明远") && has("legal_rep", "赵磊"), "{got:?}");
        // The ID number is not also a credit code; no phone inside it.
        assert!(!got.iter().any(|(k, v)| k == "credit_code" && v.starts_with("4201231978")), "{got:?}");
        assert!(!got.iter().any(|(k, v)| k == "phone" && PETITION.contains(&format!("4{v}"))), "{got:?}");
    }

    #[test]
    fn key_data_keeps_whose_fact_it_is() {
        let facts = extract(PETITION, &kinds(ALL));
        let ctx = |v: &str| facts.iter().find(|d| d.value == v).map(|d| d.context.clone()).unwrap();
        // A long party sentence keeps its head, so the far-off legal
        // representative and phone still say which party they belong to.
        for v in ["赵磊", "027-87654321", "91420116MA4K7RTB09"] {
            let c = ctx(v);
            assert!(c.starts_with("被告二：武汉云栖北岸置业有限公司") && c.contains(v), "{v}: {c}");
            assert!(c.chars().count() <= CONTEXT_HEAD_CHARS + CONTEXT_BEFORE_CHARS + 20 + CONTEXT_AFTER_CHARS);
        }
        assert!(ctx("李明远").starts_with("原告："), "{}", ctx("李明远"));
        // One line per sentence, however many facts it holds.
        let rendered = render(&facts);
        assert_eq!(rendered.matches("- 原告：武汉恒远").count(), 1, "{rendered}");
    }

    #[test]
    fn key_data_skips_labels_generic_courts_and_timestamps() {
        let text = "记一下：受理法院武汉市黄陂区人民法院。一般由工程所在地人民法院管辖。\
            [Now: 2026-10-17 13:25 Sat +08:00] 起诉日 2024-03-05。";
        let got: Vec<String> =
            extract(text, &kinds(&["org", "date"])).into_iter().map(|d| d.value).collect();
        assert_eq!(got, vec!["武汉市黄陂区人民法院", "2024-03-05"]);
    }

    #[test]
    fn key_data_render_within_keeps_the_newest_that_fit() {
        let d = |v: &str| KeyDatum { kind: "case_no", value: v.into(), context: format!("案号{v}") };
        let ledger = vec![d("一"), d("二"), d("三")];
        assert_eq!(render_within(&ledger, 100).as_deref(), Some("- 案号一\n- 案号二\n- 案号三"));
        // Each line costs 5 here ("- " is 1, three CJK chars, newline 1).
        assert_eq!(render_within(&ledger, 10).as_deref(), Some("- 案号二\n- 案号三"));
        assert_eq!(render_within(&ledger, 3), None);
        assert_eq!(token_upper_bound("ab案"), 2);
    }

    #[test]
    fn key_data_default_kinds_skip_scenario_noise() {
        let got = extract(PETITION, &kinds(DEFAULT_KINDS));
        assert!(got.iter().all(|d| DEFAULT_KINDS.contains(&d.kind)));
        assert!(got.iter().any(|d| d.kind == "case_no"));
    }

    #[test]
    fn key_data_appends_only_what_the_summary_dropped() {
        let data = extract(PETITION, &kinds(&["credit_code", "amount", "case_no"]));
        let summary = "## Goal\n起诉华建。代码 91420112MA4L12345X、91420116MA4K7RTB09，诉请 123456.78 元。";
        let missing: Vec<&str> = missing_from(summary, &data).iter().map(|d| d.kind).collect();
        assert_eq!(missing, vec!["case_no"], "commas and spaces don't count");
        let out = ensure_in_summary(summary, &data, 60);
        assert!(out.starts_with(summary) && out.contains(KEY_DATA_HEADING));
        assert!(out.contains("（2024）鄂0116民初1234号"));
        // A second compaction replaces the block instead of stacking it.
        let again = ensure_in_summary(&out, &data, 60);
        assert_eq!(again.matches(KEY_DATA_HEADING).count(), 1);
        // A copied block mid-summary goes; the sections after it stay.
        let copied = format!("## Goal\nx\n\n{KEY_DATA_HEADING}\n- y\n\n## Pending\nz");
        let cleaned = ensure_in_summary(&copied, &[], 60);
        assert!(cleaned.contains("## Pending\nz") && !cleaned.contains("- y"), "{cleaned}");
        // Nothing missing: summary unchanged.
        let full = format!("{summary} 案号（2024）鄂0116民初1234号");
        assert_eq!(ensure_in_summary(&full, &data, 60), full);
    }

    #[test]
    fn key_data_ledger_keeps_the_newest_and_round_trips() {
        let d = |k: &'static str, v: &str| KeyDatum { kind: k, value: v.to_owned(), context: format!("c {v}") };
        let ledger = merge(vec![d("phone", "13800138000")], vec![d("phone", "138 0013 8000"), d("case_no", "a")], 2);
        assert_eq!(ledger, vec![d("phone", "13800138000"), d("case_no", "a")]);
        let ledger = merge(ledger, vec![d("amount", "5元")], 2);
        assert_eq!(ledger, vec![d("case_no", "a"), d("amount", "5元")]);
        assert_eq!(from_json(&to_json(&ledger)), ledger);
        let old = from_json(r#"[["nope","x"],["amount","5元"]]"#);
        assert_eq!(old, vec![KeyDatum { kind: "amount", value: "5元".into(), context: "5元".into() }]);
        assert_eq!(render(&ledger), "- c a\n- c 5元");
    }
}
