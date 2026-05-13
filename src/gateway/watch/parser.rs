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

    if !flag_tail.is_empty() {
        apply_flags(&mut spec, flag_tail)?;
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

/// Split the source-args body at the first flag token. A flag is either one
/// of the known short/long heads (e.g. ` -H `, ` --grep `) **or any unknown
/// long flag** of the form ` --<alpha>...` — those still split so apply_flags
/// can raise an "unknown flag" error. Single-letter `-x` is NOT treated as a
/// flag boundary unless it's a known head, so `shell tail -f x` correctly
/// keeps `-f` as part of the shell command.
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
    // Also catch unknown long flags (`--xxx`) so apply_flags can reject them.
    let bytes = s.as_bytes();
    let mut i: usize = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b' '
            && bytes[i + 1] == b'-'
            && bytes[i + 2] == b'-'
            && bytes[i + 3].is_ascii_alphabetic()
            && i < best_idx
        {
            best_idx = i;
            break;
        }
        i += 1;
    }
    if best_idx == s.len() {
        (s, "")
    } else {
        (&s[..best_idx], s[best_idx + 1..].trim_start())
    }
}

fn apply_flags(spec: &mut WatchSpec, tail: &str) -> Result<()> {
    // Tokenize the tail respecting single- and double-quoted values so
    // `-H 'Auth: Bearer x'` keeps the quoted value as one token.
    let tokens = tokenize_flags(tail)?;
    let mut i = 0;
    while i < tokens.len() {
        let tok = &tokens[i];
        i += 1;
        match tok.as_str() {
            "-H" => {
                let val = tokens.get(i).ok_or_else(|| anyhow!("-H needs a value"))?.clone();
                i += 1;
                let (name, value) = val
                    .split_once(':')
                    .ok_or_else(|| anyhow!("-H value must be `Name: value`, got `{val}`"))?;
                spec.headers.push((name.trim().to_owned(), value.trim().to_owned()));
            }
            "--grep" => {
                let val = tokens.get(i).ok_or_else(|| anyhow!("--grep needs a regex"))?.clone();
                i += 1;
                // Validate regex compiles now so the user gets a clean error.
                regex::Regex::new(&val).map_err(|e| anyhow!("invalid regex: {e}"))?;
                spec.grep = Some(val);
            }
            "--jq" => {
                // jq runtime is a stretch goal (plan §Task S1). Reject up
                // front — silently passing events through a stub gives users
                // a fake-working filter. Use `--grep <regex>` until jaq lands.
                tokens.get(i).ok_or_else(|| anyhow!("--jq needs an expression"))?;
                return Err(anyhow!(
                    "--jq not implemented yet (v1); use --grep <regex>"
                ));
            }
            "--rate" => {
                let val = tokens.get(i).ok_or_else(|| anyhow!("--rate needs a number"))?.clone();
                i += 1;
                spec.rate_ms = val.parse::<u64>().map_err(|_| anyhow!("--rate must be a number, got `{val}`"))?;
            }
            "--only" | "--tee" => {
                // Stretch — accept but ignore in v1 so the command still parses.
                tokens.get(i).ok_or_else(|| anyhow!("{tok} needs a value"))?;
                i += 1;
            }
            unknown => return Err(anyhow!("unknown flag: `{unknown}`")),
        }
    }
    Ok(())
}

/// Split a flag tail into tokens, honoring single- and double-quoted strings.
/// `-H 'Auth: Bearer x' --grep "ERR"` → ["-H", "Auth: Bearer x", "--grep", "ERR"]
fn tokenize_flags(s: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                // Closing quote — emit the buffered token even if empty.
                out.push(std::mem::take(&mut buf));
            }
            (Some(_), c) => buf.push(c),
            (None, c) if c == '\'' || c == '"' => {
                quote = Some(c);
            }
            (None, c) if c.is_whitespace() => {
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
            }
            (None, c) => buf.push(c),
        }
    }
    if quote.is_some() {
        return Err(anyhow!("unterminated quoted string in flags"));
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    Ok(out)
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

    #[test]
    fn flag_parsing_grep() {
        let p = parse("/var/log/x --grep ERR").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.grep, Some("ERR".to_owned()));
            assert_eq!(spec.raw_source, "/var/log/x");
        }
    }

    #[test]
    fn flag_parsing_rate() {
        let p = parse("/var/log/x --rate 5000").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.rate_ms, 5000);
        }
    }

    #[test]
    fn flag_parsing_rate_zero() {
        let p = parse("/var/log/x --rate 0").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.rate_ms, 0);
        }
    }

    #[test]
    fn flag_parsing_header_quoted() {
        let p = parse("https://x -H 'Authorization: Bearer abc def'").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.headers, vec![("Authorization".to_owned(), "Bearer abc def".to_owned())]);
        }
    }

    #[test]
    fn flag_parsing_multiple_headers() {
        let p = parse("https://x -H 'A: 1' -H 'B: 2'").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.headers.len(), 2);
            assert_eq!(spec.headers[0], ("A".to_owned(), "1".to_owned()));
            assert_eq!(spec.headers[1], ("B".to_owned(), "2".to_owned()));
        }
    }

    #[test]
    fn flag_parsing_invalid_regex_errors() {
        assert!(parse("/var/log/x --grep [unclosed").is_err());
    }

    #[test]
    fn flag_parsing_unknown_flag_errors() {
        assert!(parse("/var/log/x --bogus value").is_err());
    }

    #[test]
    fn flag_parsing_unterminated_quote_errors() {
        assert!(parse("https://x -H 'unclosed").is_err());
    }

    #[test]
    fn flag_parsing_jq_is_rejected_in_v1() {
        let err = parse("/var/log/x --jq '.code'").unwrap_err();
        assert!(err.to_string().contains("--jq"), "got: {err}");
        assert!(err.to_string().contains("not implemented"), "got: {err}");
    }

    #[test]
    fn flag_parsing_default_rate_is_2000() {
        let p = parse("/var/log/x").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.rate_ms, 2000);
        }
    }

    #[test]
    fn shell_source_preserves_single_letter_dash_args() {
        let p = parse("shell tail -f /var/log/x").unwrap();
        if let ParsedCommand::Start(spec) = p {
            assert_eq!(spec.kind, SourceKind::Shell);
            assert_eq!(spec.raw_source, "tail -f /var/log/x");
        } else {
            panic!("expected Start");
        }
    }
}
