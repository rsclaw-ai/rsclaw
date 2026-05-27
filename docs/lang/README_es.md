# RsClaw

> **Un motor de agentes de IA que recuerda, aprende y enruta entre máquinas.**
> Binario Rust de 15MB · Flota A2A hub-spoke · Memoria de tres niveles · Base de conocimiento vector + BM25 · 13 canales · 15 proveedores LLM · Reemplazo drop-in de OpenClaw.

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

[🇺🇸 English](../../README.md) · [🇨🇳 中文](README_cn.md) · [🇯🇵 日本語](README_ja.md) · [🇰🇷 한국어](README_ko.md) · [ไทย](README_th.md) · [Tiếng Việt](README_vi.md) · [Français](README_fr.md) · [Deutsch](README_de.md) · **Español** · [Русский](README_ru.md)

<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

La mayoría de los agentes IA son procesos sin estado pegados a una caja de chat. **RsClaw es una flota**: cada nodo persiste memoria estructurada, indexa una base de conocimiento privada y habla [Google A2A v1.0](https://a2a-protocol.org/). Una petición escrita en tu portátil puede expandirse a un spoke GPU para generación de imágenes, un nodo de flota para RAG y un agente remoto socio para una tarea especializada — todo regresando como un único flujo de respuesta.

15 MB, ~20 MB RAM, binario estático único. Rust puro. Sin Node, sin Python.

💬 [Únete a la comunidad](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Instalación

```bash
# Homebrew (macOS / Linux) — recomendado
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # App de escritorio (macOS DMG)

# Cargo
cargo install rsclaw

# Una sola línea (macOS / Linux)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

```bash
rsclaw setup          # inicializar ~/.rsclaw/
rsclaw onboard        # asistente interactivo: proveedor, canal, embedder
rsclaw start
```

---

## A2A — Enrutamiento agente-a-agente nivel flota

RsClaw implementa la especificación completa de [Google A2A v1.0](https://a2a-protocol.org/latest/specification/) — streaming, notificaciones push, persistencia de tareas, cancel, interrupciones INPUT_REQUIRED, los 11 métodos JSON-RPC — más un **relay hub-spoke de primera clase** que fusiona una flota de máquinas heterogéneas en un solo agente lógico. Cada spoke mantiene una conexión WebSocket saliente persistente al hub — sin puertos entrantes requeridos en los spokes. Funciona detrás de NAT, firewalls y condiciones de red de China continental.

→ Superficie completa del protocolo, operaciones hub-spoke, identidad y ACL, recetas de túnel: [docs/a2a.md](../a2a.md).

---

## Memoria — tres niveles, consciente del decaimiento, recuperación híbrida

Memoria a largo plazo que nunca tienes que gestionar manualmente. Cada turno relevante, el runtime extrae señales duraderas en docs estructurados (entity / preference / fact / procedure / relationship / lesson / failure), los clasifica en niveles **Core / Working / Peripheral** con decaimiento **Weibull exponencial estirado** por nivel, y los recupera vía **búsqueda híbrida BM25 + vector** (fusionada por RRF). Idioma original preservado — español entra, español sale.

→ Matemáticas de niveles, diseño del prompt extractor, intercambio de embedder, API HTTP: [docs/memory.md](../memory.md).

---

## Base de conocimiento — RAG gestionado, ingesta OOXML

Almacén persistente de primera clase para documentos de proyecto, código, contratos — cualquier cosa que quieras que el agente **cite en lugar de resumir** desde el entrenamiento. Las colecciones son una capa de etiquetas sobre un índice compartido. OOXML (.docx / .xlsx / .pptx), PDF, HTML, Markdown, código fuente se canonicalizan al ingerir. Búsqueda híbrida (BM25 + vector + RRF + MMR); las respuestas citan `doc_id` + offset.

```bash
rsclaw knowledge ingest <ruta> --collection contratos
rsclaw knowledge search "previsión ingresos Q3" --collection finanzas
```

→ Modelo de colecciones, pipeline de ingesta, búsqueda, API: [docs/kb.md](../kb.md).

---

## Características principales

- **13+ canales de mensajería**: Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, webhook personalizado
- **15+ proveedores LLM**: OpenAI, Anthropic, Gemini, DeepSeek, Qwen, Doubao, Ollama, etc.
- **Cuatro ciclos de vida de agente**: Main / Named / Sub / Task; cuatro backends: Native Rust / Claude Code / OpenCode / ACP
- **36 herramientas integradas**: archivos, shell, web, automatización de navegador (CDP), imagen / video, STT / TTS, computer_use, cron, A2A, memoria, KB
- **40+ comandos pre-parseados**: cero tokens, respuesta sub-milisegundo
- **Plugin dual-runtime**: wasm (sandbox) + node/bun/deno (compatible con OpenClaw)
- **Seguridad de ejecución**: 50+ patrones de deny, escritura sandbox, skills firmadas

---

## Migración desde OpenClaw

```bash
openclaw gateway stop
rsclaw setup          # detecta ~/.openclaw/, ofrece importación en un clic
rsclaw start
```

`~/.openclaw/` nunca se modifica. Ambos pueden correr en paralelo (puertos 18888 vs 18789).

---

## Licencia

Doble licencia bajo **MIT** OR **Apache-2.0**. Uso libre en productos personales, comerciales, empresariales, SaaS o propietarios. Modificación y redistribución sin obligación copyleft. Detalles: [README en inglés](../../README.md#license).

🦀 Construido en Rust. Inspirado por la comunidad OpenClaw.
