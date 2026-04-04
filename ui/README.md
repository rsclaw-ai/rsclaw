# RsClaw Desktop

Tauri-based desktop application for the RsClaw AI gateway.

## Features

- Native desktop app for macOS, Linux, and Windows
- Built-in gateway management (start/stop/status)
- Chat interface with full agent runtime support
- Configuration editor with provider testing
- Agent management, cron tasks, and skills manager
- Channel pairing approval
- Close-to-tray with background gateway

## Tech Stack

- **Shell**: Tauri v1 (Rust)
- **Frontend**: Next.js + React
- **Gateway**: Bundled as sidecar binary (`rsclaw-cli`)

## Development

```bash
# Install dependencies
yarn install

# Dev mode (hot-reload)
yarn export:dev
npx tauri dev

# Build release
yarn export
npx tauri build
```

## Project Structure

```
app/              # Next.js frontend
  components/     # React components (chat, sidebar, control panel)
  client/         # API clients (OpenAI-compatible, gateway)
  lib/            # Shared utilities (rsclaw-api, i18n)
src-tauri/        # Tauri shell
  src/main.rs     # Tauri commands (config, provider test, cron, skills)
  binaries/       # Sidecar binary (rsclaw-cli)
```

## License

AGPL-3.0 -- see [LICENSE](../LICENSE)
