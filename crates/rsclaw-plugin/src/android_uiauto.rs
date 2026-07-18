//! CLS tunnel transport for Android agent and UIAutomator2 host imports.
//!
//! The gateway never opens ADB itself. High-level calls are deliberately
//! limited to operations whose arguments do not contain customer text. Native
//! WebDriver requests carry JSON through a mode-0600 temporary file so message
//! bodies and selectors are not exposed in the process argument list.

use std::{fs::OpenOptions, io::Write, path::PathBuf, process::Stdio, time::Duration};

use base64::Engine as _;
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

const DEFAULT_UIAUTO_PORT: u16 = 6790;
const MAX_ARGS_BYTES: usize = 64 * 1024;
const MAX_RAW_BODY_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 20 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 8 * 1024;
const MAX_PATH_BYTES: usize = 512;
const MAX_STRING_ARG_BYTES: usize = 4096;
const MAX_SELECTOR_BYTES: usize = 64 * 1024;
const MAX_INPUT_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    cls_bin: String,
    node: String,
    port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    Integer,
    Unsigned,
    String,
}

#[derive(Debug, Clone, Copy)]
struct ArgSpec {
    key: &'static str,
    flag: &'static str,
    kind: ArgKind,
    required: bool,
}

const NO_ARGS: &[ArgSpec] = &[];
const KEY_ARGS: &[ArgSpec] = &[ArgSpec::new("key", "--key", ArgKind::String, true)];
const TAP_ARGS: &[ArgSpec] = &[
    ArgSpec::new("x", "--x", ArgKind::Integer, true),
    ArgSpec::new("y", "--y", ArgKind::Integer, true),
];
const SWIPE_ARGS: &[ArgSpec] = &[
    ArgSpec::new("x1", "--x1", ArgKind::Integer, true),
    ArgSpec::new("y1", "--y1", ArgKind::Integer, true),
    ArgSpec::new("x2", "--x2", ArgKind::Integer, true),
    ArgSpec::new("y2", "--y2", ArgKind::Integer, true),
    ArgSpec::new("durationMs", "--duration-ms", ArgKind::Unsigned, false),
];
const LAUNCH_ARGS: &[ArgSpec] = &[
    ArgSpec::new("package", "--package", ArgKind::String, true),
    ArgSpec::new("activity", "--activity", ArgKind::String, false),
];

impl ArgSpec {
    const fn new(key: &'static str, flag: &'static str, kind: ArgKind, required: bool) -> Self {
        Self {
            key,
            flag,
            kind,
            required,
        }
    }
}

struct TempJsonFile {
    path: PathBuf,
}

impl TempJsonFile {
    fn create(contents: &[u8]) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "rsclaw-uiauto-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| format!("android uiauto: create request file: {error}"))?;
        if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
            if let Err(cleanup_error) = std::fs::remove_file(&path)
                && cleanup_error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    path = %path.display(),
                    error = %cleanup_error,
                    "failed to remove incomplete Android UIAutomator2 request file"
                );
            }
            return Err(format!("android uiauto: write request file: {error}"));
        }
        Ok(Self { path })
    }
}

impl Drop for TempJsonFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %error,
                "failed to remove Android UIAutomator2 request file"
            );
        }
    }
}

/// Run one non-sensitive high-level `cls android` agent command.
pub(crate) async fn call(command: &str, args_json: &str) -> Result<String, String> {
    if args_json.len() > MAX_ARGS_BYTES {
        return Err(format!(
            "android uiauto: args exceed {MAX_ARGS_BYTES} bytes"
        ));
    }
    let config = Config::from_env()?;
    let args = build_call_args(&config, command, args_json)?;
    run_cls(&config.cls_bin, &args, deadline_for(command)).await
}

/// Forward one native UIAutomator2/WebDriver JSON request through CLS.
pub(crate) async fn raw(
    method: &str,
    path: &str,
    json_body: Option<&str>,
) -> Result<String, String> {
    let config = Config::from_env()?;
    let method = validate_raw_request(method, path, json_body)?;
    let request_file = match json_body {
        Some(body) => Some(TempJsonFile::create(body.as_bytes())?),
        None => None,
    };
    let mut args = uiauto_base_args(&config, "raw");
    args.push("--method".to_string());
    args.push(method);
    if let Some(file) = request_file.as_ref() {
        args.push("--json-file".to_string());
        args.push(file.path.to_string_lossy().into_owned());
    }
    args.push(path.to_string());
    run_cls(&config.cls_bin, &args, Duration::from_secs(30)).await
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let node = std::env::var("RSCLAW_ANDROID_NODE")
            .map_err(|_| "android uiauto: RSCLAW_ANDROID_NODE is required".to_string())?;
        let cls_bin = std::env::var("RSCLAW_CLS_BIN").unwrap_or_else(|_| "cls".to_string());
        let port = match std::env::var("RSCLAW_ANDROID_UIAUTO_PORT") {
            Ok(value) => value.parse::<u16>().map_err(|error| {
                format!("android uiauto: invalid RSCLAW_ANDROID_UIAUTO_PORT: {error}")
            })?,
            Err(_) => DEFAULT_UIAUTO_PORT,
        };
        Self::new(cls_bin, node, port)
    }

    fn new(cls_bin: String, node: String, port: u16) -> Result<Self, String> {
        validate_plain_value("RSCLAW_ANDROID_NODE", &node, 256)?;
        validate_plain_value("RSCLAW_CLS_BIN", &cls_bin, 4096)?;
        if node.starts_with('-') {
            return Err(
                "android uiauto: RSCLAW_ANDROID_NODE cannot look like an option".to_string(),
            );
        }
        if port == 0 {
            return Err("android uiauto: port must be greater than zero".to_string());
        }
        Ok(Self {
            cls_bin,
            node,
            port,
        })
    }
}

fn build_call_args(config: &Config, command: &str, args_json: &str) -> Result<Vec<String>, String> {
    let specs = command_specs(command)
        .ok_or_else(|| format!("android uiauto: command `{command}` is not allowed"))?;
    let value: Value = serde_json::from_str(args_json)
        .map_err(|error| format!("android uiauto: invalid args JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "android uiauto: args JSON must be an object".to_string())?;
    validate_object_keys(command, object, specs)?;
    validate_command_requirements(command, object, specs)?;

    let mut args = agent_base_args(config, command);
    for spec in specs {
        let Some(value) = object.get(spec.key) else {
            continue;
        };
        append_option(&mut args, *spec, value)?;
    }
    Ok(args)
}

fn agent_base_args(config: &Config, command: &str) -> Vec<String> {
    vec![
        "android".to_string(),
        command.to_string(),
        "-n".to_string(),
        config.node.clone(),
    ]
}

fn uiauto_base_args(config: &Config, command: &str) -> Vec<String> {
    vec![
        "android".to_string(),
        "uiauto".to_string(),
        command.to_string(),
        "-n".to_string(),
        config.node.clone(),
        "--port".to_string(),
        config.port.to_string(),
    ]
}

fn command_specs(command: &str) -> Option<&'static [ArgSpec]> {
    match command {
        "status" | "screenshot" => Some(NO_ARGS),
        "key" => Some(KEY_ARGS),
        "tap" => Some(TAP_ARGS),
        "swipe" => Some(SWIPE_ARGS),
        "launch" => Some(LAUNCH_ARGS),
        _ => None,
    }
}

fn deadline_for(command: &str) -> Duration {
    match command {
        "screenshot" => Duration::from_secs(45),
        _ => Duration::from_secs(20),
    }
}

fn validate_object_keys(
    command: &str,
    object: &Map<String, Value>,
    specs: &[ArgSpec],
) -> Result<(), String> {
    for key in object.keys() {
        if !specs.iter().any(|spec| spec.key == key) {
            return Err(format!(
                "android uiauto: unknown `{command}` argument `{key}`"
            ));
        }
    }
    Ok(())
}

fn validate_command_requirements(
    command: &str,
    object: &Map<String, Value>,
    specs: &[ArgSpec],
) -> Result<(), String> {
    for spec in specs.iter().filter(|spec| spec.required) {
        if !object.contains_key(spec.key) {
            return Err(format!(
                "android uiauto: `{command}` requires `{}`",
                spec.key
            ));
        }
    }
    Ok(())
}

fn append_option(args: &mut Vec<String>, spec: ArgSpec, value: &Value) -> Result<(), String> {
    match spec.kind {
        ArgKind::Integer => {
            let number = value
                .as_i64()
                .ok_or_else(|| type_error(spec.key, "integer"))?;
            let number = i32::try_from(number)
                .map_err(|_| format!("android uiauto: `{}` is outside i32 range", spec.key))?;
            args.push(spec.flag.to_string());
            args.push(number.to_string());
        }
        ArgKind::Unsigned => {
            let number = value
                .as_u64()
                .ok_or_else(|| type_error(spec.key, "unsigned integer"))?;
            if number > 600_000 {
                return Err(format!(
                    "android uiauto: `{}` exceeds the maximum 600000",
                    spec.key
                ));
            }
            args.push(spec.flag.to_string());
            args.push(number.to_string());
        }
        ArgKind::String => {
            let string = value
                .as_str()
                .ok_or_else(|| type_error(spec.key, "string"))?;
            validate_plain_value(spec.key, string, MAX_STRING_ARG_BYTES)?;
            args.push(spec.flag.to_string());
            args.push(string.to_string());
        }
    }
    Ok(())
}

fn type_error(key: &str, expected: &str) -> String {
    format!("android uiauto: `{key}` must be a {expected}")
}

fn validate_plain_value(name: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("android uiauto: `{name}` cannot be empty"));
    }
    if value.len() > max_bytes {
        return Err(format!(
            "android uiauto: `{name}` exceeds {max_bytes} bytes"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(format!(
            "android uiauto: `{name}` cannot contain control characters"
        ));
    }
    Ok(())
}

fn validate_raw_request(
    method: &str,
    path: &str,
    json_body: Option<&str>,
) -> Result<String, String> {
    let method = method.trim().to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "POST") {
        return Err("android uiauto raw: method must be GET or POST".to_string());
    }
    if path.len() > MAX_PATH_BYTES
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains(['?', '#', '\\'])
        || path.chars().any(char::is_control)
    {
        return Err("android uiauto raw: unsafe endpoint path".to_string());
    }
    validate_raw_endpoint(&method, path)?;
    let body = match json_body {
        Some(body) => {
            if body.len() > MAX_RAW_BODY_BYTES {
                return Err(format!(
                    "android uiauto raw: JSON body exceeds {MAX_RAW_BODY_BYTES} bytes"
                ));
            }
            Some(
                serde_json::from_str::<Value>(body)
                    .map_err(|error| format!("android uiauto raw: invalid JSON body: {error}"))?,
            )
        }
        None => None,
    };
    validate_raw_body(&method, path, body.as_ref())?;
    Ok(method)
}

fn validate_raw_body(method: &str, path: &str, body: Option<&Value>) -> Result<(), String> {
    match (method, body) {
        ("GET", None) => return Ok(()),
        ("GET", Some(_)) => return Err("android uiauto raw: GET cannot have a body".to_string()),
        ("POST", None) => return Err("android uiauto raw: POST requires a JSON body".to_string()),
        ("POST", Some(_)) => {}
        _ => return Err("android uiauto raw: unsupported method/body combination".to_string()),
    }
    let body = body.ok_or_else(|| "android uiauto raw: POST requires a JSON body".to_string())?;
    if path == "/session" {
        return validate_session_capabilities(body);
    }

    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    match segments.as_slice() {
        ["session", _, "element" | "elements"] => validate_element_locator(body),
        ["session", _, "actions"] => validate_touch_actions(body),
        ["session", _, "element", _, "click" | "clear"] => validate_empty_object(body),
        ["session", _, "element", _, "value"] => validate_element_value(body),
        ["session", _, "appium", "device", "press_keycode"] => validate_keycode(body),
        ["session", _, "appium", "device", "set_clipboard"] => validate_clipboard(body),
        [
            "session",
            _,
            "appium",
            "device",
            "activate_app" | "terminate_app",
        ] => validate_app_id(body),
        ["session", _, "appium", "device", "start_activity"] => validate_start_activity(body),
        _ => Err(format!(
            "android uiauto raw: no body contract for `POST {path}`"
        )),
    }
}

fn validate_empty_object(body: &Value) -> Result<(), String> {
    match body.as_object() {
        Some(object) if object.is_empty() => Ok(()),
        _ => Err("android uiauto raw: endpoint requires an empty object body".to_string()),
    }
}

fn validate_element_locator(body: &Value) -> Result<(), String> {
    let object = object_with_keys(body, &["using", "value", "strategy", "selector"])?;
    let using = required_string(object, "using", 64)?;
    if !matches!(
        using,
        "id" | "xpath" | "class name" | "accessibility id" | "-android uiautomator"
    ) {
        return Err(format!(
            "android uiauto raw: locator strategy `{using}` is not allowed"
        ));
    }
    let value = required_string(object, "value", MAX_SELECTOR_BYTES)?;
    if value.is_empty() {
        return Err("android uiauto raw: locator value cannot be empty".to_string());
    }
    if let Some(strategy) = optional_string(object, "strategy", 64)?
        && strategy != using
    {
        return Err("android uiauto raw: strategy must equal using".to_string());
    }
    if let Some(selector) = optional_string(object, "selector", MAX_SELECTOR_BYTES)?
        && selector != value
    {
        return Err("android uiauto raw: selector must equal value".to_string());
    }
    Ok(())
}

fn validate_element_value(body: &Value) -> Result<(), String> {
    let object = object_with_keys(body, &["text", "value"])?;
    if object.len() != 2 {
        return Err("android uiauto raw: value body requires text and value".to_string());
    }
    let text = required_string(object, "text", MAX_INPUT_TEXT_BYTES)?;
    let characters = object
        .get("value")
        .and_then(Value::as_array)
        .ok_or_else(|| "android uiauto raw: value must be a character array".to_string())?;
    if characters.len() > MAX_INPUT_TEXT_BYTES {
        return Err("android uiauto raw: value character array is too large".to_string());
    }
    let mut reconstructed = String::with_capacity(text.len());
    for character in characters {
        let character = character
            .as_str()
            .filter(|value| value.chars().count() == 1)
            .ok_or_else(|| {
                "android uiauto raw: each value entry must contain one character".to_string()
            })?;
        reconstructed.push_str(character);
    }
    if reconstructed != text {
        return Err("android uiauto raw: text and value must encode the same input".to_string());
    }
    Ok(())
}

fn validate_touch_actions(body: &Value) -> Result<(), String> {
    let object = object_with_keys(body, &["actions"])?;
    if object.len() != 1 {
        return Err("android uiauto raw: actions body may contain only actions".to_string());
    }
    let sources = object
        .get("actions")
        .and_then(Value::as_array)
        .filter(|sources| !sources.is_empty() && sources.len() <= 4)
        .ok_or_else(|| "android uiauto raw: actions requires 1-4 input sources".to_string())?;
    for source in sources {
        let source = object_with_keys(source, &["type", "id", "parameters", "actions"])?;
        if source.len() != 4 || required_string(source, "type", 16)? != "pointer" {
            return Err("android uiauto raw: only pointer action sources are allowed".to_string());
        }
        required_string(source, "id", 64)?;
        let parameters = source
            .get("parameters")
            .ok_or_else(|| "android uiauto raw: pointer parameters are required".to_string())?;
        let parameters = object_with_keys(parameters, &["pointerType"])?;
        if parameters.len() != 1 || required_string(parameters, "pointerType", 16)? != "touch" {
            return Err("android uiauto raw: pointerType must be touch".to_string());
        }
        let actions = source
            .get("actions")
            .and_then(Value::as_array)
            .filter(|actions| !actions.is_empty() && actions.len() <= 32)
            .ok_or_else(|| {
                "android uiauto raw: pointer source requires 1-32 actions".to_string()
            })?;
        for action in actions {
            validate_touch_action(action)?;
        }
    }
    Ok(())
}

fn validate_touch_action(action: &Value) -> Result<(), String> {
    let object = action
        .as_object()
        .ok_or_else(|| "android uiauto raw: action must be an object".to_string())?;
    let action_type = required_string(object, "type", 32)?;
    match action_type {
        "pointerMove" => {
            require_exact_keys(object, &["type", "duration", "origin", "x", "y"])?;
            if required_string(object, "origin", 16)? != "viewport" {
                return Err("android uiauto raw: pointerMove origin must be viewport".to_string());
            }
            bounded_u64(object, "duration", 60_000)?;
            i32_value(object, "x")?;
            i32_value(object, "y")?;
        }
        "pointerDown" | "pointerUp" => {
            require_exact_keys(object, &["type", "button"])?;
            if bounded_u64(object, "button", 0)? != 0 {
                return Err("android uiauto raw: touch pointer button must be zero".to_string());
            }
        }
        "pause" => {
            require_exact_keys(object, &["type", "duration"])?;
            bounded_u64(object, "duration", 60_000)?;
        }
        _ => {
            return Err(format!(
                "android uiauto raw: action `{action_type}` is not allowed"
            ));
        }
    }
    Ok(())
}

fn validate_keycode(body: &Value) -> Result<(), String> {
    let object = object_with_keys(body, &["keycode"])?;
    require_exact_keys(object, &["keycode"])?;
    bounded_u64(object, "keycode", 1_000)?;
    Ok(())
}

fn validate_clipboard(body: &Value) -> Result<(), String> {
    let object = object_with_keys(body, &["content", "contentType", "label"])?;
    require_exact_keys(object, &["content", "contentType", "label"])?;
    if required_string(object, "contentType", 16)? != "plaintext" {
        return Err("android uiauto raw: clipboard contentType must be plaintext".to_string());
    }
    let label = required_string(object, "label", 64)?;
    if label != "rsclaw" {
        return Err("android uiauto raw: clipboard label must be rsclaw".to_string());
    }
    let content = required_string(object, "content", MAX_INPUT_TEXT_BYTES * 2)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(content)
        .map_err(|_| "android uiauto raw: clipboard content must be standard base64".to_string())?;
    if decoded.is_empty() || decoded.len() > MAX_INPUT_TEXT_BYTES {
        return Err(format!(
            "android uiauto raw: decoded clipboard content must be 1-{MAX_INPUT_TEXT_BYTES} bytes"
        ));
    }
    std::str::from_utf8(&decoded)
        .map_err(|_| "android uiauto raw: clipboard content must decode as UTF-8".to_string())?;
    Ok(())
}

fn validate_app_id(body: &Value) -> Result<(), String> {
    let object = object_with_keys(body, &["appId"])?;
    require_exact_keys(object, &["appId"])?;
    validate_android_component(required_string(object, "appId", 256)?, "appId")
}

fn validate_start_activity(body: &Value) -> Result<(), String> {
    let object = object_with_keys(body, &["appPackage", "appActivity"])?;
    require_exact_keys(object, &["appPackage", "appActivity"])?;
    validate_android_component(required_string(object, "appPackage", 256)?, "appPackage")?;
    validate_android_component(required_string(object, "appActivity", 256)?, "appActivity")
}

fn validate_android_component(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'$'))
    {
        return Err(format!(
            "android uiauto raw: `{field}` is not a safe Android component"
        ));
    }
    Ok(())
}

fn object_with_keys<'a>(
    value: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "android uiauto raw: body must be an object".to_string())?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!(
            "android uiauto raw: body field `{key}` is not allowed"
        ));
    }
    Ok(object)
}

fn require_exact_keys(object: &Map<String, Value>, expected: &[&str]) -> Result<(), String> {
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err(format!(
            "android uiauto raw: body requires exactly {}",
            expected.join(", ")
        ));
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    max_bytes: usize,
) -> Result<&'a str, String> {
    optional_string(object, key, max_bytes)?
        .ok_or_else(|| format!("android uiauto raw: `{key}` is required"))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    max_bytes: usize,
) -> Result<Option<&'a str>, String> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| format!("android uiauto raw: `{key}` must be a string"))?;
    if value.len() > max_bytes || value.contains('\0') {
        return Err(format!(
            "android uiauto raw: `{key}` is empty, oversized, or contains NUL"
        ));
    }
    Ok(Some(value))
}

fn bounded_u64(object: &Map<String, Value>, key: &str, max: u64) -> Result<u64, String> {
    let value = object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("android uiauto raw: `{key}` must be an unsigned integer"))?;
    if value > max {
        return Err(format!("android uiauto raw: `{key}` exceeds {max}"));
    }
    Ok(value)
}

fn i32_value(object: &Map<String, Value>, key: &str) -> Result<i32, String> {
    let value = object
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("android uiauto raw: `{key}` must be an integer"))?;
    i32::try_from(value).map_err(|_| format!("android uiauto raw: `{key}` is outside i32 range"))
}

fn validate_session_capabilities(body: &Value) -> Result<(), String> {
    let body = body
        .as_object()
        .filter(|object| object.len() == 1 && object.contains_key("capabilities"))
        .ok_or_else(|| {
            "android uiauto raw: session body may contain only `capabilities`".to_string()
        })?;
    let capabilities = body
        .get("capabilities")
        .and_then(Value::as_object)
        .filter(|object| object.len() == 1 && object.contains_key("alwaysMatch"))
        .ok_or_else(|| {
            "android uiauto raw: capabilities may contain only `alwaysMatch`".to_string()
        })?;
    let always_match = capabilities
        .get("alwaysMatch")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            "android uiauto raw: session body requires capabilities.alwaysMatch".to_string()
        })?;
    let allowed_keys = [
        "platformName",
        "appium:automationName",
        "appium:noReset",
        "appium:newCommandTimeout",
    ];
    if let Some(key) = always_match
        .keys()
        .find(|key| !allowed_keys.contains(&key.as_str()))
    {
        return Err(format!(
            "android uiauto raw: session capability `{key}` is not allowed"
        ));
    }
    if always_match.get("platformName").and_then(Value::as_str) != Some("Android") {
        return Err("android uiauto raw: platformName must be Android".to_string());
    }
    if always_match
        .get("appium:automationName")
        .and_then(Value::as_str)
        != Some("UiAutomator2")
    {
        return Err("android uiauto raw: automationName must be UiAutomator2".to_string());
    }
    if let Some(no_reset) = always_match.get("appium:noReset")
        && !no_reset.is_boolean()
    {
        return Err("android uiauto raw: noReset must be boolean".to_string());
    }
    if let Some(timeout) = always_match.get("appium:newCommandTimeout") {
        let timeout = timeout.as_u64().ok_or_else(|| {
            "android uiauto raw: newCommandTimeout must be an unsigned integer".to_string()
        })?;
        if timeout > 3600 {
            return Err("android uiauto raw: newCommandTimeout exceeds 3600 seconds".to_string());
        }
    }
    Ok(())
}

fn validate_raw_endpoint(method: &str, path: &str) -> Result<(), String> {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let allowed = match (method, segments.as_slice()) {
        ("GET", ["status" | "sessions"]) | ("POST", ["session"]) => true,
        (_, ["session", session_id, rest @ ..]) if is_identifier(session_id) => {
            session_endpoint_allowed(method, rest)
        }
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "android uiauto raw: endpoint `{method} {path}` is not allowed"
        ))
    }
}

fn session_endpoint_allowed(method: &str, segments: &[&str]) -> bool {
    match (method, segments) {
        ("GET", ["source" | "screenshot"])
        | ("GET", ["window", "current", "size"])
        | ("POST", ["element" | "elements" | "actions"])
        | (
            "POST",
            [
                "appium",
                "device",
                "press_keycode" | "set_clipboard" | "activate_app" | "terminate_app"
                | "start_activity",
            ],
        ) => true,
        (method, ["element", element_id, operation]) if is_identifier(element_id) => {
            matches!(
                (method, *operation),
                ("GET", "text" | "rect") | ("POST", "click" | "clear" | "value")
            )
        }
        ("GET", ["element", element_id, "attribute", attribute]) => {
            is_identifier(element_id) && is_safe_attribute(attribute)
        }
        _ => false,
    }
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn is_safe_attribute(value: &str) -> bool {
    matches!(
        value,
        "resourceId"
            | "className"
            | "contentDescription"
            | "text"
            | "enabled"
            | "clickable"
            | "focused"
            | "selected"
            | "bounds"
    )
}

async fn run_cls(cls_bin: &str, args: &[String], timeout: Duration) -> Result<String, String> {
    let mut command = tokio::process::Command::new(cls_bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.as_std_mut().creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("android uiauto: failed to spawn `{cls_bin}`: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "android uiauto: failed to capture cls stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "android uiauto: failed to capture cls stderr".to_string())?;
    let execution = async {
        tokio::try_join!(
            read_stdout(stdout),
            read_truncated(stderr, MAX_ERROR_BYTES),
            async {
                child
                    .wait()
                    .await
                    .map_err(|error| format!("android uiauto: wait for cls: {error}"))
            }
        )
    };
    let (stdout, stderr, status) = tokio::time::timeout(timeout, execution)
        .await
        .map_err(|_| format!("android uiauto: cls timed out after {}s", timeout.as_secs()))??;
    if !status.success() {
        let stderr = sanitized_error_text(&stderr);
        let stderr = summarize_cls_error(&stderr);
        return Err(format!(
            "android uiauto: cls exited with {}: {stderr}",
            status
        ));
    }
    let value: Value = serde_json::from_slice(&stdout)
        .map_err(|error| format!("android uiauto: cls returned invalid JSON: {error}"))?;
    serde_json::to_string(&value)
        .map_err(|error| format!("android uiauto: response serialization failed: {error}"))
}

fn summarize_cls_error(error: &str) -> &str {
    if error.contains("Failed to capture a screenshot")
        || error.contains("secure' flag")
        || error.contains("secure flag")
    {
        "device screenshot unavailable because the current window is locked or secure"
    } else if error.contains("root AccessibilityNodeInfo") {
        "accessibility root unavailable for the current window"
    } else if error.contains("UnknownCommandException") || error.contains("unknown command") {
        "requested UIAutomator2 endpoint is unavailable on this server build"
    } else {
        error
    }
}

fn sanitized_error_text(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let sanitized = stderr
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    rsclaw_util::truncate_str(sanitized.trim(), MAX_ERROR_BYTES).to_string()
}

async fn read_stdout<R>(mut reader: R) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    read_bounded(&mut reader, MAX_RESPONSE_BYTES).await
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("android uiauto: read cls stdout: {error}"))?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > limit {
            return Err(format!("android uiauto: response exceeds {limit} bytes"));
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

async fn read_truncated<R>(mut reader: R, limit: usize) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("android uiauto: read cls stderr: {error}"))?;
        if count == 0 {
            return Ok(output);
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            cls_bin: "cls".to_string(),
            node: "android-dev".to_string(),
            port: 6790,
        }
    }

    #[test]
    fn builds_typed_agent_args_without_a_shell() {
        let args = build_call_args(&config(), "tap", r#"{"x":20,"y":40}"#).unwrap();
        assert_eq!(
            args,
            [
                "android",
                "tap",
                "-n",
                "android-dev",
                "--x",
                "20",
                "--y",
                "40",
            ]
        );
    }

    #[test]
    fn rejects_unknown_commands_and_options() {
        assert!(build_call_args(&config(), "stop", "{}").is_err());
        assert!(
            build_call_args(&config(), "app-state", r#"{"package":"com.tencent.mm"}"#).is_err()
        );
        assert!(build_call_args(&config(), "screenshot", r#"{"output":"/tmp/x"}"#).is_err());
    }

    #[test]
    fn validates_required_and_exclusive_args() {
        assert!(build_call_args(&config(), "tap", r#"{"x":1}"#).is_err());
        assert!(build_call_args(&config(), "key", "{}").is_err());
        assert!(build_call_args(&config(), "key", r#"{"key":"BACK","keycode":4}"#).is_err());
        assert!(build_call_args(&config(), "key", r#"{"key":"back"}"#).is_ok());
    }

    #[test]
    fn rejects_untyped_and_out_of_range_values() {
        assert!(build_call_args(&config(), "tap", r#"{"x":"1","y":2}"#).is_err());
        assert!(
            build_call_args(
                &config(),
                "swipe",
                r#"{"x1":0,"y1":0,"x2":1,"y2":1,"durationMs":600001}"#
            )
            .is_err()
        );
        assert!(build_call_args(&config(), "tap", "[]").is_err());
    }

    #[test]
    fn raw_request_accepts_native_webdriver_paths() {
        assert_eq!(
            validate_raw_request(
                "post",
                "/session/abc/element",
                Some(r#"{"using":"xpath","value":"//*[@focused='true']"}"#),
            )
            .as_deref(),
            Ok("POST")
        );
        assert!(validate_raw_request("GET", "/session/abc/window/current/size", None).is_ok());
        assert!(validate_raw_request("GET", "/session/abc/window/rect", None).is_err());
        assert!(
            validate_raw_request("GET", "/session/abc/appium/device/current_package", None)
                .is_err()
        );
    }

    #[test]
    fn raw_request_validates_element_and_text_body_contracts() {
        let locator = r#"{"using":"xpath","value":"//*[@focused='true']","strategy":"xpath","selector":"//*[@focused='true']"}"#;
        assert!(validate_raw_request("POST", "/session/abc/element", Some(locator)).is_ok());
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/element",
                Some(r#"{"using":"xpath","value":"//*","extra":true}"#),
            )
            .is_err()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/app_state",
                Some(r#"{"appId":"com.tencent.mm"}"#),
            )
            .is_err()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/element/id/value",
                Some(r#"{"text":"你好","value":["你","好"]}"#),
            )
            .is_ok()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/element/id/value",
                Some(r#"{"text":"你好","value":["你"]}"#),
            )
            .is_err()
        );
        assert!(validate_raw_request("POST", "/session/abc/element/id/click", Some("{}")).is_ok());
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/element/id/click",
                Some(r#"{"script":"x"}"#),
            )
            .is_err()
        );
    }

    #[test]
    fn raw_request_allows_only_bounded_touch_actions() {
        let touch = r#"{"actions":[{"type":"pointer","id":"finger1","parameters":{"pointerType":"touch"},"actions":[{"type":"pointerMove","duration":0,"origin":"viewport","x":10,"y":20},{"type":"pointerDown","button":0},{"type":"pointerUp","button":0}]}]}"#;
        assert!(validate_raw_request("POST", "/session/abc/actions", Some(touch)).is_ok());

        let key_input = r#"{"actions":[{"type":"key","id":"keyboard","actions":[{"type":"keyDown","value":"a"}]}]}"#;
        assert!(validate_raw_request("POST", "/session/abc/actions", Some(key_input)).is_err());
        let long_pause = r#"{"actions":[{"type":"pointer","id":"finger1","parameters":{"pointerType":"touch"},"actions":[{"type":"pause","duration":60001}]}]}"#;
        assert!(validate_raw_request("POST", "/session/abc/actions", Some(long_pause)).is_err());
    }

    #[test]
    fn raw_request_validates_device_operation_bodies() {
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/set_clipboard",
                Some(r#"{"content":"5L2g5aW9","contentType":"plaintext","label":"rsclaw"}"#),
            )
            .is_ok()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/set_clipboard",
                Some(r#"{"content":"not-base64","contentType":"plaintext","label":"rsclaw"}"#),
            )
            .is_err()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/set_clipboard",
                Some(r#"{"content":"5L2g5aW9","contentType":"image","label":"rsclaw"}"#),
            )
            .is_err()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/press_keycode",
                Some(r#"{"keycode":279}"#),
            )
            .is_ok()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/activate_app",
                Some(r#"{"appId":"com.tencent.mm"}"#),
            )
            .is_ok()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/start_activity",
                Some(r#"{"appPackage":"com.tencent.mm","appActivity":".ui.LauncherUI"}"#),
            )
            .is_ok()
        );
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/activate_app",
                Some(r#"{"appId":"--unsafe"}"#),
            )
            .is_err()
        );
    }

    #[test]
    fn raw_session_creation_allows_only_bounded_uiautomator2_capabilities() {
        let valid = r#"{"capabilities":{"alwaysMatch":{"platformName":"Android","appium:automationName":"UiAutomator2","appium:noReset":true,"appium:newCommandTimeout":300}}}"#;
        assert!(validate_raw_request("POST", "/session", Some(valid)).is_ok());

        let executable = r#"{"capabilities":{"alwaysMatch":{"platformName":"Android","appium:automationName":"UiAutomator2","appium:chromedriverExecutable":"/tmp/x"}}}"#;
        assert!(validate_raw_request("POST", "/session", Some(executable)).is_err());

        let first_match = r#"{"capabilities":{"alwaysMatch":{"platformName":"Android","appium:automationName":"UiAutomator2"},"firstMatch":[{"appium:chromedriverExecutable":"/tmp/x"}]}}"#;
        assert!(validate_raw_request("POST", "/session", Some(first_match)).is_err());

        let legacy = r#"{"capabilities":{"alwaysMatch":{"platformName":"Android","appium:automationName":"UiAutomator2"}},"desiredCapabilities":{"appium:chromedriverExecutable":"/tmp/x"}}"#;
        assert!(validate_raw_request("POST", "/session", Some(legacy)).is_err());

        assert!(validate_raw_request("GET", "/sessions", Some("{}")).is_err());
        assert!(validate_raw_request("POST", "/session/abc/actions", None).is_err());
    }

    #[tokio::test]
    async fn stream_readers_enforce_output_bounds_and_drain_truncated_errors() {
        assert_eq!(
            read_bounded(&b"1234"[..], 4).await.as_deref(),
            Ok(&b"1234"[..])
        );
        assert!(read_bounded(&b"12345"[..], 4).await.is_err());
        assert_eq!(
            read_truncated(&b"abcdef"[..], 3).await.as_deref(),
            Ok(&b"abc"[..])
        );
        assert_eq!(
            sanitized_error_text(b"bad\n\x1b[31merror"),
            "bad  [31merror"
        );
        assert_eq!(
            summarize_cls_error(
                "Failed to capture a screenshot. Does the current view have 'secure' flag set?"
            ),
            "device screenshot unavailable because the current window is locked or secure"
        );
        assert_eq!(
            summarize_cls_error("Timed out waiting for the root AccessibilityNodeInfo"),
            "accessibility root unavailable for the current window"
        );
    }

    #[test]
    fn raw_request_rejects_tunnel_escape_and_invalid_json() {
        assert!(validate_raw_request("PUT", "/status", None).is_err());
        assert!(validate_raw_request("DELETE", "/session/abc", None).is_err());
        assert!(validate_raw_request("GET", "//example.com/status", None).is_err());
        assert!(validate_raw_request("GET", "/session/../status", None).is_err());
        assert!(validate_raw_request("POST", "/session", Some("not-json")).is_err());
    }

    #[test]
    fn raw_request_blocks_execution_and_shell_endpoints() {
        assert!(validate_raw_request("POST", "/session/abc/execute/sync", Some("{}"),).is_err());
        assert!(
            validate_raw_request(
                "POST",
                "/session/abc/appium/device/execute_shell",
                Some("{}"),
            )
            .is_err()
        );
        assert!(validate_raw_request("GET", "/session/abc/log/types", None).is_err());
    }

    #[test]
    fn config_validation_fails_closed() {
        assert!(Config::new("cls".to_string(), String::new(), 6790).is_err());
        assert!(Config::new("cls".to_string(), "android-dev".to_string(), 0).is_err());
        assert_eq!(
            Config::new("cls".to_string(), "android-dev".to_string(), 6790)
                .expect("valid configuration"),
            config()
        );
    }

    #[test]
    fn plain_values_reject_controls_but_allow_cjk() {
        assert!(validate_plain_value("node", "android-dev", 256).is_ok());
        assert!(validate_plain_value("activity", "微信主页", 256).is_ok());
        assert!(validate_plain_value("node", "android\ndev", 256).is_err());
    }
}
