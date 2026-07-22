//! Schema-driven tool-argument repair — the generic layer behind the
//! one-off fixes that kept accumulating (write_file content-as-object,
//! todo status normalization, trailing-`\n` trims on enum args).
//!
//! Small models — v1's raw-text XML params especially — hand us values
//! that are *unambiguously* convertible to what the schema wants. Bouncing
//! those calls with an error burns a round-trip and (worse) often sends
//! the model into an identical-retry loop. When intent is clear, repair
//! silently; when it isn't, leave the value alone and let the tool's own
//! validation speak.
//!
//! Repairs are deliberately conservative:
//! - string wanted, scalar given → to_string; object/array given → pretty JSON
//!   (the write_file `.json` case)
//! - number/integer wanted, numeric string given → parsed
//! - boolean wanted, "true"/"false" string given → parsed
//! - enum property → trim whitespace (v1 leaks trailing `\n`), then
//!   case-insensitive snap to the canonical enum value
//! - everything else untouched — no guessing

use serde_json::Value;

/// Repair `args` in place against the tool's JSON schema. Returns a
/// human-readable note per repair applied (empty = untouched), for
/// debug-level logging at the call site.
pub fn sanitize_args(schema: &Value, args: &mut Value) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return notes;
    };
    let Some(obj) = args.as_object_mut() else {
        return notes;
    };

    for (key, prop) in props {
        let Some(val) = obj.get_mut(key) else {
            continue;
        };
        let expected = prop.get("type").and_then(Value::as_str).unwrap_or("");
        let enum_vals = prop.get("enum").and_then(Value::as_array);

        match (expected, &*val) {
            // ---- string expected, non-string given ----
            ("string", Value::Number(n)) => {
                notes.push(format!("{key}: number → string"));
                *val = Value::String(n.to_string());
            }
            ("string", Value::Bool(b)) => {
                notes.push(format!("{key}: bool → string"));
                *val = Value::String(b.to_string());
            }
            ("string", Value::Object(_)) | ("string", Value::Array(_)) => {
                notes.push(format!("{key}: json → string"));
                let pretty = serde_json::to_string_pretty(val).unwrap_or_default();
                *val = Value::String(pretty);
            }
            // ---- number/integer expected, numeric string given ----
            ("number", Value::String(s)) | ("integer", Value::String(s)) => {
                let t = s.trim();
                if expected == "integer" {
                    if let Ok(i) = t.parse::<i64>() {
                        notes.push(format!("{key}: string → integer"));
                        *val = Value::Number(i.into());
                    }
                } else if let Ok(f) = t.parse::<f64>()
                    && let Some(n) = serde_json::Number::from_f64(f)
                {
                    notes.push(format!("{key}: string → number"));
                    *val = Value::Number(n);
                }
            }
            // ---- boolean expected, stringly bool given ----
            ("boolean", Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" => {
                    notes.push(format!("{key}: string → true"));
                    *val = Value::Bool(true);
                }
                "false" => {
                    notes.push(format!("{key}: string → false"));
                    *val = Value::Bool(false);
                }
                _ => {}
            },
            _ => {}
        }

        // ---- enum snap (after type repair so we match on strings) ----
        if let Some(allowed) = enum_vals
            && let Value::String(s) = &*val
        {
            let exact = allowed.iter().any(|a| a.as_str() == Some(s.as_str()));
            if !exact {
                let trimmed = s.trim();
                let snapped = allowed
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|a| a.eq_ignore_ascii_case(trimmed));
                if let Some(canon) = snapped {
                    notes.push(format!("{key}: '{s}' → enum '{canon}'"));
                    *val = Value::String(canon.to_owned());
                } else if trimmed != s {
                    // Not snappable, but trailing whitespace alone may be
                    // the problem (v1 newline leak) — trim and retry match.
                    notes.push(format!("{key}: trimmed whitespace"));
                    *val = Value::String(trimmed.to_owned());
                }
            }
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {"type": "string"},
                "count":   {"type": "integer"},
                "ratio":   {"type": "number"},
                "wait":    {"type": "boolean"},
                "action":  {"type": "string", "enum": ["list", "add", "remove"]},
                "free":    {"type": "string"}
            }
        })
    }

    #[test]
    fn repairs_unambiguous_mismatches() {
        let mut args = json!({
            "content": {"checked": true},
            "count": "3",
            "ratio": "0.7",
            "wait": "false",
            "action": "Add\n",
        });
        let notes = sanitize_args(&schema(), &mut args);
        assert!(args["content"].is_string());
        assert!(args["content"].as_str().unwrap().contains("\"checked\""));
        assert_eq!(args["count"], json!(3));
        assert_eq!(args["ratio"], json!(0.7));
        assert_eq!(args["wait"], json!(false));
        assert_eq!(args["action"], json!("add"));
        assert_eq!(notes.len(), 5, "{notes:?}");
    }

    #[test]
    fn leaves_valid_and_ambiguous_values_alone() {
        let mut args = json!({
            "content": "plain text\n",   // trailing \n in free string is legit
            "count": 5,
            "action": "list",
            "free": "  spaced  ",         // no enum — untouched
        });
        let notes = sanitize_args(&schema(), &mut args);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(args["content"], json!("plain text\n"));
        assert_eq!(args["free"], json!("  spaced  "));
    }

    #[test]
    fn unsnappable_enum_gets_trimmed_only() {
        let mut args = json!({"action": "destroy \n"});
        let notes = sanitize_args(&schema(), &mut args);
        assert_eq!(args["action"], json!("destroy"));
        assert_eq!(notes.len(), 1);
    }
}
