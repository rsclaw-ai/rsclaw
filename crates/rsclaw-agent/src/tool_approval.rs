//! `approvals.tools`: which tool calls need a person's go-ahead, and how a
//! call is shown to that person.
//!
//! The gate itself sits in tool dispatch; this module holds the pure parts
//! (name matching, exemptions, the summary line) so they can be tested
//! without a runtime.

use rsclaw_config::schema::ToolApprovalConfig;
use serde_json::Value;

/// A call's tool as config patterns name it: `plugin.tool` for plugin tools
/// (wire form `plugin__tool`, or `plugin_invoke` with `plugin` / `tool`
/// arguments), the bare name otherwise. Also returns the arguments the tool
/// itself receives.
pub fn call_identity<'a>(name: &str, args: &'a Value) -> (String, &'a Value) {
    if matches!(name, "plugin_invoke" | "plugin.invoke") {
        let plugin = args["plugin"].as_str().unwrap_or("").trim();
        let tool = args["tool"].as_str().unwrap_or("").trim();
        if !plugin.is_empty() && !tool.is_empty() {
            let inner = args.get("arguments").unwrap_or(&Value::Null);
            return (format!("{plugin}.{tool}"), inner);
        }
    }
    (normalize(name), args)
}

fn normalize(name: &str) -> String {
    name.trim().replacen("__", ".", 1)
}

/// `*`-glob match over the whole string.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// Whether a call to `tool` (see [`call_identity`]) in this session needs
/// approval under `cfg`.
pub fn needs_approval(
    cfg: Option<&ToolApprovalConfig>,
    tool: &str,
    channel: &str,
    session_key: &str,
) -> bool {
    let Some(cfg) = cfg else { return false };
    if !cfg.enabled.unwrap_or(true) {
        return false;
    }
    let gated = cfg
        .patterns
        .iter()
        .flatten()
        .any(|p| glob(&normalize(p), tool));
    if !gated {
        return false;
    }
    let cron = channel == "cron" || session_key.starts_with("cron:");
    if cron && cfg.exempt_cron.unwrap_or(true) {
        return false;
    }
    !cfg
        .exempt_sessions
        .iter()
        .flatten()
        .any(|p| glob(p.trim(), session_key))
}

/// Absolute local paths in file-ish arguments (`filePath`, `imagePaths`,
/// `video`, `cover`, …) that don't exist. Checked before asking anyone, so
/// nobody approves a call that can only fail.
pub fn missing_files(args: &Value) -> Vec<String> {
    let mut missing = Vec::new();
    collect_missing(args, false, &mut missing);
    missing
}

fn collect_missing(v: &Value, fileish: bool, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, v) in map {
                let k = k.to_ascii_lowercase();
                let fileish = ["path", "file", "video", "image", "cover", "audio", "music"]
                    .iter()
                    .any(|w| k.contains(w));
                collect_missing(v, fileish, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_missing(i, fileish, out)),
        Value::String(s) if fileish && is_absolute_local(s) => {
            if !std::path::Path::new(s.trim()).exists() {
                out.push(s.trim().to_owned());
            }
        }
        _ => {}
    }
}

/// `/x/…` or `C:\…` / `C:/…`; URLs and relative names don't count.
fn is_absolute_local(s: &str) -> bool {
    let s = s.trim();
    let b = s.as_bytes();
    (s.starts_with('/') && !s.starts_with("//"))
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
}

/// Arguments worth showing the approver, most telling first.
const SHOWN_KEYS: &[&str] = &[
    "title", "name", "desc", "description", "content", "text", "caption", "tags", "topic",
    "video", "video_path", "image", "images", "image_path", "file", "files", "path", "cover",
    "id", "video_id", "note_id", "item_id", "aweme_id", "url", "schedule", "publish_time",
];

/// One line per telling argument (`title: …`), each cut short; the whole
/// call when none of the usual keys is present.
pub fn summary(args: &Value) -> String {
    const MAX: usize = 120;
    let show = |v: &Value| -> String {
        let s = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let s = s.replace('\n', " ");
        if s.chars().count() > MAX {
            format!("{}…", s.chars().take(MAX).collect::<String>())
        } else {
            s
        }
    };
    let lines: Vec<String> = SHOWN_KEYS
        .iter()
        .filter_map(|k| {
            let v = args.get(*k)?;
            (!v.is_null() && v != &Value::String(String::new())).then(|| format!("{k}: {}", show(v)))
        })
        .collect();
    if lines.is_empty() {
        show(args)
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(patterns: &[&str]) -> ToolApprovalConfig {
        ToolApprovalConfig {
            enabled: Some(true),
            patterns: Some(patterns.iter().map(|s| s.to_string()).collect()),
            exempt_cron: None,
            exempt_sessions: None,
        }
    }

    #[test]
    fn plugin_calls_are_named_plugin_dot_tool() {
        let args = json!({"title": "t"});
        assert_eq!(call_identity("douyin__publish", &args).0, "douyin.publish");
        assert_eq!(call_identity("douyin.publish", &args).0, "douyin.publish");
        let inv = json!({"plugin": "xhs", "tool": "delete_note", "arguments": {"note_id": "1"}});
        let (id, inner) = call_identity("plugin_invoke", &inv);
        assert_eq!(id, "xhs.delete_note");
        assert_eq!(inner["note_id"], "1");
        assert_eq!(call_identity("exec", &args).0, "exec");
    }

    #[test]
    fn patterns_match_either_separator_and_wildcards() {
        let c = cfg(&["douyin.publish", "kuaishou__publish", "*.delete_video"]);
        assert!(needs_approval(Some(&c), "douyin.publish", "desktop", "desktop:main:1"));
        assert!(needs_approval(Some(&c), "kuaishou.publish", "wechat", "s"));
        assert!(needs_approval(Some(&c), "douyin.delete_video", "wechat", "s"));
        assert!(!needs_approval(Some(&c), "douyin.publish_draft", "wechat", "s"));
        assert!(!needs_approval(Some(&c), "douyin.list_videos", "wechat", "s"));
        assert!(!needs_approval(None, "douyin.publish", "wechat", "s"));
    }

    #[test]
    fn cron_and_listed_sessions_are_exempt() {
        let mut c = cfg(&["douyin.publish"]);
        assert!(!needs_approval(Some(&c), "douyin.publish", "cron", "agent:main:custom"));
        assert!(!needs_approval(Some(&c), "douyin.publish", "wechat", "cron:job1"));
        c.exempt_cron = Some(false);
        assert!(needs_approval(Some(&c), "douyin.publish", "cron", "agent:main:custom"));
        c.exempt_sessions = Some(vec!["agent:ops:*".to_owned()]);
        assert!(!needs_approval(Some(&c), "douyin.publish", "wechat", "agent:ops:wechat:u1"));
        c.enabled = Some(false);
        assert!(!needs_approval(Some(&c), "douyin.publish", "wechat", "s"));
    }

    #[test]
    fn missing_local_files_are_found_before_asking() {
        let here = std::env::current_dir().unwrap().join("Cargo.toml");
        let here = here.to_string_lossy().to_string();
        let args = json!({
            "title": "/not/a/path/but/a/title",
            "filePath": "/no/such/dir/dl_v_1.mp4",
            "imagePaths": [here, r"Z:\no\cover.png"],
            "url": "https://example.com/a.mp4",
            "video": "dl_v_1.mp4",
        });
        assert_eq!(missing_files(&args), vec!["/no/such/dir/dl_v_1.mp4", r"Z:\no\cover.png"]);
        assert!(missing_files(&json!({"video": here})).is_empty());
    }

    #[test]
    fn summary_shows_the_telling_arguments() {
        let s = summary(&json!({"title": "云电脑宣传", "video": "C:/v/dl_v_1.mp4", "visibility": 0}));
        assert_eq!(s, "title: 云电脑宣传\nvideo: C:/v/dl_v_1.mp4");
        assert_eq!(summary(&json!({"x": 1})), r#"{"x":1}"#);
    }
}
