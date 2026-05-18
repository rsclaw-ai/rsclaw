//! jaq (jq-compatible) interpreter wrapper for `--jq <expr>` and the
//! built-in templates. Compiles once at watch start, runs many times.
//!
//! The compiled `Filter` holds references into the `Arena` it was loaded
//! from. To avoid carrying a self-referential struct around (which Rust
//! doesn't allow natively) we leak the arena. One arena per `/watch`
//! session with `--jq` — bounded by the number of active watches, freed
//! at process exit. Each arena is small (a typed_arena over `String`s
//! holding the parsed jq source).

use anyhow::{anyhow, Result};
use jaq_core::{
    data::{self, JustLut},
    load::{Arena, File, Loader},
    unwrap_valr, Compiler, Ctx, Vars,
};
use jaq_json::Val;

/// A compiled jq filter ready to run against a `serde_json::Value`.
pub struct CompiledJq {
    filter: jaq_core::Filter<JustLut<Val>>,
}

impl CompiledJq {
    /// Parse + compile a jq expression. Returns a descriptive error on
    /// syntax / type errors so the caller can surface it back to the
    /// chat client.
    ///
    /// The arena that holds the parsed source strings is intentionally
    /// leaked: jaq's loaded modules borrow `&'s str` slices from it,
    /// and the compiled `Filter` indirectly references those strings.
    /// Storing the arena handle in `CompiledJq` would force `!Send`
    /// (typed_arena uses interior mutability) which collides with the
    /// tokio task that runs the watch processor. Leak gives us
    /// `&'static str` everywhere — the freed memory is bounded by the
    /// number of unique /watch sessions started, released at process
    /// exit.
    pub fn compile(expr: &str) -> Result<Self> {
        let arena: &'static Arena = Box::leak(Box::default());
        let defs = jaq_core::defs()
            .chain(jaq_std::defs())
            .chain(jaq_json::defs());
        let funs = jaq_core::funs::<JustLut<Val>>()
            .chain(jaq_std::funs())
            .chain(jaq_json::funs());

        let loader = Loader::new(defs);
        let file = File {
            code: expr,
            path: (),
        };
        let modules = loader
            .load(arena, file)
            .map_err(|errs| anyhow!("jq parse failed: {errs:?}"))?;
        let filter = Compiler::default()
            .with_funs(funs)
            .compile(modules)
            .map_err(|errs| anyhow!("jq compile failed: {errs:?}"))?;
        Ok(Self { filter })
    }

    /// Run the filter on one input value, return every produced output
    /// as a string. Filter errors (e.g. accessing a field on a number)
    /// are silently dropped so a malformed event in a long stream
    /// doesn't kill the whole watch.
    pub fn run(&self, input: &serde_json::Value) -> Vec<String> {
        let Some(val) = json_to_val(input) else {
            return Vec::new();
        };
        let ctx = Ctx::<JustLut<Val>>::new(&self.filter.lut, Vars::new([]));
        self.filter
            .id
            .run((ctx, val))
            .filter_map(|res| unwrap_valr(res).ok())
            .map(|v| format_jq_output(&v))
            .collect()
    }
}

/// Convert a `serde_json::Value` to a jaq `Val`. jaq has its own
/// `read::parse_single` path but it works from bytes; we already have a
/// parsed `serde_json::Value` (the SSE wire parser produced one) so
/// stringify-and-reparse here is the simplest bridge.
fn json_to_val(v: &serde_json::Value) -> Option<Val> {
    // serde_json::Value → bytes → jaq Val. The serialize-then-parse hop
    // is cheap relative to running the jq program on the value, and it
    // sidesteps an O(N) match over every JSON node type.
    let bytes = serde_json::to_vec(v).ok()?;
    jaq_json::read::parse_single(&bytes).ok()
}

/// Format a jq output value for display.
///
/// - String values (both UTF-8 and raw byte strings in jaq-json's model)
///   unquote — so `--jq '"hello"'` shows `hello`, not `"hello"`. This is
///   what users expect when they format with `"\(.code) \(.name)"`.
/// - Everything else uses jaq's `Display` impl, which produces compact JSON.
fn format_jq_output(v: &Val) -> String {
    match v {
        Val::TStr(b) | Val::BStr(b) => String::from_utf8_lossy(b.as_ref()).into_owned(),
        _ => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compiles_identity_filter() {
        let f = CompiledJq::compile(".").unwrap();
        let out = f.run(&json!({"a": 1}));
        assert_eq!(out, vec!["{\"a\":1}".to_owned()]);
    }

    #[test]
    fn field_access() {
        let f = CompiledJq::compile(".a").unwrap();
        let out = f.run(&json!({"a": 42}));
        assert_eq!(out, vec!["42".to_owned()]);
    }

    #[test]
    fn string_output_is_unquoted() {
        let f = CompiledJq::compile(".name").unwrap();
        let out = f.run(&json!({"name": "陕西煤业"}));
        assert_eq!(out, vec!["陕西煤业".to_owned()]);
    }

    #[test]
    fn select_and_array_iteration() {
        let f = CompiledJq::compile(
            r#".data | select(.count > 0) | .codes[] | "\(.code) \(.name)""#,
        )
        .unwrap();
        let input = json!({
            "event": "snapshot",
            "data": {
                "count": 2,
                "codes": [
                    {"code": "601225", "name": "陕西煤业"},
                    {"code": "002327", "name": "富安娜"}
                ]
            }
        });
        let out = f.run(&input);
        assert_eq!(
            out,
            vec![
                "601225 陕西煤业".to_owned(),
                "002327 富安娜".to_owned(),
            ]
        );
    }

    #[test]
    fn select_filters_out_when_predicate_false() {
        let f = CompiledJq::compile(r#"select(.event == "hit")"#).unwrap();
        let out = f.run(&json!({"event": "heartbeat"}));
        assert!(out.is_empty(), "heartbeat should be dropped");
    }

    #[test]
    fn syntax_error_surfaces() {
        match CompiledJq::compile("(((") {
            Ok(_) => panic!("malformed jq should error"),
            Err(err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("jq") || msg.contains("parse") || msg.contains("compile"),
                    "got: {msg}"
                );
            }
        }
    }
}
