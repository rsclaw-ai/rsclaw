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
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", &url])
            .spawn();
    }

    Ok(())
}
