use anyhow::Result;

// ---------------------------------------------------------------------------
// Memory tier detection (AGENTS.md §18 + §30)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    /// ≤ ~1.1 GB total RAM
    Low,
    /// ~2 GB
    Standard,
    /// 4 GB+
    High,
}

pub fn detect_memory_tier() -> MemoryTier {
    let total_kb = sys_info::mem_info().map(|m| m.total).unwrap_or(0);

    match total_kb {
        0..=1_200_000 => MemoryTier::Low,
        1_200_001..=2_500_000 => MemoryTier::Standard,
        _ => MemoryTier::High,
    }
}

// ---------------------------------------------------------------------------
// Cross-platform process utilities
// ---------------------------------------------------------------------------

/// Check whether a process with the given PID is still alive.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Send a termination signal to the process (SIGTERM on Unix, taskkill on
/// Windows).
pub fn process_terminate(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            anyhow::bail!("failed to send SIGTERM to process {pid}");
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        // Use /F (force) and /T (tree) -- without /F, taskkill only sends
        // WM_CLOSE which has no effect on windowless background processes.
        let status = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status()?;
        if !status.success() {
            anyhow::bail!("taskkill failed for process {pid}");
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        anyhow::bail!("process termination not supported on this platform");
    }
}

pub fn build_runtime(_tier: MemoryTier) -> Result<tokio::runtime::Runtime> {
    // Use multi_thread with 1 worker and larger stack to avoid stack overflow
    // in debug builds with large code size.
    // This is a workaround for Windows debug build stack size issues.
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .thread_stack_size(8 * 1024 * 1024) // 8MB stack per thread
        .build()?)
}
