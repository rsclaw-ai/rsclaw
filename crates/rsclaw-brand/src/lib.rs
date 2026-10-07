//! Compile-time brand identity.
//!
//! Product names, the data directory, the config file name, service
//! identifiers, the User-Agent and the hosted endpoints are generated from a
//! brand file at build time (`brand/rsclaw.toml` by default, or the file named
//! by `RSCLAW_BRAND_FILE`). See `docs/branding.md`.
//!
//! The default brand reproduces the historical RsClaw values byte for byte.

use std::borrow::Cow;
use std::sync::LazyLock;

include!(concat!(env!("OUT_DIR"), "/brand.rs"));

/// Default-brand tokens that user-facing text templates are written with,
/// paired with this build's value. Longer tokens come first so the scan in
/// [`rebrand`] prefers them.
fn text_tokens() -> Vec<(&'static str, String)> {
    vec![
        ("Crab AI Assistant", ASSISTANT_NAME.to_owned()),
        ("螃蟹AI助手", ASSISTANT_NAME_ZH.to_owned()),
        ("螃蟹AI", PRODUCT_NAME_ZH.to_owned()),
        ("RsClaw", PRODUCT_NAME.to_owned()),
        ("~/.rsclaw", format!("~/{HOME_DIR}")),
        ("rsclaw.json5", CONFIG_FILE.to_owned()),
        ("rsclaw.ai", WEBSITE.to_owned()),
        ("🦀", EMOJI.to_owned()),
    ]
}

/// The `rsclaw ` CLI-invocation token (word-bounded, see [`rebrand`]).
const CLI_TOKEN: &str = "rsclaw ";

static ACTIVE_TOKENS: LazyLock<Vec<(&'static str, String)>> = LazyLock::new(|| {
    let mut tokens: Vec<(&'static str, String)> = text_tokens()
        .into_iter()
        .filter(|(from, to)| from != to)
        .collect();
    let cli = format!("{CLI_NAME} ");
    if cli != CLI_TOKEN {
        tokens.push((CLI_TOKEN, cli));
    }
    tokens
});

/// True when this build uses the stock RsClaw text identity.
pub fn is_default_text() -> bool {
    ACTIVE_TOKENS.is_empty()
}

/// Rewrite a user-facing text template authored with the default brand
/// (`RsClaw`, `Crab AI Assistant`, `螃蟹AI助手`, `~/.rsclaw`, `rsclaw.json5`,
/// `rsclaw.ai`, `🦀`, and `rsclaw <subcommand>` invocations) into this
/// build's brand. With the default brand it returns the input unchanged
/// (borrowed), so prompt bytes and cache prefixes stay identical.
///
/// Identifiers that must not follow the brand (the `rsclaw` provider id,
/// `rsclaw-*` model names, `RSCLAW_*` env vars) are left alone: the CLI
/// token only matches `rsclaw ` at a word start, and the other tokens are
/// distinct strings.
pub fn rebrand(text: &str) -> Cow<'_, str> {
    rebrand_with(text, &ACTIVE_TOKENS)
}

fn rebrand_with<'a>(text: &'a str, tokens: &[(&'static str, String)]) -> Cow<'a, str> {
    if tokens.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut i = 0;
    'scan: while i < text.len() {
        let rest = &text[i..];
        for (from, to) in tokens {
            if !rest.starts_with(from) {
                continue;
            }
            if *from == CLI_TOKEN {
                let prev = text[..i].chars().next_back();
                let bounded = prev.is_none_or(|c| {
                    !(c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | '$'))
                });
                if !bounded {
                    continue;
                }
            }
            out.push_str(to);
            i += from.len();
            changed = true;
            continue 'scan;
        }
        let ch = rest.chars().next().unwrap_or_default();
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// One printed line, branded the way CLI output is: `line` goes through
/// [`rebrand`] when its format `template` contains default-brand words, and
/// is returned as is otherwise. Interpolated data (JSON dumps, replies, file
/// contents) printed through a brand-free template such as `"{}"` is never
/// rewritten.
pub fn brand_line<'a>(template: &str, line: &'a str) -> Cow<'a, str> {
    brand_line_with(template, line, &ACTIVE_TOKENS)
}

fn brand_line_with<'a>(
    template: &str,
    line: &'a str,
    tokens: &[(&'static str, String)],
) -> Cow<'a, str> {
    if matches!(rebrand_with(template, tokens), Cow::Owned(_)) {
        rebrand_with(line, tokens)
    } else {
        Cow::Borrowed(line)
    }
}

/// Data directory name for a `--dev` / `--profile <name>` instance:
/// `<home_dir>-<suffix>` (e.g. `.rsclaw-dev`).
pub fn profile_home_dir(suffix: &str) -> String {
    format!("{HOME_DIR}-{suffix}")
}

/// Per-project config file name looked up in the working directory:
/// `.` + `config_file` (e.g. `.rsclaw.json5`).
pub fn local_config_file() -> String {
    format!(".{CONFIG_FILE}")
}

/// `api_url` joined with `path` (which should start with `/`).
pub fn api_url(path: &str) -> String {
    format!("{API_URL}{path}")
}

/// `<user_agent>/<version>`.
pub fn user_agent(version: &str) -> String {
    format!("{USER_AGENT}/{version}")
}

#[cfg(test)]
#[allow(dead_code)]
mod spec {
    include!("spec.rs");
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_FILE: &str = include_str!("../brand/rsclaw.toml");

    #[test]
    fn default_brand_values_are_unchanged() {
        assert_eq!(ID, "rsclaw");
        assert_eq!(PRODUCT_NAME, "RsClaw");
        assert_eq!(CLI_NAME, "rsclaw");
        assert_eq!(HOME_DIR, ".rsclaw");
        assert_eq!(CONFIG_FILE, "rsclaw.json5");
        assert_eq!(SERVICE_LABEL, "ai.rsclaw.gateway");
        assert_eq!(SERVICE_UNIT, "rsclaw-gateway");
        assert_eq!(WINDOWS_SERVICE, "rsclaw");
        assert_eq!(SERVICE_DISPLAY, "RsClaw AI Gateway");
        assert_eq!(USER_AGENT, "rsclaw");
        assert_eq!(API_URL, "https://api.rsclaw.ai");
        assert_eq!(API_V1_URL, "https://api.rsclaw.ai/v1");
        assert_eq!(HUB_URL, "https://hub.rsclaw.ai");
        assert_eq!(
            TOOLS_MANIFEST_URL,
            "https://hub.rsclaw.ai/tools/manifest.json"
        );
        assert_eq!(HUB_META_URL, "https://hub.rsclaw.ai/meta.json");
        assert_eq!(DEFAULTS_URL, "https://app.rsclaw.ai/defaults.toml");
        assert_eq!(VERSION_URL, "https://app.rsclaw.ai/api/version");
        assert_eq!(
            RELEASES_API_URL,
            "https://api.github.com/repos/rsclaw-ai/rsclaw/releases?per_page=10"
        );
        assert_eq!(brand_user_agent!(), "rsclaw");
        assert_eq!(concat!(brand_cli_name!(), "/dev"), "rsclaw/dev");
        assert_eq!(profile_home_dir("dev"), ".rsclaw-dev");
        assert_eq!(local_config_file(), ".rsclaw.json5");
        assert_eq!(user_agent("1.2"), "rsclaw/1.2");
    }

    #[test]
    fn default_brand_text_is_borrowed_verbatim() {
        assert!(is_default_text());
        let s = "You are Crab AI Assistant, powered by the RsClaw Agent Engine. Run `rsclaw gateway start`.";
        assert!(matches!(rebrand(s), Cow::Borrowed(b) if b == s));
    }

    #[test]
    fn default_file_parses_and_validates() {
        let spec = spec::parse(DEFAULT_FILE).expect("parse");
        spec::validate(&spec).expect("validate");
        assert_eq!(spec.get("cli_name"), "rsclaw");
        let code = spec::generate(&spec);
        assert!(code.contains("pub const HOME_DIR: &str = \".rsclaw\";"));
        assert!(code.contains("macro_rules! brand_api_v1_url"));
    }

    fn acme() -> String {
        DEFAULT_FILE
            .replace("id = \"rsclaw\"", "id = \"acme\"")
            .replace("product_name = \"RsClaw\"", "product_name = \"Acme\"")
            .replace("cli_name = \"rsclaw\"", "cli_name = \"acme\"")
            .replace("home_dir = \".rsclaw\"", "home_dir = \".acme\"")
            .replace("https://api.rsclaw.ai", "https://api.acme.example")
            .replace("release_repo = \"rsclaw-ai/rsclaw\"", "release_repo = \"\"")
    }

    #[test]
    fn other_brand_generates_its_own_constants() {
        let spec = spec::parse(&acme()).expect("parse");
        spec::validate(&spec).expect("validate");
        let code = spec::generate(&spec);
        assert!(code.contains("pub const HOME_DIR: &str = \".acme\";"));
        assert!(code.contains("pub const API_V1_URL: &str = \"https://api.acme.example/v1\";"));
        assert!(code.contains("pub const CLI_NAME: &str = \"acme\";"));
        assert!(code.contains("pub const RELEASES_API_URL: &str = \"\";"));
    }

    #[test]
    fn rebrand_scan_replaces_tokens_and_respects_word_bounds() {
        let tokens = vec![
            ("Crab AI Assistant", "Acme Helper".to_owned()),
            ("RsClaw", "Acme".to_owned()),
            ("~/.rsclaw", "~/.acme".to_owned()),
            ("rsclaw.json5", "acme.json5".to_owned()),
            (CLI_TOKEN, "acme ".to_owned()),
        ];
        let out = rebrand_with(
            "Crab AI Assistant on RsClaw: edit ~/.rsclaw/rsclaw.json5, run `rsclaw gateway start`. 螃蟹",
            &tokens,
        );
        assert_eq!(
            out,
            "Acme Helper on Acme: edit ~/.acme/acme.json5, run `acme gateway start`. 螃蟹"
        );
        // Provider ids, env vars and model names are not CLI invocations.
        let keep = "providers.rsclaw apiKey, $RSCLAW_API_KEY, rsclaw-agent-v1, x/rsclaw y";
        assert!(matches!(rebrand_with(keep, &tokens), Cow::Borrowed(_)));
        assert_eq!(rebrand_with("rsclaw doctor", &tokens), "acme doctor");
    }

    fn acme_tokens() -> Vec<(&'static str, String)> {
        vec![
            ("RsClaw", "Acme".to_owned()),
            ("~/.rsclaw", "~/.acme".to_owned()),
            ("rsclaw.json5", "acme.json5".to_owned()),
            (CLI_TOKEN, "acme ".to_owned()),
        ]
    }

    #[test]
    fn cli_messages_are_rebranded() {
        let tokens = acme_tokens();
        for (from, to) in [
            (
                "run `rsclaw doctor --fix` to apply 2 auto-fix(es)",
                "run `acme doctor --fix` to apply 2 auto-fix(es)",
            ),
            (
                "gateway not reachable at port 18888; start it with: rsclaw gateway start",
                "gateway not reachable at port 18888; start it with: acme gateway start",
            ),
            ("rsclaw tools v2026.10.1", "acme tools v2026.10.1"),
            (
                "  installed      ~/.rsclaw/tools",
                "  installed      ~/.acme/tools",
            ),
            (
                "no config file found. Edit rsclaw.json5 or run `rsclaw setup`.",
                "no config file found. Edit acme.json5 or run `acme setup`.",
            ),
            (
                "Isolate state to ~/.rsclaw-dev, port offset applied.",
                "Isolate state to ~/.acme-dev, port offset applied.",
            ),
        ] {
            assert_eq!(rebrand_with(from, &tokens), to);
        }
    }

    #[test]
    fn brand_line_only_rewrites_branded_templates() {
        let tokens = acme_tokens();
        // A prose template that names the CLI: the whole line follows the brand.
        let line = "start it with: rsclaw gateway start (port 18888)";
        assert_eq!(
            brand_line_with(
                "start it with: rsclaw gateway start (port {port})",
                line,
                &tokens
            ),
            "start it with: acme gateway start (port 18888)"
        );
        // Data printed through a brand-free template stays byte for byte.
        let data = r#"{"reply":"RsClaw says: run rsclaw doctor"}"#;
        assert!(matches!(
            brand_line_with("{}", data, &tokens),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            brand_line_with("{text}", data, &tokens),
            Cow::Borrowed(_)
        ));
        // Default brand: nothing changes.
        assert!(matches!(
            brand_line_with("run `rsclaw x`", "run `rsclaw x`", &[]),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn invalid_brand_files_are_rejected() {
        let check = |src: String| spec::parse(&src).and_then(|s| spec::validate(&s));
        let base = DEFAULT_FILE.to_owned();
        assert!(check(base.clone()).is_ok());
        assert!(check(base.replace("home_dir = \".rsclaw\"", "home_dir = \"rsclaw\"")).is_err());
        assert!(check(base.replace("home_dir = \".rsclaw\"", "home_dir = \".a/b\"")).is_err());
        assert!(check(base.replace("cli_name = \"rsclaw\"", "cli_name = \"Rs Claw\"")).is_err());
        assert!(
            check(base.replace("https://hub.rsclaw.ai\"", "https://hub.rsclaw.ai/\"")).is_err()
        );
        assert!(check(base.replace("https://app.rsclaw.ai", "ftp://app.rsclaw.ai")).is_err());
        assert!(
            check(base.replace("config_file = \"rsclaw.json5\"", "config_file = \"x.json\""))
                .is_err()
        );
        assert!(
            check(base.replace(
                "release_repo = \"rsclaw-ai/rsclaw\"",
                "release_repo = \"nope\""
            ))
            .is_err()
        );
        assert!(check(format!("{base}\nextra = \"x\"\n")).is_err());
        assert!(check(base.replace("emoji = \"🦀\"\n", "")).is_err());
    }
}
