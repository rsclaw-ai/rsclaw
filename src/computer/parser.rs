//! Format-tolerant VLM response parser.
//!
//! Accepts the four common coordinate formats emitted by GUI VLMs:
//!
//!   1. `<|box_start|>(x, y)<|box_end|>`  (UI-TARS native, single point)
//!   2. `<point>x y</point>`              (Doubao, space-separated point)
//!   3. `(x, y)`                          (plain parenthesised tuple)
//!   4. `[x1, y1, x2, y2]` — UI-TARS-desktop bbox; the parser collapses to
//!      the centre point so downstream actions stay scalar.
//!
//! And the action types: click / left_double / right_single / drag /
//! hotkey / type / scroll / wait / finished / call_user / ... .
//!
//! In `Auto` mode the parser tries the formats in this priority order:
//! `BoxQuad` → `UiTarsBoxPair` → `DoubaoPoint` → `PlainTuple`. `BoxQuad`
//! goes first because it is the default emitted by UI-TARS-desktop and
//! the most distinctive (square brackets + 4 numbers).
//!
//! Returns a `Vec<ParsedAction>` — drivers handle coordinate scaling via
//! `ExecCtx.factors`. The parser keeps the model-emitted numbers
//! verbatim; nothing here is screen-aware.

use super::action::ParsedAction;
use std::collections::BTreeMap;

/// Coordinate format hint. `Auto` tries all four; specific values are
/// faster and avoid ambiguity when the upstream model is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordFormat {
    Auto,
    UiTarsBoxPair,
    DoubaoPoint,
    PlainTuple,
    BoxQuad,
}

/// Parse the raw `Thought: ...\nAction: ...` text emitted by a vision
/// model into structured actions. The result preserves model-space
/// coordinates; scaling to screen happens later in the driver.
///
/// Supports multiple Thought/Action pairs in one response (separated by
/// blank lines or successive `Thought:` markers). Stray text before or
/// after the first marker is skipped.
pub fn parse_vlm_response(text: &str, hint: CoordFormat) -> Vec<ParsedAction> {
    let blocks = split_blocks(text);
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        if let Some(action) = parse_block(&block, hint) {
            out.push(action);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Block splitting
// ---------------------------------------------------------------------------

/// One Thought/Action chunk. Either field may be empty (some models skip
/// the Thought for terminal actions like `finished` or `call_user`).
struct Block {
    thought: String,
    action: String,
}

/// Split the raw response into Thought/Action blocks. We scan
/// line-by-line, anchoring on `Thought:` and `Action:` markers; any
/// other lines while we are inside a section are appended to the
/// current field (so multi-line Thoughts are preserved).
fn split_blocks(text: &str) -> Vec<Block> {
    enum Section {
        None,
        Thought,
        Action,
    }

    let mut blocks: Vec<Block> = Vec::new();
    let mut cur_thought = String::new();
    let mut cur_action = String::new();
    let mut section = Section::None;
    let mut have_any = false;

    let flush = |blocks: &mut Vec<Block>, t: &mut String, a: &mut String, have: &mut bool| {
        if *have {
            blocks.push(Block {
                thought: t.trim().to_owned(),
                action: a.trim().to_owned(),
            });
        }
        t.clear();
        a.clear();
        *have = false;
    };

    for raw_line in text.lines() {
        let line = raw_line;
        let trimmed = line.trim_start();

        if let Some(rest) = strip_prefix_ci(trimmed, "Thought:") {
            // New Thought starts a new block — flush whatever we had.
            flush(&mut blocks, &mut cur_thought, &mut cur_action, &mut have_any);
            cur_thought.push_str(rest.trim_start());
            section = Section::Thought;
            have_any = true;
        } else if let Some(rest) = strip_prefix_ci(trimmed, "Action:") {
            // Action begins. If we already saw an Action without a fresh
            // Thought, flush it (two Actions back-to-back implies two
            // separate blocks even with no Thought between them).
            if matches!(section, Section::Action) {
                flush(&mut blocks, &mut cur_thought, &mut cur_action, &mut have_any);
            }
            cur_action.push_str(rest.trim_start());
            section = Section::Action;
            have_any = true;
        } else {
            // Continuation line. Skip until we are inside a section.
            match section {
                Section::None => {}
                Section::Thought => {
                    if !cur_thought.is_empty() {
                        cur_thought.push('\n');
                    }
                    cur_thought.push_str(line);
                }
                Section::Action => {
                    if !cur_action.is_empty() {
                        cur_action.push('\n');
                    }
                    cur_action.push_str(line);
                }
            }
        }
    }

    flush(&mut blocks, &mut cur_thought, &mut cur_action, &mut have_any);
    blocks
}

/// Case-insensitive `str::strip_prefix`. We are lenient about
/// `THOUGHT:` / `thought:` since the marker is a model output.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() < prefix.len() {
        return None;
    }
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Per-block parsing
// ---------------------------------------------------------------------------

/// Parse a single Thought/Action block. Returns `None` if the action
/// line is unparseable (no function call shape).
fn parse_block(block: &Block, hint: CoordFormat) -> Option<ParsedAction> {
    let action_line = block.action.trim();
    if action_line.is_empty() {
        return None;
    }

    let (fn_name, args_str) = split_call(action_line)?;
    let action_type = fn_name.to_ascii_lowercase();

    // Walk top-level args, splitting on `=` and `,` while respecting
    // quotes and brackets.
    let pairs = split_args(args_str);

    let mut raw_args: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in pairs {
        let key = canonical_key(&k);
        // Last write wins — this matches the TS impl behaviour where
        // duplicate keys overwrite.
        raw_args.insert(key, v);
    }

    // Extract coordinates from start_box / end_box if present.
    let start = raw_args.get("start_box").and_then(|v| parse_coord(v, hint));
    let end = raw_args.get("end_box").and_then(|v| parse_coord(v, hint));

    Some(ParsedAction {
        thought: block.thought.clone(),
        action_type,
        raw_args,
        start,
        end,
    })
}

/// Split `name(args)` into `(name, args)`. The args section is
/// everything between the first `(` and the LAST `)` so nested parens
/// inside string literals survive. Returns `None` if the string is not
/// shaped like a function call.
fn split_call(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let lparen = s.find('(')?;
    let rparen = s.rfind(')')?;
    if rparen <= lparen {
        return None;
    }
    let name = s[..lparen].trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let args = &s[lparen + 1..rparen];
    Some((name, args))
}

/// Map alias keys to their canonical name. Older outputs use `point` /
/// `start_point` / `end_point` instead of `start_box` / `end_box`; we
/// normalise so downstream code only has to look at the box keys.
fn canonical_key(k: &str) -> String {
    match k {
        "point" | "start_point" => "start_box".to_owned(),
        "end_point" => "end_box".to_owned(),
        other => other.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Argument splitting
// ---------------------------------------------------------------------------

/// Split a raw arg string like `start_box='[1,2,3,4]', direction='down'`
/// into key/value pairs. Quotes (`'` or `"`) and brackets (`(`, `[`,
/// `{`, `<`) are tracked so nested commas survive.
///
/// We scan once, locating `=` at depth 0 outside quotes, then the next
/// top-level `,` (or end-of-string) gives the value. Escaped chars
/// inside quotes (`\\'`, `\\"`, `\\n`, etc.) are preserved verbatim
/// — we do NOT decode escapes, because the operator decides what to do
/// with the literal `\n` the model emitted.
fn split_args(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let n = bytes.len();

    while i < n {
        // Skip whitespace and stray separators.
        while i < n && (bytes[i].is_whitespace() || bytes[i] == ',') {
            i += 1;
        }
        if i >= n {
            break;
        }

        // Read key up to '='.
        let key_start = i;
        while i < n && bytes[i] != '=' && bytes[i] != ',' {
            i += 1;
        }
        if i >= n {
            // Trailing key with no '=' → ignore (not a kwarg).
            break;
        }
        if bytes[i] == ',' {
            // Bare positional value — skip; UI-TARS always uses kwargs.
            i += 1;
            continue;
        }
        let key: String = bytes[key_start..i].iter().collect::<String>().trim().to_owned();
        i += 1; // consume '='

        // Skip whitespace before value.
        while i < n && bytes[i].is_whitespace() {
            i += 1;
        }

        // Read value, respecting quotes / brackets.
        let val_start = i;
        let mut in_quote: Option<char> = None;
        let mut depth: i32 = 0;
        while i < n {
            let c = bytes[i];

            // Backslash-escape inside a quoted value: consume next char
            // verbatim. The literal sequence (e.g. \\n, \\') stays in
            // the value — caller's job to interpret.
            if in_quote.is_some() && c == '\\' && i + 1 < n {
                i += 2;
                continue;
            }

            if let Some(q) = in_quote {
                if c == q {
                    in_quote = None;
                }
                i += 1;
                continue;
            }

            match c {
                '\'' | '"' => {
                    in_quote = Some(c);
                    i += 1;
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    i += 1;
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    i += 1;
                }
                ',' if depth == 0 => break,
                _ => i += 1,
            }
        }

        let raw_value: String = bytes[val_start..i].iter().collect::<String>();
        let value = strip_outer_quotes(raw_value.trim());
        if !key.is_empty() {
            out.push((key, value));
        }

        // Consume the separating comma if any.
        if i < n && bytes[i] == ',' {
            i += 1;
        }
    }

    out
}

/// Strip a single layer of matching `'...'` or `"..."` quotes. Leaves
/// inner content untouched (escape sequences preserved).
fn strip_outer_quotes(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == '\'' || first == '"') && first == last {
            return bytes[1..bytes.len() - 1].iter().collect();
        }
    }
    s.to_owned()
}

// ---------------------------------------------------------------------------
// Coordinate parsing
// ---------------------------------------------------------------------------

/// Parse a box/point value into a model-space `(x, y)` pair.
///
/// In `Auto` mode the formats are tried in priority order:
///   1. `BoxQuad`       — `[x1, y1, x2, y2]` (collapsed to centre)
///   2. `UiTarsBoxPair` — `<|box_start|>(x, y)<|box_end|>`
///   3. `DoubaoPoint`   — `<point>x y</point>`
///   4. `PlainTuple`    — `(x, y)`
///
/// Returns `None` if no format matches.
fn parse_coord(value: &str, hint: CoordFormat) -> Option<(f32, f32)> {
    let v = value.trim();
    match hint {
        CoordFormat::BoxQuad => parse_box_quad(v),
        CoordFormat::UiTarsBoxPair => parse_ui_tars_box_pair(v),
        CoordFormat::DoubaoPoint => parse_doubao_point(v),
        CoordFormat::PlainTuple => parse_plain_tuple(v),
        CoordFormat::Auto => parse_box_quad(v)
            .or_else(|| parse_ui_tars_box_pair(v))
            .or_else(|| parse_doubao_point(v))
            .or_else(|| parse_plain_tuple(v)),
    }
}

/// `[x1, y1, x2, y2]` → centre `((x1+x2)/2, (y1+y2)/2)`. Also accepts
/// the 2-tuple `[x, y]` (which returns the point as-is) since some
/// models emit it.
fn parse_box_quad(v: &str) -> Option<(f32, f32)> {
    let inner = v.strip_prefix('[')?.strip_suffix(']')?;
    parse_numeric_tuple(inner, &[',']).and_then(|nums| match nums.len() {
        2 => Some((nums[0], nums[1])),
        4 => Some(((nums[0] + nums[2]) / 2.0, (nums[1] + nums[3]) / 2.0)),
        _ => None,
    })
}

/// `<|box_start|>(x, y)<|box_end|>` → `(x, y)`. Tolerates extra
/// whitespace and an optional leading `(`/trailing `)`.
fn parse_ui_tars_box_pair(v: &str) -> Option<(f32, f32)> {
    let start = v.find("<|box_start|>")?;
    let end = v.find("<|box_end|>")?;
    if end <= start {
        return None;
    }
    let inner = v[start + "<|box_start|>".len()..end].trim();
    let inner = inner
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(inner);
    let nums = parse_numeric_tuple(inner, &[','])?;
    match nums.len() {
        2 => Some((nums[0], nums[1])),
        4 => Some(((nums[0] + nums[2]) / 2.0, (nums[1] + nums[3]) / 2.0)),
        _ => None,
    }
}

/// `<point>x y</point>` → `(x, y)`. Numbers are space-separated.
fn parse_doubao_point(v: &str) -> Option<(f32, f32)> {
    let start = v.find("<point>")?;
    let end = v.find("</point>")?;
    if end <= start {
        return None;
    }
    let inner = v[start + "<point>".len()..end].trim();
    // Doubao uses spaces; some emitters use commas — accept both.
    let nums = parse_numeric_tuple(inner, &[' ', ','])?;
    match nums.len() {
        2 => Some((nums[0], nums[1])),
        _ => None,
    }
}

/// `(x, y)` (or `(x1, y1, x2, y2)` collapsed to its centre).
fn parse_plain_tuple(v: &str) -> Option<(f32, f32)> {
    let inner = v.strip_prefix('(').and_then(|s| s.strip_suffix(')'))?;
    let nums = parse_numeric_tuple(inner, &[','])?;
    match nums.len() {
        2 => Some((nums[0], nums[1])),
        4 => Some(((nums[0] + nums[2]) / 2.0, (nums[1] + nums[3]) / 2.0)),
        _ => None,
    }
}

/// Parse `s` as numbers separated by any of `seps`. Whitespace is
/// always allowed in addition to the explicit separators. Returns
/// `None` if any token fails to parse — we want all-or-nothing.
fn parse_numeric_tuple(s: &str, seps: &[char]) -> Option<Vec<f32>> {
    let mut out = Vec::new();
    let split_iter = s.split(|c: char| c.is_whitespace() || seps.contains(&c));
    for tok in split_iter {
        if tok.is_empty() {
            continue;
        }
        out.push(tok.parse::<f32>().ok()?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> ParsedAction {
        let mut v = parse_vlm_response(text, CoordFormat::Auto);
        assert_eq!(v.len(), 1, "expected one action, got {}: {:?}", v.len(), v);
        v.remove(0)
    }

    #[test]
    fn ui_tars_15_box_pair() {
        let text = "Thought: click button\nAction: click(start_box='<|box_start|>(133,487)<|box_end|>')";
        let a = one(text);
        assert_eq!(a.action_type, "click");
        assert_eq!(a.thought, "click button");
        assert_eq!(a.start, Some((133.0, 487.0)));
        assert_eq!(a.end, None);
    }

    #[test]
    fn doubao_point_format() {
        let text = "Thought: click logo\nAction: click(point='<point>510 150</point>')";
        let a = one(text);
        assert_eq!(a.action_type, "click");
        assert_eq!(a.start, Some((510.0, 150.0)));
        // Alias normalisation: `point` should become `start_box` in raw_args.
        assert!(a.raw_args.contains_key("start_box"));
        assert!(!a.raw_args.contains_key("point"));
    }

    #[test]
    fn box_quad_centre() {
        let text = "Thought: click\nAction: click(start_box='[133,487,156,510]')";
        let a = one(text);
        assert_eq!(a.action_type, "click");
        // Centre = ((133+156)/2, (487+510)/2) = (144.5, 498.5).
        assert_eq!(a.start, Some((144.5, 498.5)));
    }

    #[test]
    fn plain_tuple() {
        let text = "Thought: click\nAction: click(start_box='(133, 487)')";
        let a = one(text);
        assert_eq!(a.start, Some((133.0, 487.0)));
    }

    #[test]
    fn drag_with_two_boxes() {
        let text = "Thought: drag\nAction: drag(start_box='[10,20,30,40]', end_box='[50,60,70,80]')";
        let a = one(text);
        assert_eq!(a.action_type, "drag");
        // Centres: (20, 30) and (60, 70).
        assert_eq!(a.start, Some((20.0, 30.0)));
        assert_eq!(a.end, Some((60.0, 70.0)));
    }

    #[test]
    fn type_with_embedded_quotes() {
        let text = r#"Thought: type
Action: type(content='hello \"world\"')"#;
        let a = one(text);
        assert_eq!(a.action_type, "type");
        // Escapes are preserved verbatim — the operator decodes.
        assert_eq!(a.raw_args.get("content"), Some(&"hello \\\"world\\\"".to_owned()));
        assert_eq!(a.start, None);
    }

    #[test]
    fn type_with_newline_submission() {
        let text = r"Thought: submit
Action: type(content='hello\n')";
        let a = one(text);
        assert_eq!(a.action_type, "type");
        // Literal backslash-n preserved (not collapsed to '\n').
        assert_eq!(a.raw_args.get("content"), Some(&"hello\\n".to_owned()));
    }

    #[test]
    fn hotkey_no_coords() {
        let text = "Thought: copy\nAction: hotkey(key='ctrl c')";
        let a = one(text);
        assert_eq!(a.action_type, "hotkey");
        assert_eq!(a.raw_args.get("key"), Some(&"ctrl c".to_owned()));
        assert_eq!(a.start, None);
        assert_eq!(a.end, None);
    }

    #[test]
    fn wait_no_args() {
        let text = "Thought: pause\nAction: wait()";
        let a = one(text);
        assert_eq!(a.action_type, "wait");
        assert!(a.raw_args.is_empty());
        assert_eq!(a.start, None);
    }

    #[test]
    fn finished_with_content() {
        let text = "Thought: \nAction: finished(content='Task done')";
        let a = one(text);
        assert_eq!(a.action_type, "finished");
        assert_eq!(a.raw_args.get("content"), Some(&"Task done".to_owned()));
        assert_eq!(a.thought, "");
    }

    #[test]
    fn call_user_no_args() {
        let text = "Action: call_user()";
        let a = one(text);
        assert_eq!(a.action_type, "call_user");
        // Empty Thought is allowed.
        assert_eq!(a.thought, "");
    }

    #[test]
    fn multi_action_pair() {
        let text = "Thought: first\nAction: click(start_box='(10,20)')\n\nThought: second\nAction: type(content='hi')";
        let actions = parse_vlm_response(text, CoordFormat::Auto);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].action_type, "click");
        assert_eq!(actions[0].start, Some((10.0, 20.0)));
        assert_eq!(actions[1].action_type, "type");
        assert_eq!(actions[1].raw_args.get("content"), Some(&"hi".to_owned()));
    }

    #[test]
    fn unparseable_line_skipped() {
        // Two blocks, but the first action is malformed — it should be
        // dropped without panicking.
        let text = "Thought: bad\nAction: not_a_function_call\n\nThought: good\nAction: click(start_box='(1,2)')";
        let actions = parse_vlm_response(text, CoordFormat::Auto);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, "click");
    }

    #[test]
    fn auto_mode_matches_each_explicit_format() {
        // Each text uses one format; Auto and the matching explicit hint
        // should produce identical (start, action_type).
        let cases: &[(&str, CoordFormat, (f32, f32))] = &[
            (
                "Thought: x\nAction: click(start_box='[10,20,30,40]')",
                CoordFormat::BoxQuad,
                (20.0, 30.0),
            ),
            (
                "Thought: x\nAction: click(start_box='<|box_start|>(50,60)<|box_end|>')",
                CoordFormat::UiTarsBoxPair,
                (50.0, 60.0),
            ),
            (
                "Thought: x\nAction: click(point='<point>70 80</point>')",
                CoordFormat::DoubaoPoint,
                (70.0, 80.0),
            ),
            (
                "Thought: x\nAction: click(start_box='(90, 100)')",
                CoordFormat::PlainTuple,
                (90.0, 100.0),
            ),
        ];
        for (text, hint, expected) in cases {
            let auto = parse_vlm_response(text, CoordFormat::Auto);
            let explicit = parse_vlm_response(text, *hint);
            assert_eq!(auto.len(), 1, "auto failed: {text}");
            assert_eq!(explicit.len(), 1, "explicit failed: {text}");
            assert_eq!(auto[0].start, Some(*expected), "auto wrong: {text}");
            assert_eq!(explicit[0].start, Some(*expected), "explicit wrong: {text}");
            assert_eq!(auto[0].action_type, explicit[0].action_type);
        }
    }

    #[test]
    fn stray_text_before_thought() {
        // Some models prepend chatter; we should skip until Thought:.
        let text = "Some random preamble.\nMore filler.\nThought: real\nAction: click(start_box='(5,5)')";
        let a = one(text);
        assert_eq!(a.action_type, "click");
        assert_eq!(a.thought, "real");
        assert_eq!(a.start, Some((5.0, 5.0)));
    }
}
