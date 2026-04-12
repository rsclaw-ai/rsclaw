//! Pre-parsed skill system -- local command shortcuts that bypass LLM.
//!
//! Commands starting with / or $ or matching known patterns are executed
//! locally without consuming LLM tokens.

use std::collections::HashMap;

use anyhow::Result;
use regex::Regex;
use serde_json::{Value, json};
use tracing::{info, warn};

/// Result of pre-parse matching.
pub enum PreParseResult {
    /// No match -- pass to LLM as normal.
    PassThrough,
    /// Direct text response (empty string = suppress reply entirely).
    DirectResponse(String),
    /// Execute a tool with these args, return result directly.
    ToolCall { tool: String, args: Value },
    /// Command is blocked by safety rules.
    Blocked(String),
    /// Command needs user confirmation before execution.
    NeedsConfirm { command: String, reason: String },
}

/// Pre-parse engine loaded from defaults.toml.
pub struct PreParseEngine {
    /// Command patterns: (regex, handler)
    commands: Vec<(Regex, CommandHandler)>,
    /// Exec deny patterns (compiled regexes)
    deny_patterns: Vec<Regex>,
    /// Exec confirm patterns
    confirm_patterns: Vec<Regex>,
    /// Exec allow patterns (override deny)
    allow_patterns: Vec<Regex>,
    /// Whether safety rules are active (from config tools.exec.safety)
    pub safety_enabled: bool,
}

enum CommandHandler {
    /// Execute a tool with args derived from captures
    Tool {
        tool: String,
        arg_template: ArgTemplate,
    },
    /// Return a direct response
    Direct(Box<dyn Fn(&regex::Captures<'_>) -> String + Send + Sync>),
    /// Execute a built-in function
    #[allow(dead_code)]
    BuiltIn(Box<dyn Fn(&regex::Captures<'_>) -> Result<String> + Send + Sync>),
}

enum ArgTemplate {
    /// Single argument from a capture group
    Single { param: String, group: usize },
    /// Named captures mapped to params
    Named(HashMap<String, String>),
    /// Static args
    Static(Value),
    /// Search query: parse optional trailing provider name
    SearchQuery,
}

impl PreParseEngine {
    /// Build from defaults.toml content.
    pub fn load_with_safety(safety_enabled: bool) -> Self {
        let mut commands = Vec::new();

        // --- Help ---
        add_direct(&mut commands, r"^(?i)(?:/help|/\?|/\s*$)", |_| {
            "__HELP__".into()
        });

        // --- Shell commands ---
        // /run <cmd>, /sh <cmd>, /exec <cmd> -- run via shell for proper shell behavior
        // Windows: use cmd /C, Unix: use sh -c
        let (shell_cmd, shell_args): (&str, Vec<&str>) = if cfg!(target_os = "windows") {
            ("cmd", vec!["/C"])
        } else {
            ("sh", vec!["-c"])
        };
        add_cmd(
            &mut commands,
            r"(?i)^[/!](?:run|sh|exec)\s+(.+)$",
            "exec",
            ArgTemplate::Static(json!({"cmd": shell_cmd, "args": shell_args})),
        );
        // $ <cmd>
        add_cmd(
            &mut commands,
            r"^\$\s+(.+)$",
            "exec",
            ArgTemplate::Static(json!({"cmd": shell_cmd, "args": shell_args})),
        );
        // ! <cmd>
        add_cmd(
            &mut commands,
            r"^!\s+(.+)$",
            "exec",
            ArgTemplate::Static(json!({"cmd": shell_cmd, "args": shell_args})),
        );

        // --- File operations ---
        add_cmd(
            &mut commands,
            r"(?i)^/(?:read|cat)\s+(.+)$",
            "read",
            ArgTemplate::Single {
                param: "path".into(),
                group: 1,
            },
        );
        // /ls [args] -- run ls with optional arguments, just like shell
        add_cmd(
            &mut commands,
            r"(?i)^/ls(?:\s+(.+))?$",
            "exec",
            ArgTemplate::Single {
                param: "cmd".into(),
                group: 0, // special: handled in build_tool_args
            },
        );
        add_cmd(
            &mut commands,
            r"(?i)^/write\s+(\S+)\s+(.+)$",
            "write",
            ArgTemplate::Named(HashMap::new()),
        ); // special handling

        // --- Search & web ---
        // /search <query> [provider] — provider is optional last word matching known engines
        add_cmd(
            &mut commands,
            r"(?i)^/search\s+(.+)$",
            "web_search",
            ArgTemplate::SearchQuery,
        );
        add_cmd(
            &mut commands,
            r"(?i)^/fetch\s+(.+)$",
            "web_fetch",
            ArgTemplate::Single {
                param: "url".into(),
                group: 1,
            },
        );
        add_cmd(
            &mut commands,
            r"(?i)^/screenshot\s+(.+)$",
            "web_browser",
            ArgTemplate::Static(json!({"action": "screenshot"})),
        );
        // Desktop screenshot (no URL)
        add_cmd(
            &mut commands,
            r"(?i)^/(?:ss|screenshot)$",
            "computer_use",
            ArgTemplate::Static(json!({"action": "screenshot"})),
        );
        // --- Skill management ---
        add_cmd(
            &mut commands,
            r"(?i)^/skill\s+install\s+(.+)$",
            "exec",
            ArgTemplate::Static(json!({})),
        );
        add_cmd(
            &mut commands,
            r"(?i)^/skill\s+list$",
            "exec",
            ArgTemplate::Static(json!({})),
        );
        add_cmd(
            &mut commands,
            r"(?i)^/skill\s+search\s+(.+)$",
            "exec",
            ArgTemplate::Static(json!({})),
        );

        // --- System ---
        add_direct(&mut commands, r"(?i)^/status$", |_| "__STATUS__".into());
        add_direct(&mut commands, r"(?i)^/health$", |_| "__HEALTH__".into());
        add_direct(&mut commands, r"(?i)^/version$", |_| {
            format!("rsclaw v{}", env!("RSCLAW_BUILD_VERSION"))
        });
        add_direct(&mut commands, r"(?i)^/uptime$", |_| "__UPTIME__".into());
        add_direct(&mut commands, r"(?i)^/models?$", |_| "__MODELS__".into());
        add_direct(&mut commands, r"(?i)^/model\s+(.+)$", |caps| {
            format!("__MODEL_SET__:{}", &caps[1])
        });

        // --- Session ---
        add_direct(&mut commands, r"(?i)^/abort$", |_| "__ABORT__".into());
        add_direct(&mut commands, r"(?i)^/clear$", |_| "__CLEAR__".into());
        add_direct(&mut commands, r"(?i)^/reset$", |_| "__RESET__".into());
        add_direct(&mut commands, r"(?i)^/history(?:\s+(\d+))?$", |caps| {
            let n = caps.get(1).map(|m| m.as_str()).unwrap_or("20");
            format!("__HISTORY__:{n}")
        });
        add_direct(&mut commands, r"(?i)^/sessions$", |_| "__SESSIONS__".into());

        // --- Memory ---
        add_cmd(
            &mut commands,
            r"(?i)^/remember\s+(.+)$",
            "memory_put",
            ArgTemplate::Single {
                param: "text".into(),
                group: 1,
            },
        );
        add_cmd(
            &mut commands,
            r"(?i)^/recall\s+(.+)$",
            "memory_search",
            ArgTemplate::Single {
                param: "query".into(),
                group: 1,
            },
        );

        // --- Upload & Token limits ---
        add_direct(&mut commands, r"(?i)^/get_upload_size$", |_| {
            "__GET_UPLOAD_SIZE__".into()
        });
        add_direct(&mut commands, r"(?i)^/set_upload_size\s+(\d+)$", |caps| {
            format!("__SET_UPLOAD_SIZE__:{}", &caps[1])
        });
        add_direct(&mut commands, r"(?i)^/get_upload_chars$", |_| {
            "__GET_UPLOAD_CHARS__".into()
        });
        add_direct(&mut commands, r"(?i)^/set_upload_chars\s+(\d+)$", |caps| {
            format!("__SET_UPLOAD_CHARS__:{}", &caps[1])
        });

        // Persistent config versions (write to config file)
        add_direct(
            &mut commands,
            r"(?i)^/config_upload_size\s+(\d+)$",
            |caps| format!("__CONFIG_UPLOAD_SIZE__:{}", &caps[1]),
        );
        add_direct(
            &mut commands,
            r"(?i)^/config_upload_chars\s+(\d+)$",
            |caps| format!("__CONFIG_UPLOAD_CHARS__:{}", &caps[1]),
        );

        // --- History ---
        add_direct(&mut commands, r"(?i)^/history(?:\s+(\d+))?$", |caps| {
            let n = caps.get(1).map(|m| m.as_str()).unwrap_or("20");
            format!("__HISTORY__:{n}")
        });

        // --- Cron ---
        add_direct(&mut commands, r"(?i)^/cron(?:\s+list)?$", |_| {
            "__CRON_LIST__".into()
        });

        // --- Message ---
        add_cmd(
            &mut commands,
            r"(?i)^/send\s+(\S+)\s+(.+)$",
            "message",
            ArgTemplate::Named(HashMap::new()),
        );

        // --- Background context (/ctx, formerly /btw) ---
        add_direct(&mut commands, r"(?i)^/ctx\s+--list$", |_| {
            "__CTX_LIST__".into()
        });
        add_direct(&mut commands, r"(?i)^/ctx\s+--clear$", |_| {
            "__CTX_CLEAR__".into()
        });
        add_direct(&mut commands, r"(?i)^/ctx\s+--remove\s+(\d+)$", |caps| {
            format!("__CTX_REMOVE__:{}", &caps[1])
        });
        add_direct(
            &mut commands,
            r"(?i)^/ctx\s+--ttl\s+(\d+)\s+(.+)$",
            |caps| format!("__CTX_TTL__:{}:{}", &caps[1], &caps[2]),
        );
        add_direct(&mut commands, r"(?i)^/ctx\s+--global\s+(.+)$", |caps| {
            format!("__CTX_GLOBAL__:{}", &caps[1])
        });
        // Must be last /ctx pattern to avoid matching flags
        add_direct(&mut commands, r"(?i)^/ctx\s+(.+)$", |caps| {
            format!("__CTX_ADD__:{}", &caps[1])
        });

        // Bare /ctx with no args -> show usage
        add_direct(&mut commands, r"(?i)^/ctx\s*$", |_| "__CTX_USAGE__".into());

        // --- Side-channel quick query (/btw) ---
        add_direct(&mut commands, r"(?i)^/btw\s+(.+)$", |caps| {
            format!("__SIDE_QUERY__:{}", &caps[1])
        });

        // --- Find & Grep --- wrap in shell for glob expansion
        // Windows: use dir /s /b and findstr, Unix: use find and grep
        let (find_cmd, find_args, grep_cmd, grep_args): (&str, Vec<&str>, &str, Vec<&str>) =
            if cfg!(target_os = "windows") {
                (
                    "cmd",
                    vec!["/C", "dir /s /b"],
                    "cmd",
                    vec!["/C", "findstr /s /i /n"],
                )
            } else {
                (
                    "sh",
                    vec!["-c", "find . -name "],
                    "sh",
                    vec!["-c", "grep -rn "],
                )
            };
        add_cmd(
            &mut commands,
            r"(?i)^/find\s+(.+)$",
            "exec",
            ArgTemplate::Static(json!({"cmd": find_cmd, "args": find_args})),
        );
        add_cmd(
            &mut commands,
            r"(?i)^/grep\s+(.+)$",
            "exec",
            ArgTemplate::Static(json!({"cmd": grep_cmd, "args": grep_args})),
        );

        // Load exec safety rules from defaults.toml
        let (deny, confirm, allow) = load_safety_rules();

        Self {
            commands,
            deny_patterns: deny,
            confirm_patterns: confirm,
            allow_patterns: allow,
            safety_enabled,
        }
    }

    /// Convenience: load with safety disabled (openclaw compat default).
    pub fn load() -> Self {
        Self::load_with_safety(false)
    }

    /// Try to pre-parse a user message.
    /// Returns PassThrough if no local command matched.
    pub fn try_parse(&self, input: &str) -> PreParseResult {
        let trimmed = input.trim();

        // Don't intercept empty or very long messages
        if trimmed.is_empty() || trimmed.len() > 500 {
            return PreParseResult::PassThrough;
        }

        for (pattern, handler) in &self.commands {
            if let Some(caps) = pattern.captures(trimmed) {
                match handler {
                    CommandHandler::Tool { tool, arg_template } => {
                        let args = build_args(arg_template, &caps, trimmed);

                        // For exec tool, check safety rules (only when enabled)
                        if tool == "exec" && self.safety_enabled {
                            if let Some(cmd_str) = args["cmd"].as_str() {
                                let full_cmd = if let Some(cmd_args) = args["args"].as_array() {
                                    format!(
                                        "{} {}",
                                        cmd_str,
                                        cmd_args
                                            .iter()
                                            .filter_map(|a| a.as_str())
                                            .collect::<Vec<_>>()
                                            .join(" ")
                                    )
                                } else {
                                    cmd_str.to_owned()
                                };
                                match self.check_safety(&full_cmd) {
                                    SafetyCheck::Allow => {}
                                    SafetyCheck::Deny(reason) => {
                                        return PreParseResult::Blocked(reason);
                                    }
                                    SafetyCheck::Confirm(reason) => {
                                        return PreParseResult::NeedsConfirm {
                                            command: full_cmd,
                                            reason,
                                        };
                                    }
                                }
                            }
                        }

                        info!(tool = tool, "pre-parse: local command matched");
                        return PreParseResult::ToolCall {
                            tool: tool.clone(),
                            args,
                        };
                    }
                    CommandHandler::Direct(f) => {
                        let response = f(&caps);
                        return PreParseResult::DirectResponse(response);
                    }
                    CommandHandler::BuiltIn(f) => match f(&caps) {
                        Ok(response) => return PreParseResult::DirectResponse(response),
                        Err(e) => return PreParseResult::DirectResponse(format!("error: {e}")),
                    },
                }
            }
        }

        PreParseResult::PassThrough
    }

    /// Check exec command against safety rules.
    pub fn check_exec_safety(&self, cmd: &str) -> SafetyCheck {
        if !self.safety_enabled {
            return SafetyCheck::Allow;
        }
        self.check_safety(cmd)
    }

    fn check_safety(&self, cmd: &str) -> SafetyCheck {
        // Allow overrides deny
        for pat in &self.allow_patterns {
            if pat.is_match(cmd) {
                return SafetyCheck::Allow;
            }
        }
        // Check deny
        for pat in &self.deny_patterns {
            if pat.is_match(cmd) {
                return SafetyCheck::Deny(format!(
                    "command blocked by safety rule: {}",
                    pat.as_str()
                ));
            }
        }
        // Check confirm
        for pat in &self.confirm_patterns {
            if pat.is_match(cmd) {
                return SafetyCheck::Confirm(format!(
                    "dangerous command requires confirmation: {}",
                    pat.as_str()
                ));
            }
        }
        SafetyCheck::Allow
    }
}

pub enum SafetyCheck {
    Allow,
    Deny(String),
    Confirm(String),
}

// --- Helper functions ---

fn add_cmd(
    commands: &mut Vec<(Regex, CommandHandler)>,
    pattern: &str,
    tool: &str,
    template: ArgTemplate,
) {
    if let Ok(re) = Regex::new(pattern) {
        commands.push((
            re,
            CommandHandler::Tool {
                tool: tool.to_owned(),
                arg_template: template,
            },
        ));
    }
}

fn add_direct(
    commands: &mut Vec<(Regex, CommandHandler)>,
    pattern: &str,
    f: impl Fn(&regex::Captures<'_>) -> String + Send + Sync + 'static,
) {
    if let Ok(re) = Regex::new(pattern) {
        commands.push((re, CommandHandler::Direct(Box::new(f))));
    }
}

fn build_args(template: &ArgTemplate, caps: &regex::Captures<'_>, _full_input: &str) -> Value {
    match template {
        ArgTemplate::Single { param, group } => {
            if *group == 0 {
                // Special: for /ls, pass args directly like shell
                let user_args = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                if user_args.is_empty() {
                    return json!({"cmd": "ls", "args": []});
                }
                // Split user args and pass through
                let args: Vec<&str> = user_args.split_whitespace().collect();
                return json!({"cmd": "ls", "args": args});
            }
            let val = caps.get(*group).map(|m| m.as_str().trim()).unwrap_or("");
            json!({ param: val })
        }
        ArgTemplate::Named(_map) => {
            // For /write <path> <content> or /send <target> <text>
            if let (Some(g1), Some(g2)) = (caps.get(1), caps.get(2)) {
                json!({"path": g1.as_str().trim(), "content": g2.as_str()})
            } else {
                json!({})
            }
        }
        ArgTemplate::SearchQuery => {
            let input = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            let known = ["google", "bing", "baidu", "ddg", "duckduckgo", "sogou", "serper", "brave"];
            if let Some(last_space) = input.rfind(' ') {
                let last_word = input[last_space + 1..].trim().to_lowercase();
                if known.contains(&last_word.as_str()) {
                    let provider = match last_word.as_str() {
                        "ddg" | "duckduckgo" => "duckduckgo-free",
                        "baidu" => "baidu-free",
                        "sogou" => "sogou-free",
                        other => other,
                    };
                    return json!({"query": input[..last_space].trim(), "provider": provider});
                }
            }
            json!({"query": input})
        }
        ArgTemplate::Static(val) => {
            let mut v = val.clone();
            // Append capture group 1 to the args array or as url
            if let Some(captured) = caps.get(1) {
                let text = captured.as_str().trim();
                if let Some(obj) = v.as_object_mut() {
                    if let Some(arr) = obj.get_mut("args").and_then(|a| a.as_array_mut()) {
                        // If last arg ends with space (e.g. "sh -c 'grep -rn '"),
                        // concatenate instead of pushing a new element.
                        // This ensures sh -c gets one complete command string.
                        let should_concat = arr
                            .last()
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s.ends_with(' '));
                        if should_concat {
                            if let Some(last) = arr
                                .last_mut()
                                .and_then(|v| v.as_str().map(|s| s.to_owned()))
                            {
                                *arr.last_mut().unwrap() = json!(format!("{last}{text}"));
                            }
                        } else {
                            arr.push(json!(text));
                        }
                    } else {
                        // No args array -- insert as url (for screenshot, etc.)
                        obj.insert("url".to_owned(), json!(text));
                    }
                }
            }
            v
        }
    }
}

fn load_safety_rules() -> (Vec<Regex>, Vec<Regex>, Vec<Regex>) {
    #[derive(serde::Deserialize, Default)]
    struct Defs {
        #[serde(default)]
        exec_safety: ExecSafety,
    }
    #[derive(serde::Deserialize, Default)]
    struct ExecSafety {
        #[serde(default)]
        deny: Vec<String>,
        #[serde(default)]
        confirm: Vec<String>,
        #[serde(default)]
        allow: Vec<String>,
    }

    let content = crate::config::loader::load_defaults_toml();

    let defs: Defs = toml::from_str(&content).unwrap_or_default();

    let compile = |patterns: &[String]| -> Vec<Regex> {
        patterns
            .iter()
            .filter_map(|p| match Regex::new(&format!("(?i){p}")) {
                Ok(re) => Some(re),
                Err(e) => {
                    warn!(pattern = p, error = %e, "invalid safety regex");
                    None
                }
            })
            .collect()
    };

    (
        compile(&defs.exec_safety.deny),
        compile(&defs.exec_safety.confirm),
        compile(&defs.exec_safety.allow),
    )
}
