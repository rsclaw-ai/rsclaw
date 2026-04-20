//! Tool list builder — generates the consolidated ToolDef list for an agent.
//!
//! Extracted from `runtime.rs` to reduce file size.
//! All public items are re-exported by `runtime.rs` so callers are unaffected.

use serde_json::{Value, json};

use crate::{
    config::schema::ExternalAgentConfig,
    provider::ToolDef,
    skill::SkillRegistry,
};
use super::registry::AgentRegistry;

/// Compute the set of allowed tool names based on toolset level + custom tools.
/// Returns None for "full" (no filtering), Some(set) for others.
pub(crate) fn toolset_allowed_names(
    toolset: &str,
    custom_tools: Option<&Vec<String>>,
) -> Option<std::collections::HashSet<String>> {
    const MINIMAL: &[&str] = &["execute_command", "read_file", "write_file", "send_file", "list_dir", "search_file", "search_content", "web_search", "web_fetch", "memory", "clarify", "anycli"];
    const WEB: &[&str] = &["web_search", "web_fetch", "web_download", "read_file", "write_file", "list_dir", "search_file", "memory"];
    const CODE: &[&str] = &["execute_command", "read_file", "write_file", "list_dir", "search_file", "search_content", "memory"];
    const STANDARD: &[&str] = &[
        "execute_command",
        "read_file",
        "write_file",
        "list_dir",
        "search_file",
        "search_content",
        "web_search",
        "web_fetch",
        "memory",
        "web_browser",
        "image_gen",
        "video_gen",
        "channel",
        "cron",
        "computer_use",
        "clarify",
        "anycli",
    ];

    // `agent` and `session` are always available regardless of toolset.
    // Permission control is handled at dispatch time based on AgentKind.

    let base: Option<&[&str]> = match toolset {
        "minimal" => Some(MINIMAL),
        "web" => Some(WEB),
        "code" => Some(CODE),
        "standard" => Some(STANDARD),
        "full" => None,
        _ => Some(STANDARD),
    };

    match (base, custom_tools) {
        (None, None) => None, // full, no custom -> no filtering
        (None, Some(extra)) => {
            // full + custom whitelist -> use custom as whitelist
            Some(extra.iter().cloned().collect())
        }
        (Some(base_list), None) => Some(base_list.iter().map(|s| s.to_string()).collect()),
        (Some(base_list), Some(extra)) => {
            // Merge: toolset base + custom extras, deduplicated
            let mut set: std::collections::HashSet<String> =
                base_list.iter().map(|s| s.to_string()).collect();
            set.extend(extra.iter().cloned());
            Some(set)
        }
    }
}

/// Build the complete tool list for an agent runtime.
///
/// Includes built-in tools, per-agent A2A tools, external agent tools,
/// and skill-derived tools.
pub(crate) fn build_tool_list(
    skills: &SkillRegistry,
    agents: Option<&AgentRegistry>,
    caller_id: &str,
    external_agents: &[ExternalAgentConfig],
    wasm_plugins: &[crate::plugin::WasmPlugin],
) -> Vec<ToolDef> {
    let mut tools = Vec::new();

    // Built-in tools — consolidated (32+ tools -> ~13 unified tools).
    tools.push(ToolDef {
        name: "memory".to_owned(),
        description: "Manage long-term memory across sessions.\n\
            Actions:\n\
            - search: Semantic search over stored memories. Example: {\"action\":\"search\",\"query\":\"user preferences\"}\n\
            - get: Retrieve a specific memory by ID. Example: {\"action\":\"get\",\"id\":\"abc-123\"}\n\
            - put: Store a new memory. Example: {\"action\":\"put\",\"text\":\"User prefers dark mode\",\"kind\":\"fact\"}\n\
            Use this tool to recall prior context, user preferences, or previously learned information.\n\
            Search BEFORE answering questions about past conversations or user details.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["search", "get", "put"], "description": "Action to perform: search, get, or put"},
                "query":  {"type": "string", "description": "Search query (for search). Examples: 'user name', 'project deadlines', 'API keys'"},
                "id":     {"type": "string", "description": "Memory document ID (for get)"},
                "text":   {"type": "string", "description": "Content to store (for put). Be specific and include context."},
                "scope":  {"type": "string", "description": "Scope filter (optional)"},
                "kind":   {"type": "string", "description": "Document kind: note (general), fact (verified info), summary (session summary), remember (user explicitly asked to remember)"},
                "top_k":  {"type": "integer", "description": "Max results (for search, default 5)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "read_file".to_owned(),
        description: "Read a file from the agent workspace.\n\
            Path is relative to workspace root.\n\
            Supports text files, code, config, markdown, etc.\n\
            Example: {\"path\":\"config.json\"} or {\"path\":\"src/main.py\"}\n\
            For binary files (images, PDFs), use the dedicated tools instead.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative file path. Examples: 'README.md', 'src/app.py', 'data/output.csv'"}
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: "write_file".to_owned(),
        description: "Write/create a file. Use this for ALL file creation and writing — do NOT use execute_command with notepad, echo, or any other editor/command to create files.\n\
            Creates parent directories as needed. Path is relative to workspace root.\n\
            Both 'path' and 'content' are required.\n\
            CRITICAL: When writing user-provided content, copy it EXACTLY character-by-character. \
            Never omit, rephrase, or regenerate numbers, dates, addresses, names, or any specific values. \
            If the user said '135号168栋', the content MUST contain '135号168栋' exactly.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "Relative file path within the workspace (REQUIRED). Example: 'output.py'"},
                "content": {"type": "string", "description": "File content to write (REQUIRED). MUST preserve all numbers, dates, and specific values from the user's message exactly as given."},
                "explanation": {"type": "string", "description": "Brief explanation of what you are creating and why, to help organize your thoughts before writing content."}
            },
            "required": ["path", "content"]
        }),
    });
    tools.push(ToolDef {
        name: "send_file".to_owned(),
        description: "Send a file from the workspace to the user as an attachment. \
            Use this when the user asks you to send, share, or download a file. \
            The file will be delivered as a chat attachment (not as text). \
            Path is relative to workspace root.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to send (relative to workspace or absolute)"}
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: "execute_command".to_owned(),
        description: if cfg!(target_os = "windows") {
            "Run a shell command (PowerShell) on Windows.\n\
             IMPORTANT: For file listing use `list_dir`, for file search use `search_file`, for content search use `search_content`, for tool install use `install_tool`. Only use exec for commands that have no dedicated tool.\n\
             Use exec for: git operations, running scripts (node/python/cargo), system info (systeminfo, ipconfig, Get-Process), package management (npm/pip), process management (Start-Process, Stop-Process, taskkill).\n\
             PowerShell tips:\n\
             - Pipes: Get-Process | Sort-Object CPU -Descending | Select-Object -First 10\n\
             - Network: Test-NetConnection host -Port 80; Invoke-WebRequest -Uri <url>\n\
             - Text: (Get-Content file) -replace 'old','new'\n\
             - Dates: Get-Date -Format 'yyyy-MM-dd'; [DateTimeOffset]::Now.ToUnixTimeSeconds()\n\
             - Do NOT wrap commands in extra cmd /c or powershell -Command layers.\n\
             - Do NOT use exec for destructive operations on personal directories (Desktop, Downloads, Documents).\n\
             - Commands run in background by default (wait=false). Use wait=true only for short commands where you need the output immediately.\n\
             - If a command fails, do NOT retry with the same arguments. Try a different approach or ask the user."
                .to_owned()
        } else if cfg!(target_os = "macos") {
            "Run a shell command (bash/zsh) on macOS.\n\
             IMPORTANT: For file listing use `list_dir`, for file search use `search_file`, for content search use `search_content`, for tool install use `install_tool`. Only use exec for commands that have no dedicated tool.\n\
             Use exec for: git operations, running scripts (node/python/cargo), system info (uname, df, top), package management (brew/npm/pip), process management (ps, kill).\n\
             Tips: Use `date +%s` for Unix timestamps (never calculate manually). Use `| head -n 20` to limit output.\n\
             If a command fails, do NOT retry with the same arguments. Try a different approach or ask the user."
                .to_owned()
        } else {
            "Run a shell command (bash/sh) on Linux.\n\
             IMPORTANT: For file listing use `list_dir`, for file search use `search_file`, for content search use `search_content`, for tool install use `install_tool`. Only use exec for commands that have no dedicated tool.\n\
             Use exec for: git operations, running scripts (node/python/cargo), system info (uname, df, top), package management (apt/npm/pip), process management (ps, kill).\n\
             Tips: Use `date +%s` for Unix timestamps (never calculate manually). Use `| head -n 20` to limit output.\n\
             If a command fails, do NOT retry with the same arguments. Try a different approach or ask the user."
                .to_owned()
        },
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute. Must be valid for the current OS."},
                "timeout": {"type": "integer", "description": "Timeout in seconds (default: 30, max: 300)"},
                "wait": {"type": "boolean", "description": "If true, wait for the command to finish and return output. If false (default), run in background and return a task_id. Results are delivered on your next turn."},
                "task_id": {"type": "string", "description": "Poll a previously started background task by its task_id."}
            },
            "required": []
        }),
    });
    tools.push(ToolDef {
        name: "agent".to_owned(),
        description: "Manage agents. You are the architect — delegate work, never block.\n\
            Actions:\n\
            - task: Create a task agent for a one-shot job. Returns immediately with task_id. The task agent runs independently and delivers results when done.\n\
            - spawn: Create a persistent agent (survives across turns).\n\
            - send: Send a message to an existing agent (async, result delivered when done).\n\
            - list: List all registered agents.\n\
            - kill: Stop an agent.\n\
            Tips:\n\
            - Use task for independent, parallelizable work. You can dispatch multiple tasks at once.\n\
            - Always specify toolset matching the task (web for search, code for file ops).\n\
            - After dispatching, tell the user what you delegated and continue with other work.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["spawn", "task", "send", "list", "kill"], "description": "Action to perform"},
                "id":      {"type": "string", "description": "Agent ID (for spawn/send/kill)"},
                "model":   {"type": "string", "description": "Model string (for spawn/task)"},
                "system":  {"type": "string", "description": "Role description (for spawn/task)"},
                "message": {"type": "string", "description": "Message to send (for task/send)"},
                "toolset": {"type": "string", "enum": ["minimal", "standard", "web", "code", "full"], "description": "Tool access level. Default: standard."}
            },
            "required": ["action"]
        }),
    });

    // Tool installer (structured alternative to exec rsclaw tools install).
    tools.push(ToolDef {
        name: "install_tool".to_owned(),
        description: "Install a tool/runtime. Available: python, node, ffmpeg, chrome, opencode, claude-code, sherpa-onnx.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "enum": ["python", "node", "ffmpeg", "chrome", "opencode", "claude-code", "sherpa-onnx"], "description": "Tool name to install"}
            },
            "required": ["name"]
        }),
    });

    // File operation tools (structured alternatives to exec ls/find/grep).
    // These help small models avoid digit-loss and dead-loop issues.
    tools.push(ToolDef {
        name: "list_dir".to_owned(),
        description: "List files and directories in a given path.\n\
            Use this instead of execute_command with ls/dir.\n\
            - Returns file names, sizes, and types.\n\
            - Does not display hidden/dot files by default.\n\
            - Use 'pattern' to filter by glob (e.g. '*.json').\n\
            - Use 'recursive' to list subdirectories.\n\
            CRITICAL: 'path' must be returned before other parameters.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":      {"type": "string", "description": "Directory path to list. Relative to workspace root or absolute. Examples: '.', 'src/', '/tmp'"},
                "recursive": {"type": "boolean", "description": "If true, list all files in subdirectories recursively. Default: false."},
                "pattern":   {"type": "string", "description": "Glob pattern filter. Examples: '*.json', '*.py', 'test_*'"}
            }
        }),
    });
    tools.push(ToolDef {
        name: "search_file".to_owned(),
        description: "Search for files by name pattern. Use this instead of execute_command with find.\n\
            - Supports wildcard patterns for flexible matching.\n\
            - Returns relative file paths.\n\
            - Prefer this over list_dir when you have a specific file pattern.\n\
            CRITICAL: 'pattern' must be returned before other parameters.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "REQUIRED: File name pattern with wildcards. Examples: '*.log', 'config*', 'test_*.py', '**/*.rs'"},
                "path":    {"type": "string", "description": "Root directory to search in. Defaults to workspace root. Can be relative or absolute."},
                "max_results": {"type": "integer", "description": "Maximum results to return (default: 20)"}
            },
            "required": ["pattern"]
        }),
    });
    tools.push(ToolDef {
        name: "search_content".to_owned(),
        description: "Search file contents by regex or text pattern. Built on ripgrep.\n\
            Use this instead of execute_command with grep/rg. This tool is faster and respects .gitignore.\n\
            - Supports full regex syntax: 'log.*Error', 'function\\s+\\w+', 'TODO|FIXME'\n\
            - Escape special chars for literal matches: 'functionCall\\('\n\
            - Use 'include' to filter by file type: '*.py', '*.rs'\n\
            CRITICAL: 'pattern' must be returned before other parameters.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern":  {"type": "string", "description": "REQUIRED: Regex pattern to search for. Examples: 'TODO', 'import.*from', 'class\\s+\\w+', 'def main'"},
                "path":     {"type": "string", "description": "File or directory to search in. Defaults to workspace root."},
                "include":  {"type": "string", "description": "File glob filter. Examples: '*.py', '*.{ts,tsx}', '*.rs'"},
                "ignore_case": {"type": "boolean", "description": "If true, match case-insensitively. Default: false."},
                "max_results": {"type": "integer", "description": "Maximum results (default: 20)"}
            },
            "required": ["pattern"]
        }),
    });

    // Web tools.
    tools.push(ToolDef {
        name: "web_search".to_owned(),
        description: "Search the web for real-time information.\n\
            When to use:\n\
            - Questions beyond your knowledge cutoff or training data\n\
            - Current events, recent updates, time-sensitive information\n\
            - Latest documentation, API references, version-specific features\n\
            - When unsure about facts — search BEFORE saying 'I don't know'\n\
            Tips:\n\
            - Be specific: include version numbers, dates, or exact terms\n\
            - Use the current year (not past years) for latest docs\n\
            - For Chinese content, search in Chinese for better results".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query":    {"type": "string", "description": "Search query — be specific, include keywords and dates"},
                "provider": {"type": "string", "description": "Search provider: duckduckgo, google, bing, brave. Leave empty for default."},
                "limit":    {"type": "integer", "description": "Max results (default 5)"}
            },
            "required": ["query"]
        }),
    });
    tools.push(ToolDef {
        name: "web_fetch".to_owned(),
        description: "Fetch a web page and convert to readable text/markdown.\n\
            Use this to read documentation, articles, API docs, or any web content.\n\
            - URL must be fully-formed (https://...)\n\
            - HTTP auto-upgraded to HTTPS\n\
            - Falls back to browser rendering for JS-heavy pages\n\
            - Results cached 15 minutes\n\
            - For large pages, use 'prompt' to extract specific information\n\
            - This is read-only — does not modify anything\n\
            - If content is behind login, use web_browser instead".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "url":    {"type": "string", "description": "Full URL to fetch (e.g. https://docs.example.com/api)"},
                "prompt": {"type": "string", "description": "What to extract from the page (e.g. 'list all API endpoints')"}
            },
            "required": ["url"]
        }),
    });
    tools.push(ToolDef {
        name: "web_download".to_owned(),
        description: "Download a file (image/video/document/archive) from URL to local path.\n\
            - Supports resume for large files\n\
            - Use use_browser_cookies=true for authenticated downloads (e.g. after logging in via web_browser)\n\
            - Path is relative to workspace/downloads/ — just use filename like 'photo.jpg'\n\
            - Do NOT use execute_command with curl/wget — always use this tool\n\
            - After downloading, use send_file to deliver the file to the user".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "url":  {"type": "string", "description": "Full URL to download"},
                "path": {"type": "string", "description": "Destination filename (e.g. 'video.mp4', 'report.pdf'). Relative to workspace/downloads/."},
                "cookies": {"type": "string", "description": "Cookie header string, e.g. 'session=abc; token=xyz'"},
                "use_browser_cookies": {"type": "boolean", "description": "Auto-extract cookies from active browser session for this URL's domain (use after web_browser login)"}
            },
            "required": ["url", "path"]
        }),
    });
    tools.push(ToolDef {
        name: "web_browser".to_owned(),
        description: "Control a web browser. Core workflow:\n\
            1. `open` — navigate to a URL\n\
            2. `snapshot` — get page structure with interactive element refs (@e1, @e2...). Use `interactive: true` to only get actionable elements (saves tokens).\n\
            3. `click` ref=@e1 / `fill` ref=@e2 text='...' — interact using refs\n\
            4. Re-snapshot after any page change to get updated refs\n\
            Interaction: hover (triggers menus/tooltips), dblclick, drag (from=@e1 to=@e2, for sliders), focus, scrollintoview.\n\
            Quick search: `search` — auto-find search box on ANY site, fill text, submit, return results.\n\
            `clickAt` ref=@e1 or x=100 y=200 — real mouse click via CDP (for file dialogs, anti-bot sites).\n\
            Semantic locators: `getbytext` value='Submit', `getbyrole` value='button', `getbylabel` value='Email' — find elements without @ref.\n\
            Frame: `frame` selector=@e1 (switch to iframe), `mainframe` (switch back).\n\
            Console: `console` — get browser console messages (log/warn/error).\n\
            Content: `content` — get full page HTML.\n\
            WaitForUrl: `waitforurl` url='dashboard' — wait for URL change (after login/redirect).\n\
            Other: type, select, check, scroll, screenshot, pdf, press, back, forward, reload, wait, evaluate, cookies, get_text, get_url, get_title, find, get_article, upload, new_tab, switch_tab, close_tab.\n\
            IMPORTANT: Always snapshot BEFORE clicking/filling. Element refs change after page updates.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":     {"type": "string", "enum": [
                    "open", "navigate", "snapshot", "click", "clickAt", "fill", "type",
                    "select", "check", "uncheck", "scroll", "screenshot", "pdf",
                    "hover", "dblclick", "drag", "focus", "scrollintoview",
                    "back", "forward", "reload", "get_text", "get_url", "get_title",
                    "wait", "evaluate", "cookies", "press", "set_viewport",
                    "dialog", "state", "network", "new_tab", "list_tabs",
                    "switch_tab", "close_tab", "highlight", "clipboard", "find",
                    "get_article", "upload", "context", "emulate", "diff", "record",
                    "search", "console", "content", "frame", "mainframe",
                    "waitforurl", "getbytext", "getbyrole", "getbylabel"
                ]},
                "url":        {"type": "string", "description": "URL for open/navigate"},
                "interactive":{"type": "boolean", "description": "For snapshot: only return actionable elements (saves ~80% tokens). Default: false"},
                "ref":        {"type": "string", "description": "Element ref like @e3 from snapshot"},
                "from":       {"type": "string", "description": "Source element ref for drag"},
                "to":         {"type": "string", "description": "Target element ref for drag"},
                "x":          {"type": "number", "description": "X pixel coordinate for clickAt"},
                "y":          {"type": "number", "description": "Y pixel coordinate for clickAt"},
                "text":       {"type": "string", "description": "Text for fill/type/click-by-text/clipboard/dialog"},
                "value":      {"type": "string", "description": "Value for select, or sub-action for cookies/state/dialog/network/clipboard/context/emulate/diff/record"},
                "key":        {"type": "string", "description": "Key name for press (Enter, Tab, Escape, etc.)"},
                "direction":  {"type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction"},
                "amount":     {"type": "integer", "description": "Scroll distance in pixels (default 500)"},
                "selector":   {"type": "string", "description": "CSS selector for scroll container"},
                "js":         {"type": "string", "description": "JavaScript for evaluate action"},
                "target":     {"type": "string", "description": "Wait target: CSS selector, text, url, networkidle, fn"},
                "timeout":    {"type": "number", "description": "Timeout in seconds (default 15)"},
                "format":     {"type": "string", "enum": ["png", "jpeg"], "description": "Screenshot format"},
                "quality":    {"type": "integer", "description": "JPEG quality (1-100)"},
                "full_page":  {"type": "boolean", "description": "Capture full scrollable page"},
                "annotate":   {"type": "boolean", "description": "Overlay numbered labels on interactive elements"},
                "width":      {"type": "integer", "description": "Viewport width for set_viewport"},
                "height":     {"type": "integer", "description": "Viewport height for set_viewport"},
                "scale":      {"type": "number", "description": "Device scale factor for set_viewport"},
                "mobile":     {"type": "boolean", "description": "Mobile emulation for set_viewport"},
                "target_id":  {"type": "string", "description": "Tab target ID for switch_tab/close_tab"},
                "state":      {"type": "object", "description": "State object for state load"},
                "pattern":    {"type": "string", "description": "URL pattern for network block/intercept"},
                "by":         {"type": "string", "enum": ["text", "label"], "description": "Find element by text or label"},
                "then":       {"type": "string", "description": "Action after find (click)"},
                "cookie":     {"type": "object", "description": "Cookie object for cookies set"},
                "files":      {"type": "array", "items": {"type": "string"}, "description": "File paths for upload"},
                "context_id": {"type": "string", "description": "Browser context ID for cookie isolation"},
                "latitude":   {"type": "number", "description": "Latitude for geolocation emulation"},
                "longitude":  {"type": "number", "description": "Longitude for geolocation emulation"},
                "accuracy":   {"type": "number", "description": "Geolocation accuracy in meters"},
                "locale":     {"type": "string", "description": "Locale for emulation (e.g. en-US, zh-CN)"},
                "timezone_id":{"type": "string", "description": "IANA timezone (e.g. Asia/Shanghai)"},
                "permissions":{"type": "array", "items": {"type": "string"}, "description": "Browser permissions to grant"},
                "action_type":{"type": "string", "description": "Intercept action: block or mock"},
                "body":       {"type": "string", "description": "Mock response body for network intercept"},
                "headed":     {"type": "boolean", "description": "true=foreground (visible window), false=background (headless). Default: auto-detect based on display availability. Omit this field to use the default."}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "computer_use".to_owned(),
        description: "Control the computer desktop. ONLY use when the user EXPLICITLY asks to take a screenshot, click, type, or interact with the desktop. Do NOT call this tool just because the message mentions words like 'screenshot' or 'screen' in other contexts. Screenshots auto-resize for HiDPI and return scale factor for coordinate mapping.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":    {"type": "string", "enum": [
                    "screenshot", "mouse_move", "mouse_click", "left_click",
                    "double_click", "triple_click", "right_click", "middle_click",
                    "drag", "scroll", "type", "key", "hold_key",
                    "cursor_position", "get_active_window", "wait"
                ], "description": "Action to perform"},
                "x":         {"type": "number", "description": "X coordinate (mouse actions, drag start)"},
                "y":         {"type": "number", "description": "Y coordinate (mouse actions, drag start)"},
                "to_x":      {"type": "number", "description": "Drag destination X"},
                "to_y":      {"type": "number", "description": "Drag destination Y"},
                "button":    {"type": "string", "enum": ["left", "right", "middle"], "description": "Mouse button (default: left)"},
                "text":      {"type": "string", "description": "Text for type action"},
                "key":       {"type": "string", "description": "Key name or combo (e.g. Enter, ctrl+c, cmd+shift+s)"},
                "then":      {"type": "string", "enum": ["click", "double_click", "right_click", "triple_click"], "description": "Sub-action for hold_key (default: click)"},
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction (default: down)"},
                "amount":    {"type": "integer", "description": "Scroll clicks (default: 3)"},
                "ms":        {"type": "integer", "description": "Wait duration in milliseconds (max 10000)"}
            },
            "required": ["action"]
        }),
    });

    // --- New openclaw-compatible tools ---

    tools.push(ToolDef {
        name: "image_gen".to_owned(),
        description: "Generate an image from a text description using an AI image model. Pass the user's original description as-is (preserve their language, do not translate).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Image description. IMPORTANT: use the user's original language and wording, do not translate to English."},
                "size":   {"type": "string", "description": "Image size, e.g. 2048x2048", "default": "2048x2048"}
            },
            "required": ["prompt"]
        }),
    });
    tools.push(ToolDef {
        name: "video_gen".to_owned(),
        description: "Generate a video from a text description using an AI video model. \
            Use this tool whenever the user asks to: create a video, animate an image, \
            generate a clip, make a short film, produce footage, or anything involving \
            video output. Pass the user's original description as-is (preserve their \
            language, do not translate).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt":       {"type": "string", "description": "Video description. Use the user's original language and wording."},
                "duration":     {"type": "integer", "description": "Duration in seconds (default: 5)", "default": 5},
                "aspect_ratio": {"type": "string", "description": "Aspect ratio: 16:9, 9:16, 1:1 (default: 16:9)", "default": "16:9"},
                "model":        {"type": "string", "description": "Video model to use, e.g. seedance, minimax, kling (optional, uses configured default)"}
            },
            "required": ["prompt"]
        }),
    });
    tools.push(ToolDef {
        name: "pdf".to_owned(),
        description: "Extract text content from a PDF file or URL.\n\
            - Supports local files and remote URLs.\n\
            - Returns extracted text suitable for analysis.\n\
            - For large PDFs, content may be truncated.\n\
            Example: {\"path\":\"report.pdf\"} or {\"path\":\"https://example.com/doc.pdf\"}".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "REQUIRED: File path (relative to workspace) or full URL. Examples: 'docs/report.pdf', 'https://example.com/whitepaper.pdf'"}
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: "text_to_voice".to_owned(),
        description: "Convert text to speech audio and send as voice message.\n\
            - Generates audio from text input.\n\
            - On macOS uses 'say', on Linux uses espeak/sherpa-onnx.\n\
            - Result is sent as a voice attachment to the user.\n\
            Example: {\"text\":\"Hello world\",\"voice\":\"Tingting\"}".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "text":  {"type": "string", "description": "REQUIRED: Text to convert to speech. Can be any language."},
                "voice": {"type": "string", "description": "Voice name. macOS: run 'say -v ?' for list. Linux: run 'espeak --voices'. Examples: 'Tingting' (Chinese), 'Samantha' (English)"}
            },
            "required": ["text"]
        }),
    });
    tools.push(ToolDef {
        name: "send_message".to_owned(),
        description: "Send a message to a chat channel target (user or group).\n\
            Use this to proactively reach out to users on messaging platforms.\n\
            Channel is auto-detected from current session if not specified.\n\
            Example: {\"target\":\"user123\",\"text\":\"Task completed!\",\"channel\":\"telegram\"}".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string", "description": "Channel type. Examples: 'telegram', 'discord', 'feishu', 'weixin', 'slack'"},
                "target":  {"type": "string", "description": "REQUIRED: Target user ID or group/chat ID"},
                "text":    {"type": "string", "description": "REQUIRED: Message text to send"}
            },
            "required": ["target", "text"]
        }),
    });
    tools.push(ToolDef {
        name: "cron".to_owned(),
        description: "List, add, edit, remove, enable or disable cron jobs.\n\
            Supports both recurring (cron expression) and one-shot (delay_ms) schedules.\n\
            For one-shot: set delay_ms instead of schedule. Example: delay_ms=1200000 for 20 minutes.\n\
            One-shot jobs auto-remove after execution.\n\
            For edit/remove/enable/disable, prefer using `index` from the list output instead of `id`.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":   {"type": "string", "enum": ["list", "add", "edit", "remove", "enable", "disable"], "description": "Action to perform"},
                "schedule": {"type": "string", "description": "Cron schedule expression (for add/edit recurring jobs)"},
                "delay_ms": {"type": "number", "description": "Delay in milliseconds for one-shot timer (e.g., 1200000 = 20 min). Use instead of schedule for reminders/timers."},
                "message":  {"type": "string", "description": "Message or task to run (for add, edit)"},
                "index":    {"type": "number", "description": "Job index from list (1-based, for edit/remove/enable/disable - preferred)"},
                "id":       {"type": "string", "description": "Job ID (for edit/remove/enable/disable - use index instead if possible)"},
                "name":     {"type": "string", "description": "Job name (for add, edit)"},
                "tz":       {"type": "string", "description": "Timezone e.g. Asia/Shanghai (for add, edit)"},
                "agentId":  {"type": "string", "description": "Agent ID to run the job (for add, edit, default: main)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "session".to_owned(),
        description: "Manage sessions. Actions: send (message to another agent), list (all active sessions), history (retrieve conversation), status (session info).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":     {"type": "string", "enum": ["send", "list", "history", "status"], "description": "Action to perform"},
                "agentId":    {"type": "string", "description": "Target agent ID (for send)"},
                "sessionKey": {"type": "string", "description": "Session key (for send/history/status)"},
                "message":    {"type": "string", "description": "Message text (for send)"},
                "limit":      {"type": "number", "description": "Max messages to return (for history, default 50)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "gateway".to_owned(),
        description: "Query gateway status and information.\n\
            - status: Current gateway state, uptime, connected channels, active agents\n\
            - health: Health check (OK/degraded)\n\
            - version: Gateway version and build info".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["status", "health", "version"], "description": "REQUIRED: Info to retrieve. Examples: 'status', 'version'"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "opencode".to_owned(),
        description: "Execute coding tasks using OpenCode (a powerful coding agent). IMPORTANT: When creating new projects or files, ALWAYS create a dedicated project directory first (e.g., 'my-project/') and place all files inside it. Do NOT create files directly in the workspace root. The task will run asynchronously and results will be sent when complete.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The coding task to execute. Be specific about file paths and always mention creating a project subdirectory for new projects."}
            },
            "required": ["task"]
        }),
    });
    tools.push(ToolDef {
        name: "claudecode".to_owned(),
        description: "Execute coding tasks using Claude Code (official Claude Agent SDK via ACP protocol). Uses Claude's native coding capabilities with full context awareness. IMPORTANT: When creating new projects or files, ALWAYS create a dedicated project directory first. The task will run asynchronously and results will be sent when complete.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The coding task to execute. Be specific about requirements and file paths."}
            },
            "required": ["task"]
        }),
    });
    tools.push(ToolDef {
        name: "channel".to_owned(),
        description: "Perform channel-specific actions (send, reply, pin, delete messages). Channel is auto-detected from current session or can be specified explicitly: telegram, discord, slack, whatsapp, feishu, weixin, qq, dingtalk.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":    {"type": "string", "enum": ["send", "reply", "forward", "pin", "unpin", "delete"], "description": "Action to perform"},
                "channel":   {"type": "string", "description": "Channel type (auto-detected if omitted): telegram, discord, slack, whatsapp, feishu, weixin, qq, dingtalk"},
                "chatId":    {"type": "string", "description": "Chat/channel ID"},
                "text":      {"type": "string", "description": "Message text"},
                "messageId": {"type": "string", "description": "Message ID (for reply/pin/delete)"}
            },
            "required": ["action"]
        }),
    });

    tools.push(ToolDef {
        name: "anycli".to_owned(),
        description: "Extract structured data from websites using declarative adapters.\n\
            Actions:\n\
            - run: Execute an adapter command (e.g., hackernews top, bilibili hot)\n\
            - list: List all available adapters\n\
            - info: Show adapter details and available commands\n\
            - search: Search community hub for adapters\n\
            - install: Install an adapter from the hub\n\
            Built-in adapters: hackernews, bilibili, arxiv, wikipedia, github-trending.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["run", "list", "info", "search", "install"], "description": "Action to perform"},
                "adapter": {"type": "string", "description": "Adapter name (for run/info)"},
                "command": {"type": "string", "description": "Command name within adapter (for run)"},
                "params":  {"type": "object", "description": "Key-value parameters (for run), e.g. {\"limit\": \"10\", \"query\": \"rust\"}"},
                "query":   {"type": "string", "description": "Search query (for search)"},
                "name":    {"type": "string", "description": "Adapter name (for install)"},
                "format":  {"type": "string", "enum": ["json", "table", "csv", "markdown"], "description": "Output format (for run, default: json)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "clarify".to_owned(),
        description: "Ask the user a clarifying question before proceeding. Use when:\n\
            - The request is ambiguous and multiple valid interpretations exist\n\
            - A choice is needed (e.g., which file, which format, which approach)\n\
            - Destructive or irreversible action needs confirmation\n\
            Provide options for quick selection or leave open-ended for free-form answers.\n\
            IMPORTANT: Do NOT use this for simple confirmations. Only when genuine ambiguity exists.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "The question to ask the user"},
                "options":  {"type": "array", "items": {"type": "string"}, "description": "Optional list of choices. Omit for open-ended questions."}
            },
            "required": ["question"]
        }),
    });
    tools.push(ToolDef {
        name: "pairing".to_owned(),
        description: "Manage channel pairing (dmPolicy=pairing). Actions: list (show pending codes and approved peers), approve (approve a pairing code), revoke (revoke an approved peer).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["list", "approve", "revoke"], "description": "Action to perform"},
                "code":    {"type": "string", "description": "Pairing code to approve (for approve action, e.g. ZGTB-NB79)"},
                "channel": {"type": "string", "description": "Channel name (for revoke action, e.g. qq, telegram)"},
                "peerId":  {"type": "string", "description": "Peer ID to revoke (for revoke action)"}
            },
            "required": ["action"]
        }),
    });

    // Document tools — split into simple independent tools for better small-model compatibility.
    // Formatting note injected into content-bearing tools.
    let doc_fmt_hint = " Structure content professionally: use # headings, - bullet lists, blank lines between sections. For notices/reports: add title, organize into sections.";

    tools.push(ToolDef {
        name: "create_docx".to_owned(),
        description: format!("Create a Word document (.docx).{doc_fmt_hint} After creating, use send_file to deliver."),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "File path, e.g. 'report.docx'"},
                "content": {"type": "string", "description": "Document content. Use # for headings, - for lists, blank lines for paragraphs."},
                "title":   {"type": "string", "description": "Document title (optional, displayed at top)"},
                "explanation": {"type": "string", "description": "Brief explanation of what you are creating and why, to help organize your thoughts before writing content."}
            },
            "required": ["path", "content"]
        }),
    });
    tools.push(ToolDef {
        name: "create_pdf".to_owned(),
        description: format!("Create a PDF document.{doc_fmt_hint} After creating, use send_file to deliver."),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "File path, e.g. 'report.pdf'"},
                "content": {"type": "string", "description": "Document content. Use # for headings, - for lists, blank lines for paragraphs."},
                "title":   {"type": "string", "description": "Document title (optional, displayed at top)"},
                "explanation": {"type": "string", "description": "Brief explanation of what you are creating and why, to help organize your thoughts before writing content."}
            },
            "required": ["path", "content"]
        }),
    });
    tools.push(ToolDef {
        name: "create_xlsx".to_owned(),
        description: "Create an Excel spreadsheet (.xlsx). Extract structured data into columns with meaningful headers. After creating, use send_file to deliver.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":   {"type": "string", "description": "File path, e.g. 'data.xlsx'"},
                "sheets": {"type": "array", "description": "Sheets: [{name, headers: [str], rows: [[value]]}]",
                    "items": {"type": "object", "properties": {
                        "name":    {"type": "string", "description": "Sheet name (tab label in the spreadsheet)."},
                        "headers": {"type": "array", "items": {"type": "string"}, "description": "Column header labels for the first row."},
                        "rows":    {"type": "array", "items": {"type": "array"}, "description": "Data rows, each an array of cell values in column order."}
                    }}
                },
                "explanation": {"type": "string", "description": "Brief explanation of what you are creating and why, to help organize your thoughts before writing content."}
            },
            "required": ["path", "sheets"]
        }),
    });
    tools.push(ToolDef {
        name: "create_pptx".to_owned(),
        description: "Create a PowerPoint presentation (.pptx). After creating, use send_file to deliver.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":   {"type": "string", "description": "File path, e.g. 'deck.pptx'"},
                "slides": {"type": "array", "description": "Slides: [{title, body}]",
                    "items": {"type": "object", "properties": {
                        "title": {"type": "string", "description": "Slide title displayed at the top."},
                        "body":  {"type": "string", "description": "Slide body text. Use newlines to separate bullet points."}
                    }}
                },
                "explanation": {"type": "string", "description": "Brief explanation of what you are creating and why, to help organize your thoughts before writing content."}
            },
            "required": ["path", "slides"]
        }),
    });
    // Keep doc tool for read/edit operations (less frequently used by small models).
    tools.push(ToolDef {
        name: "doc".to_owned(),
        description: "Read or edit existing documents.\n\
            Actions: read_doc (xlsx/docx/pdf), edit_excel, edit_word, edit_pdf.\n\
            For CREATING new documents, use create_docx/create_pdf/create_xlsx/create_pptx instead.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["read_doc", "edit_excel", "edit_word", "edit_pdf"], "description": "Action to perform"},
                "path":    {"type": "string", "description": "File path"},
                "content": {"type": "string", "description": "For edit_word: replacement content"},
                "append":  {"type": "string", "description": "For edit_word: text to append"},
                "sheets":  {"type": "array", "description": "For edit_excel: [{name, headers, rows}]",
                    "items": {"type": "object", "properties": {
                        "name":    {"type": "string", "description": "Sheet name (tab label in the spreadsheet)."},
                        "headers": {"type": "array", "items": {"type": "string"}, "description": "Column header labels for the first row."},
                        "rows":    {"type": "array", "items": {"type": "array"}, "description": "Data rows, each an array of cell values in column order."}
                    }}
                },
                "append_rows": {"type": "array", "description": "For edit_excel: append rows to an existing sheet without replacing it.",
                    "items": {"type": "object", "properties": {
                        "sheet": {"type": "string", "description": "Name of the existing sheet to append to."},
                        "rows":  {"type": "array", "items": {"type": "array"}, "description": "Rows to append, each an array of cell values."}
                    }}
                },
                "replacements": {"type": "array", "description": "For edit_pdf: [{find, replace}]",
                    "items": {"type": "object", "properties": {
                        "find":    {"type": "string", "description": "Text string to find in the PDF."},
                        "replace": {"type": "string", "description": "Replacement text."}
                    }}
                },
                "delete_pages": {"type": "array", "description": "For edit_pdf: 1-indexed page numbers to delete", "items": {"type": "integer"}}
            },
            "required": ["action", "path"]
        }),
    });

    // Dynamic per-agent A2A tools.
    if let Some(reg) = agents {
        for handle in reg.all() {
            if handle.id == caller_id {
                continue;
            }
            tools.push(ToolDef {
                name: format!("agent_{}", handle.id),
                description: format!(
                    "Send a task to agent '{}'. Returns the agent's reply.",
                    handle.id
                ),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "Task or message to send"}
                    },
                    "required": ["text"]
                }),
            });
        }
    }

    // External remote agent A2A tools (remote gateways).
    tracing::debug!(
        count = external_agents.len(),
        "build_tool_list: external agents"
    );
    for ext in external_agents {
        if ext.id == caller_id {
            continue;
        }
        tools.push(ToolDef {
            name: format!("agent_{}", ext.id),
            description: format!(
                "Send a task to remote agent '{}' at {}. Returns the agent's reply.",
                ext.id, ext.url
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Task or message to send"}
                },
                "required": ["text"]
            }),
        });
    }

    // Skill tools.
    for skill in skills.all() {
        for spec in &skill.tools {
            tools.push(ToolDef {
                name: format!("{}.{}", skill.name, spec.name),
                description: spec.description.clone(),
                parameters: spec
                    .input_schema
                    .clone()
                    .unwrap_or_else(|| Value::Object(Default::default())),
            });
        }
    }

    // WASM plugin tools — replace built-in equivalents.
    let _wasm_replaces: Vec<String> = Vec::new();
    for plugin in wasm_plugins {
        for tool in &plugin.tools {
            let full_name = format!("{}.{}", plugin.name, tool.name);
            tools.push(ToolDef {
                name: full_name,
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            });
        }
    }

    // Inject `additionalProperties: false` and `$schema` into every tool's
    // parameters object. This enables constrained decoding in Ollama/vLLM,
    // which dramatically reduces digit-loss on small models (9b).
    for tool in &mut tools {
        if let Some(obj) = tool.parameters.as_object_mut() {
            obj.entry("additionalProperties").or_insert(json!(false));
            obj.entry("$schema").or_insert(json!("http://json-schema.org/draft-07/schema#"));
        }
    }

    tools
}
