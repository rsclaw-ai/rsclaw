//! /watch command parser.
//!
//! Grammar (see spec §"命令文法"):
//!
//!   /watch ::= START | LIST | STOP
//!   START ::= /watch [file|sse|shell] <args> [FLAGS...]
//!
//! Auto-detect (when kind is omitted):
//!   - first token starts with `http://` or `https://` → sse
//!   - first token is a path (`/`, `~/`, `./`, `../`, Windows `[A-Z]:[\\/]`) → file
//!   - otherwise → error (caller must prefix with `shell` for raw shell commands)

use anyhow::{anyhow, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedCommand {
    Start(WatchSpec),
    List,
    Stop(StopTarget),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopTarget {
    One(String),    // a watch id, e.g. "w_abc12345"
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WatchSpec {
    pub kind: SourceKind,
    pub raw_source: String,                // the literal SOURCE_ARGS (un-normalized)
    pub headers: Vec<(String, String)>,    // -H 'Name: value' pairs (SSE only)
    pub grep: Option<String>,              // --grep <regex>
    pub jq: Option<String>,                // --jq <expr>  (stretch)
    pub rate_ms: u64,                      // --rate <ms>, default 2000, 0 = unlimited
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceKind {
    File,
    Sse,
    Shell,
}

/// Parse the body of a `/watch` command. The leading `/watch ` has already been stripped.
///
/// Returns `Err` for malformed input (the caller formats it into a chat reply).
pub fn parse(body: &str) -> Result<ParsedCommand> {
    let body = body.trim();
    if body.is_empty() {
        return Err(anyhow!("usage: /watch <source> [flags] | /watch list | /watch stop <id|all>"));
    }

    // Management commands first — they are exact-prefix matches.
    if body == "list" {
        return Ok(ParsedCommand::List);
    }
    if let Some(rest) = body.strip_prefix("stop") {
        let arg = rest.trim();
        if arg.is_empty() {
            return Err(anyhow!("usage: /watch stop <id|all>"));
        }
        if arg == "all" {
            return Ok(ParsedCommand::Stop(StopTarget::All));
        }
        return Ok(ParsedCommand::Stop(StopTarget::One(arg.to_owned())));
    }

    Ok(ParsedCommand::Start(parse_start(body)?))
}

fn parse_start(body: &str) -> Result<WatchSpec> {
    // First token decides whether we have an explicit kind.
    let (first, rest) = split_first_token(body);
    let (kind, args_body) = match first {
        "file" => (SourceKind::File, rest.trim()),
        "sse" => (SourceKind::Sse, rest.trim()),
        "shell" => (SourceKind::Shell, rest.trim()),
        _ => {
            // Auto-detect: leave the body intact (first wasn't a kind keyword).
            (auto_detect_kind(body)?, body.trim())
        }
    };

    if args_body.is_empty() {
        return Err(anyhow!("missing source argument"));
    }

    // Split args from flags. Flags start at the first ` -H ` / ` --grep ` / `
    // --jq ` / ` --rate ` token boundary.
    let (raw_source, flag_tail) = split_source_and_flags(args_body);

    let mut spec = WatchSpec {
        kind,
        raw_source: raw_source.trim().to_owned(),
        headers: Vec::new(),
        grep: None,
        jq: None,
        rate_ms: 2000,
    };

    // Flag parsing is done in Task 4; for now if flag_tail is non-empty, just
    // store the rest into raw_source so the test for first-token routing passes,
    // then Task 4 will replace this.
    if !flag_tail.is_empty() {
        spec.raw_source = format!("{} {}", spec.raw_source, flag_tail).trim().to_owned();
    }

    if spec.raw_source.is_empty() {
        return Err(anyhow!("missing source argument"));
    }
    Ok(spec)
}

fn split_first_token(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], &s[idx..]),
        None => (s, ""),
    }
}

fn auto_detect_kind(body: &str) -> Result<SourceKind> {
    let first = body.split_whitespace().next().unwrap_or("");

    if first.starts_with("http://") || first.starts_with("https://") {
        return Ok(SourceKind::Sse);
    }
    if first.starts_with('/')
        || first.starts_with("~/")
        || first.starts_with("./")
        || first.starts_with("../")
        || is_windows_drive_path(first)
    {
        return Ok(SourceKind::File);
    }
    Err(anyhow!(
        "unknown source `{first}`; prefix with file/sse/shell or use a URL/path"
    ))
}

fn is_windows_drive_path(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && matches!(chars.next(), Some(':'))
        && matches!(chars.next(), Some('\\') | Some('/'))
}

/// Split the source-args body at the first flag token. Flags begin with
/// ` -H `, ` --grep `, ` --jq `, ` --rate `, ` --only `, ` --tee ` (note the
/// leading space — flags inside a path or URL shouldn't trigger the split).
fn split_source_and_flags(s: &str) -> (&str, &str) {
    const FLAG_HEADS: &[&str] = &[" -H ", " --grep ", " --jq ", " --rate ", " --only ", " --tee "];
    let mut best_idx = s.len();
    for head in FLAG_HEADS {
        if let Some(idx) = s.find(head) {
            if idx < best_idx {
                best_idx = idx;
            }
        }
    }
    if best_idx == s.len() {
        (s, "")
    } else {
        (&s[..best_idx], s[best_idx + 1..].trim_start())
        // Note: best_idx points at the leading space, so +1 skips it.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_error() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn list_command() {
        assert_eq!(parse("list").unwrap(), ParsedCommand::List);
        assert_eq!(parse("  list  ").unwrap(), ParsedCommand::List);
    }

    #[test]
    fn stop_command() {
        assert_eq!(
            parse("stop w_abc12345").unwrap(),
            ParsedCommand::Stop(StopTarget::One("w_abc12345".into()))
        );
        assert_eq!(parse("stop all").unwrap(), ParsedCommand::Stop(StopTarget::All));
        assert!(parse("stop").is_err());
    }

    #[test]
    fn auto_detect_url() {
        let p = parse("https://api.example/events").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.kind, SourceKind::Sse);
            assert_eq!(spec.raw_source, "https://api.example/events");
        } else {
            panic!("not a Start");
        }
    }

    #[test]
    fn auto_detect_unix_path() {
        let p = parse("/var/log/app.log").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.kind, SourceKind::File);
            assert_eq!(spec.raw_source, "/var/log/app.log");
        }
    }

    #[test]
    fn auto_detect_relative_path() {
        for src in ["~/log/x", "./x", "../x"] {
            let p = parse(src).unwrap();
            if let ParsedCommand::Start(spec) = p {
                assert_eq!(spec.kind, SourceKind::File, "src={src}");
            }
        }
    }

    #[test]
    fn auto_detect_windows_drive_path() {
        let p = parse(r"C:\logs\app.log").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.kind, SourceKind::File);
        }
    }

    #[test]
    fn explicit_kind_overrides_autodetect() {
        let p = parse("shell tail -f x.log").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.kind, SourceKind::Shell);
            assert_eq!(spec.raw_source, "tail -f x.log");
        }
    }

    #[test]
    fn raw_command_without_kind_errors() {
        // `tail -f x` doesn't auto-detect (not URL, not path) and has no explicit kind.
        assert!(parse("tail -f x").is_err());
    }
}
