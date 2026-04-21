use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

use super::gateway::gateway_pid_file;
use super::style::{banner, dim, green, kv, red};
use crate::{
    cli::{HealthArgs, LogsArgs, StatusArgs, TuiArgs},
    config,
};

// ---------------------------------------------------------------------------
// cmd_logs / cmd_status / cmd_health
// ---------------------------------------------------------------------------

pub async fn cmd_logs(args: LogsArgs) -> Result<()> {
    let log_file = config::loader::log_file();
    if !log_file.exists() {
        println!("no gateway.log found at {}", log_file.display());
        return Ok(());
    }
    if args.follow {
        // Stream new lines from the log file.
        use tokio::io::{AsyncBufReadExt as _, BufReader};
        let file = tokio::fs::File::open(&log_file).await?;
        let mut reader = BufReader::new(file).lines();
        // Seek to end by consuming existing lines first.
        while reader.next_line().await?.is_some() {}
        loop {
            match reader.next_line().await? {
                Some(line) => println!("{line}"),
                None => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
            }
        }
    } else {
        // Print last N lines (default 50, overridden by --limit).
        let content = std::fs::read_to_string(&log_file)?;
        let lines: Vec<&str> = content.lines().collect();
        let limit = args.limit.unwrap_or(50);
        let start = lines.len().saturating_sub(limit);
        for line in &lines[start..] {
            if args.json {
                println!("{}", serde_json::json!({"line": line}));
            } else {
                println!("{line}");
            }
        }
        Ok(())
    }
}

pub async fn cmd_status(args: StatusArgs) -> Result<()> {
    let version = option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev");
    if args.json {
        let mut info = serde_json::json!({
            "version": version,
        });
        match config::load() {
            Ok(cfg) => {
                info["config"] = serde_json::json!("ok");
                info["agents"] = serde_json::json!(cfg.agents.list.len());
            }
            Err(e) => {
                info["config"] = serde_json::json!(format!("error: {e:#}"));
            }
        }
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        banner(&format!("rsclaw v{version}"));

        // Config path
        let config_path = config::loader::detect_config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| dim("not found").to_string());
        kv("Config:", &config_path);

        // Gateway status
        let gw_status = gateway_status_str();
        let running = gw_status.starts_with("running");
        let status_display = if running { green(&gw_status) } else { red(&gw_status) };
        kv("Gateway:", &status_display);

        // Suppress tracing output during config load for status display
        match config::load_quiet() {
            Ok(cfg) => {
                // Providers
                let provider_count = cfg
                    .model
                    .models
                    .as_ref()
                    .map(|m| m.providers.len())
                    .unwrap_or(0);
                if provider_count > 0 {
                    let mut names: Vec<&str> = cfg
                        .model
                        .models
                        .as_ref()
                        .map(|m| m.providers.keys().map(|k| k.as_str()).collect())
                        .unwrap_or_default();
                    names.sort();
                    kv(
                        "Providers:",
                        &format!("{} ({})", names.join(", "), provider_count),
                    );
                } else {
                    kv("Providers:", &dim("none configured"));
                }

                // Channels -- enumerate active ones from the per-platform Option fields
                let ch = &cfg.channel.channels;
                let mut ch_names: Vec<&str> = Vec::new();
                if ch.telegram.is_some() { ch_names.push("telegram"); }
                if ch.discord.is_some() { ch_names.push("discord"); }
                if ch.slack.is_some() { ch_names.push("slack"); }
                if ch.whatsapp.is_some() { ch_names.push("whatsapp"); }
                if ch.signal.is_some() { ch_names.push("signal"); }
                if ch.imessage.is_some() { ch_names.push("imessage"); }
                if ch.mattermost.is_some() { ch_names.push("mattermost"); }
                if ch.msteams.is_some() { ch_names.push("msteams"); }
                if ch.googlechat.is_some() { ch_names.push("googlechat"); }
                if ch.feishu.is_some() { ch_names.push("feishu"); }
                if ch.dingtalk.is_some() { ch_names.push("dingtalk"); }
                if ch.wecom.is_some() { ch_names.push("wecom"); }
                if ch.wechat.is_some() { ch_names.push("wechat"); }
                if ch.qq.is_some() { ch_names.push("qq"); }
                if ch.line.is_some() { ch_names.push("line"); }
                if ch.zalo.is_some() { ch_names.push("zalo"); }
                if ch.matrix.is_some() { ch_names.push("matrix"); }
                let ch_count = ch_names.len();
                if ch_count > 0 {
                    kv(
                        "Channels:",
                        &format!("{} ({})", ch_names.join(", "), ch_count),
                    );
                } else {
                    kv("Channels:", &dim("none configured"));
                }

                // Agents
                let agent_count = cfg.agents.list.len();
                if agent_count > 0 {
                    let first = &cfg.agents.list[0];
                    let model = first
                        .model
                        .as_ref()
                        .and_then(|m| m.primary.as_deref())
                        .or_else(|| {
                            cfg.agents
                                .defaults
                                .model
                                .as_ref()
                                .and_then(|m| m.primary.as_deref())
                        })
                        .unwrap_or("--");
                    if agent_count == 1 {
                        kv(
                            "Agent:",
                            &format!("{} (model: {})", first.id, model),
                        );
                    } else {
                        kv(
                            "Agents:",
                            &format!(
                                "{} + {} more (model: {})",
                                first.id,
                                agent_count - 1,
                                model
                            ),
                        );
                    }
                } else {
                    // agents.list empty 鈥?default "main" agent is auto-synthesized
                    let model = cfg.agents
                        .defaults
                        .model
                        .as_ref()
                        .and_then(|m| m.primary.as_deref())
                        .unwrap_or("--");
                    kv("Agent:", &format!("main (default, model: {})", model));
                }

                // Port info when gateway is running
                if running {
                    kv("Port:", &format!("{}", cfg.gateway.port));
                }
            }
            Err(e) => {
                kv("Config:", &red(&format!("error \u{2014} {e:#}")));
            }
        }

        // Tools
        let (avail, total) = super::tools::tools_count();
        let summary = super::tools::tools_summary_line();
        kv("Tools:", &format!("{}/{} — {}", avail, total, summary));

        println!();
    }
    Ok(())
}

pub async fn cmd_health(args: HealthArgs) -> Result<()> {
    if args.json {
        println!("{}", serde_json::json!({"status": "ok"}));
    } else {
        println!("OK");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TUI
// ---------------------------------------------------------------------------

struct TuiState {
    /// "running (pid XXXX)" or "stopped"
    gateway_status: String,
    gateway_port: u16,
    /// (id, model, sessions) -- session count is "--" (requires live gateway
    /// query)
    agents: Vec<(String, String, String)>,
    /// Last 10 log lines
    logs: Vec<String>,
}

impl TuiState {
    fn load() -> Self {
        let gateway_status = gateway_status_str();

        let (gateway_port, agents) = match config::load() {
            Ok(cfg) => {
                let port = cfg.gateway.port;
                let list = cfg
                    .agents
                    .list
                    .iter()
                    .map(|a| {
                        let model = a
                            .model
                            .as_ref()
                            .and_then(|m| m.primary.as_deref())
                            .unwrap_or("--")
                            .to_string();
                        (a.id.clone(), model, "--".to_string())
                    })
                    .collect();
                (port, list)
            }
            Err(_) => (18888, vec![]),
        };

        let logs = read_last_log_lines(10);

        TuiState {
            gateway_status,
            gateway_port,
            agents,
            logs,
        }
    }
}

fn gateway_status_str() -> String {
    let pid_path = gateway_pid_file();
    let pid_str = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return "stopped".to_string(),
    };
    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => return "stopped (bad PID file)".to_string(),
    };
    let alive = crate::sys::process_alive(pid);

    if alive {
        format!("running (pid {pid})")
    } else {
        let _ = std::fs::remove_file(&pid_path);
        format!("stopped (stale pid {pid})")
    }
}

fn read_last_log_lines(n: usize) -> Vec<String> {
    let log_path = config::loader::log_file();
    match std::fs::read_to_string(&log_path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].iter().map(|s| s.to_string()).collect()
        }
        Err(_) => vec!["(no gateway.log found)".to_string()],
    }
}

pub async fn cmd_tui(_args: TuiArgs) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_tui_loop(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_tui_loop(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    let mut state = TuiState::load();

    loop {
        terminal.draw(|f| draw_ui(f, &state))?;

        if event::poll(std::time::Duration::from_millis(500))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => break,
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    state = TuiState::load();
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn draw_ui(f: &mut ratatui::Frame, state: &TuiState) {
    let area = f.area();

    // Split: gateway(3) / agents(dynamic) / logs(dynamic) / help(1)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(area);

    // -- Gateway status --
    let running = state.gateway_status.starts_with("running");
    let status_color = if running { Color::Green } else { Color::Red };
    let gw_text = Line::from(vec![
        Span::raw("  Status: "),
        Span::styled(
            &state.gateway_status,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("   Port: {}", state.gateway_port)),
    ]);
    let gw_para =
        Paragraph::new(gw_text).block(Block::default().title(" Gateway ").borders(Borders::ALL));
    f.render_widget(gw_para, chunks[0]);

    // -- Agent list --
    let header = ListItem::new(Line::from(vec![Span::styled(
        format!("{:<20} {:<36} {}", "ID", "Model", "Sessions"),
        Style::default().add_modifier(Modifier::BOLD),
    )]));
    let mut items: Vec<ListItem> = vec![header];
    if state.agents.is_empty() {
        items.push(ListItem::new("  (no config loaded)"));
    } else {
        for (id, model, sessions) in &state.agents {
            items.push(ListItem::new(format!(
                "{:<20} {:<36} {}",
                id, model, sessions
            )));
        }
    }
    let agent_list =
        List::new(items).block(Block::default().title(" Agents ").borders(Borders::ALL));
    f.render_widget(agent_list, chunks[1]);

    // -- Logs --
    let log_items: Vec<ListItem> = if state.logs.is_empty() {
        vec![ListItem::new("  (no log entries)")]
    } else {
        state
            .logs
            .iter()
            .map(|l| ListItem::new(l.as_str()))
            .collect()
    };
    let log_list = List::new(log_items).block(
        Block::default()
            .title(" Recent Logs ")
            .borders(Borders::ALL),
    );
    f.render_widget(log_list, chunks[2]);

    // -- Help bar --
    let help = Paragraph::new(Line::from(vec![
        Span::styled(" q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit   "),
        Span::styled("r", Style::default().fg(Color::Yellow)),
        Span::raw(" refresh"),
    ]));
    f.render_widget(help, chunks[3]);
}
