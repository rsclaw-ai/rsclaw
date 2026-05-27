# RsClaw

> **Eine KI-Agenten-Engine, die sich erinnert, lernt und über Maschinen hinweg routet.**
> 15MB Rust-Binary · A2A Hub-Spoke-Flotte · Dreistufiges Gedächtnis · Vektor + BM25 Wissensbasis · 13 Kanäle · 15 LLM-Anbieter · OpenClaw Drop-in-Ersatz.

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

[🇺🇸 English](../../README.md) · [🇨🇳 中文](README_cn.md) · [🇯🇵 日本語](README_ja.md) · [🇰🇷 한국어](README_ko.md) · [ไทย](README_th.md) · [Tiếng Việt](README_vi.md) · [Français](README_fr.md) · **Deutsch** · [Español](README_es.md) · [Русский](README_ru.md)

<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

Die meisten KI-Agenten sind zustandslose Prozesse, die an einen Chat gebunden sind. **RsClaw ist eine Flotte**: Jeder Knoten speichert strukturiertes Gedächtnis, indiziert eine private Wissensbasis und spricht [Google A2A v1.0](https://a2a-protocol.org/). Eine Anfrage von Ihrem Laptop kann an einen GPU-Spoke für Bildgenerierung, einen Flottenknoten für RAG und einen Remote-Partner-Agent für Spezialaufgaben verteilt werden — alles als ein gestreamter Antwortstrom.

15 MB, ~20 MB RAM, statisches Single-Binary. Reines Rust. Kein Node, kein Python.

💬 [Community beitreten](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Installation

```bash
# Homebrew (macOS / Linux) — empfohlen
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # Desktop-App (macOS DMG)

# Cargo
cargo install rsclaw

# One-Liner (macOS / Linux)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

```bash
rsclaw setup          # ~/.rsclaw/ initialisieren
rsclaw onboard        # Interaktiver Assistent: Provider, Channel, Embedder
rsclaw start
```

---

## A2A — Flotten-Level Agent-zu-Agent-Routing

RsClaw implementiert die vollständige [Google A2A v1.0-Spezifikation](https://a2a-protocol.org/latest/specification/) — Streaming, Push-Benachrichtigungen, Task-Persistenz, Cancel, INPUT_REQUIRED-Interrupts, alle 11 JSON-RPC-Methoden — plus ein erstklassiges **Hub-Spoke-Relay**, das eine heterogene Maschinenflotte zu einem logischen Agent verschmilzt. Jeder Spoke hält eine persistente ausgehende WebSocket-Verbindung zum Hub — keine eingehenden Ports auf den Spokes erforderlich. Funktioniert hinter NAT, Firewalls und chinesischer Festlandverbindung.

→ Vollständige Protokollfläche, Hub-Spoke-Betrieb, Identität & ACL, Tunnel-Rezepte: [docs/a2a.md](../a2a.md).

---

## Gedächtnis — dreistufig, zerfallsbewusst, hybride Suche

Langzeitgedächtnis, das Sie nie manuell verwalten müssen. Jede relevante Konversationsrunde extrahiert die Laufzeit dauerhafte Signale in strukturierte Docs (entity / preference / fact / procedure / relationship / lesson / failure), klassifiziert sie in **Core / Working / Peripheral**-Stufen mit per-Stufe **Weibull-Streck-Exponential**-Zerfall und ruft sie über **hybride BM25 + Vektorsuche** (RRF-Fusion) zurück. Originalsprache wird beibehalten — Deutsch rein, Deutsch raus.

→ Stufenmathematik, Extraktor-Prompt-Design, Embedder-Wechsel, HTTP-API: [docs/memory.md](../memory.md).

---

## Wissensbasis — verwaltetes RAG, OOXML-Ingest

First-Class-Speicher für Projektdokumente, Code, Verträge, alles, was der Agent zitieren statt zusammenfassen soll. Collections sind Tag-Veneer über einem gemeinsamen Index. OOXML (.docx / .xlsx / .pptx), PDF, HTML, Markdown, Quellcode werden bei Ingest kanonisiert. Hybride Suche (BM25 + Vektor + RRF + MMR), Antworten zitieren `doc_id` + Offset.

```bash
rsclaw knowledge ingest <pfad> --collection vertraege
rsclaw knowledge search "Q3 Umsatzprognose" --collection finanzbericht
```

→ Collections-Modell, Ingest-Pipeline, Such-Pipeline, CLI / HTTP-API: [docs/kb.md](../kb.md).

---

## Kernfunktionen

- **13+ Nachrichtenkanäle**: Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, benutzerdefinierter Webhook
- **15+ LLM-Anbieter**: OpenAI, Anthropic, Gemini, DeepSeek, Qwen, Doubao, Ollama, etc.
- **Vier Agent-Lebenszeiten**: Main / Named / Sub / Task; vier Backends: Native Rust / Claude Code / OpenCode / ACP
- **36 eingebaute Tools**: Dateien, Shell, Web, Browser-Automatisierung (CDP), Image / Video, STT / TTS, computer_use, Cron, A2A, Memory, KB
- **40+ vorab geparste Befehle**: zero-token, Sub-Millisekunden-Antwort
- **Plugin-Dual-Runtime**: wasm (sandboxed) + node/bun/deno (OpenClaw-kompatibel)
- **Exec-Sicherheit**: 50+ Deny-Patterns, sandboxed Write, signierte Skills

---

## Migration von OpenClaw

```bash
openclaw gateway stop
rsclaw setup          # Erkennt ~/.openclaw/, bietet One-Click-Import
rsclaw start
```

`~/.openclaw/` wird niemals modifiziert. Beide können parallel laufen (Ports 18888 vs 18789).

---

## Lizenz

Doppelt lizenziert unter **MIT** OR **Apache-2.0**. Frei nutzbar in persönlichen, kommerziellen, Enterprise-, SaaS- oder proprietären Produkten. Modifikation und Weiterverteilung ohne Copyleft-Verpflichtung. Details: [Englisches README](../../README.md#license).

🦀 Gebaut mit Rust. Inspiriert von der OpenClaw-Community.
