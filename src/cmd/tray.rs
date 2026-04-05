use anyhow::Result;

// ---------------------------------------------------------------------------
// System tray (feature = "tray")
// ---------------------------------------------------------------------------

#[cfg(feature = "tray")]
pub fn cmd_tray() -> Result<()> {
    use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::{
        TrayIcon, TrayIconBuilder,
        menu::MenuEvent as TrayMenuEvent,
        Icon,
    };

    let menu = Menu::new();

    let status_item = MenuItem::new("Status: checking...", false, None);
    let separator1 = PredefinedMenuItem::separator();
    let start_item = MenuItem::new("Start Gateway", true, None);
    let stop_item = MenuItem::new("Stop Gateway", true, None);
    let restart_item = MenuItem::new("Restart Gateway", true, None);
    let separator2 = PredefinedMenuItem::separator();
    let logs_item = MenuItem::new("View Logs", true, None);
    let doctor_item = MenuItem::new("Doctor", true, None);
    let config_item = MenuItem::new("Open Config", true, None);
    let separator3 = PredefinedMenuItem::separator();
    let version_item = MenuItem::new(
        format!("rsclaw {}", env!("RSCLAW_BUILD_VERSION")),
        false,
        None,
    );
    let quit_item = MenuItem::new("Quit", true, None);

    menu.append_items(&[
        &status_item,
        &separator1,
        &start_item,
        &stop_item,
        &restart_item,
        &separator2,
        &logs_item,
        &doctor_item,
        &config_item,
        &separator3,
        &version_item,
        &quit_item,
    ])?;

    let icon = load_icon();

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("RsClaw Gateway")
        .with_icon(icon)
        .build()?;

    // Update initial status
    update_status(&status_item, &start_item, &stop_item, &restart_item);

    // IDs for matching
    let start_id = start_item.id().clone();
    let stop_id = stop_item.id().clone();
    let restart_id = restart_item.id().clone();
    let logs_id = logs_item.id().clone();
    let doctor_id = doctor_item.id().clone();
    let config_id = config_item.id().clone();
    let quit_id = quit_item.id().clone();

    // Run the event loop
    let event_loop = winit_or_platform_loop();
    let mut last_check = std::time::Instant::now();

    loop {
        // Poll menu events
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            let id = event.id;
            if id == start_id {
                let _ = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["gateway", "start"])
                    .spawn();
                std::thread::sleep(std::time::Duration::from_secs(1));
                update_status(&status_item, &start_item, &stop_item, &restart_item);
            } else if id == stop_id {
                let _ = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["gateway", "stop"])
                    .status();
                std::thread::sleep(std::time::Duration::from_millis(500));
                update_status(&status_item, &start_item, &stop_item, &restart_item);
            } else if id == restart_id {
                let _ = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["gateway", "stop"])
                    .status();
                std::thread::sleep(std::time::Duration::from_millis(500));
                let _ = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["gateway", "start"])
                    .spawn();
                std::thread::sleep(std::time::Duration::from_secs(1));
                update_status(&status_item, &start_item, &stop_item, &restart_item);
            } else if id == logs_id {
                open_terminal_with(&["logs", "--follow"]);
            } else if id == doctor_id {
                open_terminal_with(&["doctor", "--fix"]);
            } else if id == config_id {
                open_config();
            } else if id == quit_id {
                break;
            }
        }

        // Periodic status refresh (every 10s)
        if last_check.elapsed() > std::time::Duration::from_secs(10) {
            update_status(&status_item, &start_item, &stop_item, &restart_item);
            last_check = std::time::Instant::now();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    Ok(())
}

#[cfg(feature = "tray")]
fn update_status(
    status_item: &muda::MenuItem,
    start_item: &muda::MenuItem,
    stop_item: &muda::MenuItem,
    restart_item: &muda::MenuItem,
) {
    let pid_file = crate::cmd::gateway::gateway_pid_file();
    let running = pid_file.exists()
        && std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|pid| crate::sys::process_alive(pid));

    if running {
        status_item.set_text("Status: Running");
        start_item.set_enabled(false);
        stop_item.set_enabled(true);
        restart_item.set_enabled(true);
    } else {
        status_item.set_text("Status: Stopped");
        start_item.set_enabled(true);
        stop_item.set_enabled(false);
        restart_item.set_enabled(false);
    }
}

#[cfg(feature = "tray")]
fn load_icon() -> tray_icon::Icon {
    // Embedded 16x16 RGBA icon (orange "Rs" on transparent)
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    // Fill with orange (#e8590c) circle
    let center = size as f32 / 2.0;
    let radius = center - 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let idx = ((y * size + x) * 4) as usize;
            if dx * dx + dy * dy <= radius * radius {
                rgba[idx] = 0xe8;     // R
                rgba[idx + 1] = 0x59; // G
                rgba[idx + 2] = 0x0c; // B
                rgba[idx + 3] = 0xff; // A
            }
        }
    }

    tray_icon::Icon::from_rgba(rgba, size, size).expect("failed to create icon")
}

#[cfg(feature = "tray")]
fn open_terminal_with(args: &[&str]) {
    let exe = std::env::current_exe().unwrap();

    #[cfg(target_os = "macos")]
    {
        let cmd = format!("{} {}", exe.display(), args.join(" "));
        let _ = std::process::Command::new("osascript")
            .args(["-e", &format!("tell application \"Terminal\" to do script \"{}\"", cmd)])
            .spawn();
    }

    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "cmd", "/k"])
            .arg(exe)
            .args(args)
            .spawn();
    }

    #[cfg(target_os = "linux")]
    {
        // Try common terminal emulators
        for term in &["x-terminal-emulator", "gnome-terminal", "xterm"] {
            if std::process::Command::new(term)
                .args(["--", exe.to_str().unwrap()])
                .args(args)
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
}

#[cfg(feature = "tray")]
fn open_config() {
    let base = crate::config::loader::base_dir();
    let config_path = base.join("rsclaw.json5");

    if !config_path.exists() {
        return;
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&config_path).spawn();
    }

    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("notepad").arg(&config_path).spawn();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&config_path).spawn();
    }
}

#[cfg(feature = "tray")]
fn winit_or_platform_loop() {
    // On macOS, we need to initialize the app properly for the menu bar.
    // tray-icon handles this internally, but we need the event loop running.
    #[cfg(target_os = "macos")]
    {
        // macOS requires NSApplication to be initialized for tray icons
        // tray-icon does this internally
    }
}

#[cfg(not(feature = "tray"))]
pub fn cmd_tray() -> Result<()> {
    anyhow::bail!(
        "tray feature not enabled. Rebuild with: cargo build --release --features tray\n\
         Or use the PowerShell tray instead: powershell -File scripts/rsclaw-tray.ps1"
    );
}
