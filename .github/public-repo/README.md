# rsclaw

OpenClaw-compatible multi-agent framework built in Rust.

## Install

### macOS (Apple Silicon)

```bash
curl -LO https://github.com/rsclaw-ai/rsclaw/releases/latest/download/rsclaw-aarch64-apple-darwin.tar.gz
tar xzf rsclaw-aarch64-apple-darwin.tar.gz
chmod +x rsclaw
sudo mv rsclaw /usr/local/bin/
```

### macOS (Intel)

```bash
curl -LO https://github.com/rsclaw-ai/rsclaw/releases/latest/download/rsclaw-x86_64-apple-darwin.tar.gz
tar xzf rsclaw-x86_64-apple-darwin.tar.gz
chmod +x rsclaw
sudo mv rsclaw /usr/local/bin/
```

### Linux (x86_64)

```bash
curl -LO https://github.com/rsclaw-ai/rsclaw/releases/latest/download/rsclaw-x86_64-unknown-linux-gnu.tar.gz
tar xzf rsclaw-x86_64-unknown-linux-gnu.tar.gz
chmod +x rsclaw
sudo mv rsclaw /usr/local/bin/
```

### Linux (aarch64)

```bash
curl -LO https://github.com/rsclaw-ai/rsclaw/releases/latest/download/rsclaw-aarch64-unknown-linux-gnu.tar.gz
tar xzf rsclaw-aarch64-unknown-linux-gnu.tar.gz
chmod +x rsclaw
sudo mv rsclaw /usr/local/bin/
```

### Verify

```bash
rsclaw --version
```

## Quick Start

```bash
# Initialize configuration
rsclaw setup

# Start the gateway
rsclaw gateway
```

## Checksums

Each release includes a `SHA256SUMS.txt` file. Verify your download:

```bash
sha256sum -c SHA256SUMS.txt
```

## License

Dual-licensed under MIT and Apache-2.0.
