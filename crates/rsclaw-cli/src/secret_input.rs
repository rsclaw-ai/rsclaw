//! Shared clap value parser for secret-bearing flags (tokens, passwords).
//!
//! A literal `-` means "read the secret from stdin", so it never has to
//! appear in argv (visible in `ps` / shell history) or in the environment.

use std::io::Read;

/// clap `value_parser` for secret flags: `-` reads one secret from stdin
/// (trailing CR/LF trimmed); any other value is returned unchanged.
pub fn secret_value(raw: &str) -> Result<String, String> {
    if raw != "-" {
        return Ok(raw.to_owned());
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("failed to read secret from stdin: {e}"))?;
    normalize_stdin_secret(buf)
}

/// Strip the trailing newline(s) a pipe or heredoc adds; reject empty input.
fn normalize_stdin_secret(mut buf: String) -> Result<String, String> {
    while buf.ends_with('\n') || buf.ends_with('\r') {
        buf.pop();
    }
    if buf.is_empty() {
        return Err("no secret received on stdin".to_owned());
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_value_passes_literal_through() {
        assert_eq!(secret_value("abc").as_deref(), Ok("abc"));
    }

    #[test]
    fn normalize_stdin_secret_trims_trailing_newlines_only() {
        assert_eq!(normalize_stdin_secret("tok\n".into()).as_deref(), Ok("tok"));
        assert_eq!(normalize_stdin_secret("tok\r\n".into()).as_deref(), Ok("tok"));
        assert_eq!(normalize_stdin_secret(" tok ".into()).as_deref(), Ok(" tok "));
        assert!(normalize_stdin_secret("\n".into()).is_err());
    }
}
