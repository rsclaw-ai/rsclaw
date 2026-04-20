# RsClaw

**AI Agent Engine with long-term memory and self-learning — one 15MB binary, 13 channels, 15 LLM providers, A2A cross-machine orchestration, browser automation, all in pure Rust. Your AI never forgets and gets better the more you use it.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)
[![Binary Size](https://img.shields.io/badge/binary-~15MB-green)]()

[English](../../README.md) | [中文](README_cn.md) | [日本語](README_ja.md) | [한국어](README_ko.md) | [ไทย](README_th.md) | [Tiếng Việt](README_vi.md) | [Français](README_fr.md) | [Deutsch](README_de.md) | **Español** | [Русский](README_ru.md)

RsClaw es una reescritura completa de [OpenClaw](https://github.com/openclaw/openclaw) en Rust, ofreciendo el mismo protocolo de pasarela IA multiagente con un inicio 10x mas rapido, un tamano 10x menor y cero dependencias de Node.js.


<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

💬 [Join Community](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Caracteristicas principales

- **13+ canales de mensajeria** -- Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, Webhook personalizado
- **15 proveedores LLM** -- OpenAI, Anthropic, Google Gemini, DeepSeek, Qwen, Ollama, etc.
- **32 herramientas integradas** -- Archivos, Shell, Busqueda web/Navegador, Generacion de imagenes, Memoria, Mensajeria, cron, A2A
- **40+ comandos PreParse** -- Evitan el LLM, cero tokens, respuesta sub-milisegundo
- **Automatizacion de navegador CDP** -- Control headless Chrome integrado (20 acciones)
- **Protocolo A2A** -- Google A2A v0.3 (colaboracion de agentes entre redes)
- **Seguridad de ejecucion** -- Reglas deny/confirm/allow, 50+ patrones de rechazo

## Instalacion rapida

```bash
# macOS / Linux (deteccion automatica de plataforma)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

### Compilar desde el codigo fuente

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --release
```

## Inicio rapido

```bash
rsclaw onboard    # Asistente de configuracion
rsclaw start      # Iniciar la pasarela
rsclaw status     # Verificar estado
rsclaw doctor --fix  # Verificacion de salud
```

## Plataformas soportadas

macOS (x86_64, ARM64), Linux (x86_64, ARM64), Windows (x86_64, ARM64)

## Documentacion

Documentacion completa en [README.md](../../README.md) (中文) o [README_en.md](../../README.md) (English).

## Licencia

Este proyecto esta bajo la licencia [GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE).

Puede usar, modificar y distribuir este software libremente, pero cualquier version modificada (incluyendo servicios de red) debe ser de codigo abierto bajo la misma licencia.
