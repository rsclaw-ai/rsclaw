use anyhow::Result;

pub async fn cmd_docs(query: Vec<String>) -> Result<()> {
    let base = "https://docs.openclaw.ai";

    let url = if query.is_empty() {
        base.to_string()
    } else {
        let q = query.join("+");
        format!("{base}/search?q={q}")
    };

    println!("{url}");

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        // CREATE_NO_WINDOW = 0x08000000 — without this the `cmd /C start`
        // wrapper flashes a console window for ~50ms before handing off
        // to ShellExecute (the thing that actually opens the browser).
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", &url])
            .creation_flags(0x08000000)
            .spawn();
    }

    Ok(())
}
