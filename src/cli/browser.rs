use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum BrowserCommand {
    /// Open a URL in the browser.
    Open {
        /// URL to navigate to.
        url: String,
    },
    /// Take a snapshot of the current page (accessibility tree with element refs).
    Snapshot {
        /// Only show interactive/actionable elements.
        #[arg(long, short)]
        interactive: bool,
        /// Remove empty structural elements (no text content).
        #[arg(long, short)]
        compact: bool,
        /// Limit tree depth.
        #[arg(long, short)]
        depth: Option<u32>,
        /// Scope snapshot to a CSS selector.
        #[arg(long, short = 'S')]
        selector: Option<String>,
    },
    /// Click an element by @ref.
    Click {
        /// Element reference (e.g., @e3).
        #[arg(name = "ref")]
        eref: String,
    },
    /// Click using real mouse events at coordinates or @ref.
    ClickAt {
        /// Element reference or coordinates.
        #[arg(name = "ref")]
        eref: Option<String>,
        /// X coordinate.
        #[arg(long)]
        x: Option<f64>,
        /// Y coordinate.
        #[arg(long)]
        y: Option<f64>,
    },
    /// Fill text into an input element.
    Fill {
        /// Element reference (e.g., @e5).
        #[arg(name = "ref")]
        eref: String,
        /// Text to fill.
        text: String,
    },
    /// Press a key (Enter, Tab, Escape, etc.).
    Press {
        /// Key name.
        key: String,
    },
    /// Scroll the page.
    Scroll {
        /// Direction: up, down, left, right.
        #[arg(default_value = "down")]
        direction: String,
        /// Distance in pixels.
        #[arg(long, default_value = "500")]
        amount: i32,
    },
    /// Take a screenshot.
    Screenshot {
        /// Output path (default: screenshot.png).
        #[arg(default_value = "screenshot.png")]
        path: String,
    },
    /// Get page text content.
    Text,
    /// Get current URL.
    Url,
    /// Get page title.
    Title,
    /// Get full page HTML.
    Content,
    /// Get browser console messages.
    Console {
        /// Max entries to show.
        #[arg(long, default_value = "50")]
        limit: u64,
    },
    /// Wait for a condition (selector, text, url, networkidle).
    Wait {
        /// Wait target.
        target: String,
        /// Timeout in seconds.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },
    /// Wait for URL to match a pattern.
    WaitForUrl {
        /// URL pattern to match.
        pattern: String,
        /// Timeout in seconds.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },
    /// Execute JavaScript.
    Evaluate {
        /// JavaScript code.
        js: String,
    },
    /// Find elements by text.
    GetByText {
        /// Text to search for.
        text: String,
        /// Exact match.
        #[arg(long)]
        exact: bool,
    },
    /// Find elements by ARIA role.
    GetByRole {
        /// Role name (e.g., button, link, textbox).
        role: String,
    },
    /// Find elements by label.
    GetByLabel {
        /// Label text.
        label: String,
    },
    /// Find element by text content or label.
    Find {
        /// Text or label to search for.
        text: String,
    },
    /// Navigate back.
    Back,
    /// Navigate forward.
    Forward,
    /// Reload page.
    Reload,
    /// Close the browser.
    Close,
    /// Tab management subcommands.
    #[command(subcommand)]
    Tab(TabCommand),
    /// Get element property (text, html, value, attribute, count, bounding box).
    #[command(subcommand)]
    Get(GetCommand),
    /// Get page errors.
    Errors,
    /// Execute multiple browser commands in a single session.
    Batch {
        /// Commands to execute, each as a quoted string (e.g., "open https://example.com" "snapshot -i").
        commands: Vec<String>,
    },
    /// Keyboard input subcommands.
    #[command(subcommand)]
    Keyboard(KeyboardCommand),
    /// Download a file by clicking an element.
    Download {
        /// Element selector or @ref to click.
        selector: String,
        /// Output file path.
        path: String,
    },
    /// Auth vault management for saved credentials.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// List available Chrome profiles.
    Profiles,
    /// Connect to a browser via CDP (port number or ws:// URL).
    Connect {
        /// CDP port number or WebSocket URL (e.g., 9222 or ws://127.0.0.1:9222/...).
        target: String,
    },
    /// Save browser state (cookies + localStorage) to a JSON file.
    StateSave {
        /// Output file path.
        path: String,
    },
    /// Load browser state (cookies + localStorage) from a JSON file.
    StateLoad {
        /// Input file path.
        path: String,
    },
    /// List network requests from the page.
    Requests {
        /// Clear request history after listing.
        #[arg(long)]
        clear: bool,
        /// Filter requests by URL pattern.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Show or list browser sessions/targets.
    Session {
        /// Action: show (current session info) or list (all debugging targets).
        #[arg(default_value = "show")]
        action: String,
    },
    /// Run any browser action with JSON args (advanced).
    Raw {
        /// Action name.
        action: String,
        /// JSON arguments.
        #[arg(default_value = "{}")]
        args: String,
    },
}

/// Tab management subcommands.
#[derive(Subcommand, Debug)]
pub enum TabCommand {
    /// Open a new tab.
    New {
        /// Optional URL to open in the new tab.
        url: Option<String>,
    },
    /// List all open tabs.
    List,
    /// Close a tab by index.
    Close {
        /// Tab index (0-based).
        index: u32,
    },
    /// Switch to a tab by index.
    Switch {
        /// Tab index (0-based).
        index: u32,
    },
}

/// Get element property subcommands.
#[derive(Subcommand, Debug)]
pub enum GetCommand {
    /// Get element text content.
    Text {
        /// CSS selector or @ref.
        selector: Option<String>,
    },
    /// Get element inner HTML.
    Html {
        /// CSS selector or @ref.
        selector: Option<String>,
    },
    /// Get input element value.
    Value {
        /// CSS selector or @ref.
        selector: Option<String>,
    },
    /// Get element attribute value.
    Attr {
        /// Attribute name.
        name: String,
        /// CSS selector or @ref.
        selector: Option<String>,
    },
    /// Count elements matching a selector.
    Count {
        /// CSS selector.
        selector: String,
    },
    /// Get element bounding box.
    Box {
        /// CSS selector or @ref.
        selector: Option<String>,
    },
}

/// Keyboard input subcommands.
#[derive(Subcommand, Debug)]
pub enum KeyboardCommand {
    /// Type text with key events (keyDown/keyUp per character).
    Type {
        /// Text to type.
        text: String,
    },
    /// Insert text directly (bypasses key events).
    Inserttext {
        /// Text to insert.
        text: String,
    },
}

/// Auth vault subcommands.
#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Save credentials to the vault.
    Save {
        /// Site domain or identifier.
        site: String,
        /// Username.
        #[arg(long)]
        username: String,
        /// Password.
        #[arg(long)]
        password: String,
    },
    /// Auto-login using saved credentials.
    Login {
        /// Site domain or identifier.
        site: String,
    },
    /// List saved credential entries.
    List,
    /// Show details for a saved credential.
    Show {
        /// Site domain or identifier.
        site: String,
    },
    /// Delete a saved credential.
    Delete {
        /// Site domain or identifier.
        site: String,
    },
}
