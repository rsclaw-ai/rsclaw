// Brand file parsing, validation and code generation.
//
// Shared by `build.rs` (via `include!`) and the crate's unit tests, so it
// must not refer to `crate::` items and may only depend on `toml` + `std`.

/// Every string field of a brand file, in generation order.
/// `(toml key, const name)`; `urls.*` keys live under the `[urls]` table.
pub const FIELDS: &[(&str, &str)] = &[
    ("id", "ID"),
    ("product_name", "PRODUCT_NAME"),
    ("product_name_zh", "PRODUCT_NAME_ZH"),
    ("assistant_name", "ASSISTANT_NAME"),
    ("assistant_name_zh", "ASSISTANT_NAME_ZH"),
    ("emoji", "EMOJI"),
    ("website", "WEBSITE"),
    ("cli_name", "CLI_NAME"),
    ("home_dir", "HOME_DIR"),
    ("config_file", "CONFIG_FILE"),
    ("service_label", "SERVICE_LABEL"),
    ("service_unit", "SERVICE_UNIT"),
    ("windows_service", "WINDOWS_SERVICE"),
    ("service_display", "SERVICE_DISPLAY"),
    ("user_agent", "USER_AGENT"),
    ("default_port", "DEFAULT_PORT"),
    ("release_repo", "RELEASE_REPO"),
    ("urls.api", "API_URL"),
    ("urls.hub", "HUB_URL"),
    ("urls.app", "APP_URL"),
];

/// A parsed brand file: one value per entry of [`FIELDS`], same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrandSpec {
    pub values: Vec<(String, String)>,
}

impl BrandSpec {
    /// Value of a field by toml key (`"cli_name"`, `"urls.api"`).
    pub fn get(&self, key: &str) -> &str {
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
}

/// Parse a brand TOML document. Every field is required; unknown keys are
/// rejected so a typo cannot silently fall back to another brand's value.
pub fn parse(src: &str) -> Result<BrandSpec, String> {
    let table: toml::Table = src.parse().map_err(|e| format!("not valid TOML: {e}"))?;
    let urls = match table.get("urls") {
        Some(toml::Value::Table(t)) => t.clone(),
        Some(_) => return Err("`urls` must be a table".to_owned()),
        None => return Err("missing `[urls]` table".to_owned()),
    };
    for key in table.keys() {
        if key != "urls" && !FIELDS.iter().any(|(k, _)| k == key) {
            return Err(format!("unknown field `{key}`"));
        }
    }
    for key in urls.keys() {
        if !FIELDS.iter().any(|(k, _)| k.strip_prefix("urls.") == Some(key.as_str())) {
            return Err(format!("unknown field `urls.{key}`"));
        }
    }
    let mut values = Vec::with_capacity(FIELDS.len());
    for (key, _) in FIELDS {
        let raw = match key.strip_prefix("urls.") {
            Some(sub) => urls.get(sub),
            None => table.get(*key),
        };
        let value = match raw {
            Some(toml::Value::String(s)) => s.clone(),
            Some(toml::Value::Integer(n)) if *key == "default_port" => n.to_string(),
            Some(_) => return Err(format!("`{key}` must be a string")),
            None => return Err(format!("missing field `{key}`")),
        };
        values.push(((*key).to_owned(), value));
    }
    Ok(BrandSpec { values })
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn is_base_url(s: &str) -> bool {
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"));
    match rest {
        Some(host_and_path) => {
            !host_and_path.is_empty()
                && !s.ends_with('/')
                && !s.contains(['?', '#', ' ', '"', '\\'])
                && !s.chars().any(char::is_control)
        }
        None => false,
    }
}

/// Check every field. Errors name the offending key.
pub fn validate(spec: &BrandSpec) -> Result<(), String> {
    for (key, value) in &spec.values {
        if value.chars().any(char::is_control) {
            return Err(format!("`{key}` contains control characters"));
        }
        let ok = match key.as_str() {
            "id" | "cli_name" | "service_unit" | "windows_service" | "user_agent" => is_slug(value),
            // The dev instance uses port + 1, named profiles port + 2 ..= port + 255.
            "default_port" => value.parse::<u16>().is_ok_and(|p| (1024..=65000).contains(&p)),
            "home_dir" => {
                value.len() > 1
                    && value.starts_with('.')
                    && !value.contains("..")
                    && value[1..].chars().all(|c| {
                        c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
                    })
            }
            "config_file" => {
                is_slug(value.trim_end_matches(".json5")) && value.ends_with(".json5")
            }
            "service_label" => {
                !value.is_empty()
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            }
            "website" => {
                !value.is_empty()
                    && !value.contains("://")
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            }
            "release_repo" => {
                value.is_empty()
                    || value.split_once('/').is_some_and(|(owner, repo)| {
                        let part = |p: &str| {
                            !p.is_empty()
                                && p.chars().all(|c| {
                                    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
                                })
                        };
                        part(owner) && part(repo)
                    })
            }
            "emoji" => true,
            // An empty `urls.app` turns off release checks and the remote
            // defaults refresh (for brands without their own release host).
            "urls.app" => value.is_empty() || is_base_url(value),
            k if k.starts_with("urls.") => is_base_url(value),
            _ => !value.trim().is_empty(),
        };
        if !ok {
            return Err(format!("invalid value for `{key}`: {value:?}"));
        }
    }
    Ok(())
}

/// Rust source for `$OUT_DIR/brand.rs`: one `pub const` per field, derived
/// URL constants, and a `brand_<name>!()` literal macro for each so values
/// can feed `concat!` and attribute positions.
pub fn generate(spec: &BrandSpec) -> String {
    let api = spec.get("urls.api");
    let hub = spec.get("urls.hub");
    let app = spec.get("urls.app");
    let repo = spec.get("release_repo");
    let mut consts: Vec<(String, String, &str)> = FIELDS
        .iter()
        .filter(|(key, _)| *key != "default_port")
        .map(|(key, name)| ((*name).to_owned(), spec.get(key).to_owned(), *key))
        .collect();
    let releases = if repo.is_empty() {
        String::new()
    } else {
        format!("https://api.github.com/repos/{repo}/releases?per_page=10")
    };
    for (name, value, doc) in [
        ("API_V1_URL", format!("{api}/v1"), "`urls.api` + `/v1`"),
        (
            "DEFAULTS_URL",
            if app.is_empty() { String::new() } else { format!("{app}/defaults.toml") },
            "`urls.app` + `/defaults.toml` (empty when `urls.app` is empty)",
        ),
        (
            "VERSION_URL",
            if app.is_empty() { String::new() } else { format!("{app}/api/version") },
            "`urls.app` + `/api/version` (empty when `urls.app` is empty)",
        ),
        (
            "TOOLS_MANIFEST_URL",
            format!("{hub}/tools/manifest.json"),
            "`urls.hub` + `/tools/manifest.json`",
        ),
        ("HUB_META_URL", format!("{hub}/meta.json"), "`urls.hub` + `/meta.json`"),
        (
            "RELEASES_API_URL",
            releases,
            "GitHub releases API for `release_repo` (empty when unset)",
        ),
    ] {
        consts.push((name.to_owned(), value, doc));
    }

    let mut out = String::from("// @generated by rsclaw-brand/build.rs — do not edit.\n");
    let port: u16 = spec.get("default_port").parse().unwrap_or(18888);
    out.push_str(&format!(
        "/// Brand value: `default_port`, the gateway's default port.\npub const DEFAULT_PORT: u16 = {port};\n\
         /// `default_port` + 1: the `--dev` instance's port.\npub const DEV_PORT: u16 = {};\n\
         /// Literal form of [`DEFAULT_PORT`] for `concat!` and attributes.\n\
         #[macro_export]\nmacro_rules! brand_default_port {{ () => {{ {port} }}; }}\n",
        port + 1
    ));
    for (name, value, doc) in &consts {
        out.push_str(&format!("/// Brand value: {doc}.\npub const {name}: &str = {value:?};\n"));
    }
    for (name, value, _) in &consts {
        let mac = format!("brand_{}", name.to_ascii_lowercase());
        out.push_str(&format!(
            "/// Literal form of [`{name}`] for `concat!` and attributes.\n\
             #[macro_export]\nmacro_rules! {mac} {{ () => {{ {value:?} }}; }}\n"
        ));
    }
    out
}

/// Optional single-file assets of a brand directory:
/// `(const name, path relative to the brand directory, doc)`.
pub const ASSET_FILES: &[(&str, &str, &str)] = &[
    ("CONFIG_DEFAULTS", "config.defaults.json5", "default config layer, under the user's config"),
    ("POLICY_LOCKED", "policy.locked.json5", "locked config layer, over the user's config"),
    ("DEFAULTS_OVERRIDE", "defaults.override.toml", "override merged into the defaults catalog"),
    ("PROMPT_POLICY", "prompts/policy.md", "distributor policy prompt (any language)"),
    ("PROMPT_POLICY_ZH", "prompts/policy.zh.md", "distributor policy prompt for Chinese sessions"),
    ("PROMPT_POLICY_EN", "prompts/policy.en.md", "distributor policy prompt for other sessions"),
];

/// Languages with bootstrap templates.
pub const PROMPT_LANGS: &[&str] = &["zh", "en"];

/// Bootstrap workspace files a brand may template (`prompts/<lang>/<file>`).
pub const PROMPT_FILES: &[&str] = &["SOUL.md", "IDENTITY.md", "AGENTS.md", "USER.md", "HEARTBEAT.md"];

/// Rust source for the brand-directory assets. `found(rel)` returns the
/// absolute path of an existing asset (relative path as in [`ASSET_FILES`]
/// or `prompts/<lang>/<file>`), or `None`.
pub fn generate_assets(found: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    for (name, rel, doc) in ASSET_FILES {
        let value = match found(rel) {
            Some(path) => format!("Some(include_str!({path:?}))"),
            None => "None".to_owned(),
        };
        out.push_str(&format!(
            "/// Brand asset `{rel}`: {doc}.\npub const {name}: Option<&str> = {value};\n"
        ));
    }
    let mut arms = String::new();
    for lang in PROMPT_LANGS {
        for file in PROMPT_FILES {
            if let Some(path) = found(&format!("prompts/{lang}/{file}")) {
                arms.push_str(&format!(
                    "        ({lang:?}, {file:?}) => Some(include_str!({path:?})),\n"
                ));
            }
        }
    }
    let body = if arms.is_empty() {
        "    let _ = (lang, file);\n    None\n".to_owned()
    } else {
        format!("    match (lang, file) {{\n{arms}        _ => None,\n    }}\n")
    };
    out.push_str(&format!(
        "/// Brand bootstrap template `prompts/<lang>/<file>` (`lang` is `zh` or `en`).\n\
         pub fn prompt_template(lang: &str, file: &str) -> Option<&'static str> {{\n{body}}}\n"
    ));
    out
}
