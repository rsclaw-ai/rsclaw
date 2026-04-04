//! OpenCode client - ACP HTTP mode with connection pooling

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result};
use reqwest::Client;
use tokio::{process::Command, time::timeout};

pub struct OpenCodeClient {
    cwd: PathBuf,
    port: u16,
    child: Option<tokio::process::Child>,
    http_client: Client,
    started: bool,
}

impl OpenCodeClient {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            port: 19898,
            child: None,
            http_client: Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_default(),
            started: false,
        }
    }

    pub async fn run(&mut self, opencode_path: &str, prompt: &str) -> Result<String> {
        if !self.started {
            self.spawn(opencode_path).await?;
            self.started = true;
        }
        self.send_message(prompt).await
    }

    async fn spawn(&mut self, opencode_path: &str) -> Result<()> {
        tracing::info!("[OpenCode ACP] Starting server on port {}", self.port);

        let mut child = Command::new(opencode_path)
            .args(["acp", "--port", &self.port.to_string()])
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn OpenCode")?;

        for i in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let url = format!("http://127.0.0.1:{}/health", self.port);
            if self.http_client.get(&url).send().await.is_ok() {
                tracing::info!("[OpenCode ACP] Server ready after {} attempts", i + 1);
                self.child = Some(child);
                return Ok(());
            }
        }

        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!("OpenCode exited: {:?}", status);
        }
        anyhow::bail!("OpenCode server failed to start")
    }

    async fn send_message(&mut self, prompt: &str) -> Result<String> {
        let url = format!("http://127.0.0.1:{}/message", self.port);
        tracing::info!("[OpenCode ACP] Sending: {}", prompt);

        let response = timeout(
            Duration::from_secs(180),
            self.http_client
                .post(&url)
                .json(&serde_json::json!({ "text": prompt }))
                .send(),
        )
        .await
        .context("Request timed out")?
        .context("Failed to send")?;

        let result = response.text().await.context("Failed to read response")?;
        tracing::debug!("[OpenCode ACP] Response: {}", result);
        Ok(format!("[OpenCode ACP completed]\n\n{}", result))
    }

    pub async fn is_available(&self, opencode_path: &str) -> bool {
        Command::new(opencode_path)
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
        self.started = false;
    }
}

impl Drop for OpenCodeClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}
