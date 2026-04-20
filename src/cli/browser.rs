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
    /// Run any browser action with JSON args (advanced).
    Raw {
        /// Action name.
        action: String,
        /// JSON arguments.
        #[arg(default_value = "{}")]
        args: String,
    },
}
