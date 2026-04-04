//! ACP stream implementations for stdin/stdout and subprocess I/O
//!
//! Key insight: tokio's Lines iterator has wake-up issues in multi-future
//! select! environments. ProcessReader uses manual BufReader polling instead.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::acp::jsonrpc::JsonRpcMessage;

/// NDJSON codec marker
pub struct NdJsonCodec;

impl NdJsonCodec {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NdJsonCodec {
    fn default() -> Self {
        Self::new()
    }
}

/// Read from stdin (when rsclaw runs as subprocess)
pub struct StdinReader {
    reader: BufReader<tokio::io::Stdin>,
}

impl StdinReader {
    pub fn new() -> Self {
        Self {
            reader: BufReader::new(tokio::io::stdin()),
        }
    }

    pub async fn read_message(&mut self) -> Result<Option<JsonRpcMessage>> {
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line).await {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    let line = line.trim();
                    if !line.is_empty() {
                        let msg =
                            serde_json::from_str(line).context("Failed to parse NDJSON message")?;
                        return Ok(Some(msg));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Default for StdinReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Write to stdout (when rsclaw runs as subprocess)
pub struct StdioWriter {
    writer: tokio::io::BufWriter<tokio::io::Stdout>,
}

impl StdioWriter {
    pub fn new() -> Self {
        Self {
            writer: tokio::io::BufWriter::new(tokio::io::stdout()),
        }
    }

    pub async fn write_message(&mut self, msg: &JsonRpcMessage) -> Result<()> {
        let line = serde_json::to_string(msg)?;
        // Single write for JSON + newline (like Python does)
        let mut combined = line.as_bytes().to_vec();
        combined.push(b'\n');
        self.writer.write_all(&combined).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

impl Default for StdioWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Combined stdin/stdout stream
pub struct NdJsonStream {
    pub reader: StdinReader,
    pub writer: StdioWriter,
}

impl NdJsonStream {
    pub fn new() -> Self {
        Self {
            reader: StdinReader::new(),
            writer: StdioWriter::new(),
        }
    }

    pub async fn next(&mut self) -> Result<Option<JsonRpcMessage>> {
        self.reader.read_message().await
    }

    pub async fn send(&mut self, msg: &JsonRpcMessage) -> Result<()> {
        self.writer.write_message(msg).await
    }
}

impl Default for NdJsonStream {
    fn default() -> Self {
        Self::new()
    }
}

/// Read from subprocess stdout with manual polling (avoids Lines wake-up
/// issues)
pub struct ProcessReader {
    reader: BufReader<tokio::process::ChildStdout>,
}

impl ProcessReader {
    pub async fn from_child(child: &mut tokio::process::Child) -> Result<Self> {
        let stdout = child.stdout.take().context("Child has no stdout")?;
        Ok(Self {
            reader: BufReader::new(stdout),
        })
    }

    /// Read one line with timeout-based polling
    pub async fn read_message(&mut self) -> Result<Option<JsonRpcMessage>> {
        let mut line = String::new();
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);

        loop {
            if start.elapsed() > timeout {
                anyhow::bail!("Timeout reading from subprocess");
            }

            line.clear();
            tokio::select! {
                result = self.reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => return Ok(None), // EOF
                        Ok(_) => {
                            let line = line.trim();
                            if !line.is_empty() {
                                match serde_json::from_str(line) {
                                    Ok(msg) => return Ok(Some(msg)),
                                    Err(e) => return Err(e.into()),
                                }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // No data available yet, continue polling
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    // Continue polling
                }
            }
        }
    }
}

/// Write to subprocess stdin
pub struct ProcessWriter {
    writer: tokio::io::BufWriter<tokio::process::ChildStdin>,
}

impl ProcessWriter {
    pub async fn from_child(child: &mut tokio::process::Child) -> Result<Self> {
        let stdin = child.stdin.take().context("Child has no stdin")?;
        Ok(Self {
            writer: tokio::io::BufWriter::new(stdin),
        })
    }

    /// Write message with single write operation (JSON + newline)
    pub async fn write_message(&mut self, msg: &JsonRpcMessage) -> Result<()> {
        let line = serde_json::to_string(msg)?;
        let mut combined = line.as_bytes().to_vec();
        combined.push(b'\n');
        self.writer.write_all(&combined).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

/// Combined subprocess stream
pub struct SubprocessStream {
    pub reader: ProcessReader,
    pub writer: ProcessWriter,
}

impl SubprocessStream {
    pub async fn new(child: &mut tokio::process::Child) -> Result<Self> {
        Ok(Self {
            reader: ProcessReader::from_child(child).await?,
            writer: ProcessWriter::from_child(child).await?,
        })
    }

    pub async fn next(&mut self) -> Result<Option<JsonRpcMessage>> {
        self.reader.read_message().await
    }

    pub async fn send(&mut self, msg: &JsonRpcMessage) -> Result<()> {
        self.writer.write_message(msg).await
    }
}
