# RsClaw

> **AI-агент движок, который помнит, обучается и маршрутизирует между машинами.**
> Rust-бинарь 15МБ · A2A hub-spoke флот · Трёхуровневая память · База знаний vector + BM25 · 13 каналов · 15 LLM-провайдеров · Drop-in замена OpenClaw.

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

[🇺🇸 English](../../README.md) · [🇨🇳 中文](README_cn.md) · [🇯🇵 日本語](README_ja.md) · [🇰🇷 한국어](README_ko.md) · [ไทย](README_th.md) · [Tiếng Việt](README_vi.md) · [Français](README_fr.md) · [Deutsch](README_de.md) · [Español](README_es.md) · **Русский**

<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

Большинство AI-агентов — это процессы без состояния, привязанные к чат-боксу. **RsClaw — это флот**: каждый узел сохраняет структурированную память, индексирует приватную базу знаний и говорит на [Google A2A v1.0](https://a2a-protocol.org/). Запрос с вашего ноутбука может разветвиться к GPU-споку для генерации изображений, узлу флота для RAG и удалённому партнёрскому агенту для специализированной задачи — всё возвращается как один стримящийся ответ.

15 МБ, ~20 МБ RAM, единый статический бинарь. Чистый Rust. Никакого Node, никакого Python.

💬 [Присоединиться к сообществу](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Установка

```bash
# Homebrew (macOS / Linux) — рекомендуется
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # десктопное приложение (macOS DMG)

# Cargo
cargo install rsclaw

# Одной строкой (macOS / Linux)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

```bash
rsclaw setup          # инициализировать ~/.rsclaw/
rsclaw onboard        # интерактивный мастер: провайдер, канал, эмбеддер
rsclaw start
```

---

## A2A — Маршрутизация агент-агент уровня флота

RsClaw полностью реализует [спецификацию Google A2A v1.0](https://a2a-protocol.org/latest/specification/) — streaming, push-уведомления, персистентность задач, cancel, прерывания INPUT_REQUIRED, все 11 JSON-RPC методов — плюс **first-class hub-spoke relay**, объединяющий разнородный флот машин в один логический агент. Каждый спок держит одну постоянную исходящую WebSocket-связь с хабом — входящие порты на споках не требуются. Работает за NAT, фаерволами и в сетевых условиях материкового Китая.

→ Полная поверхность протокола, операции hub-spoke, identity & ACL, рецепты туннеля: [docs/a2a.md](../a2a.md).

---

## Память — три уровня, с учётом затухания, гибридный recall

Долговременная память, которой никогда не нужно управлять вручную. На каждом релевантном ходу runtime извлекает устойчивые сигналы в структурированные docs (entity / preference / fact / procedure / relationship / lesson / failure), классифицирует их по уровням **Core / Working / Peripheral** с **Weibull-растянуто-экспоненциальным** затуханием на уровень, и recall'ит через **гибридный поиск BM25 + вектор** (фьюзинг RRF). Оригинальный язык сохраняется — русский на входе, русский на выходе.

→ Математика уровней, дизайн prompt'а экстрактора, смена эмбеддера, HTTP API: [docs/memory.md](../memory.md).

---

## База знаний — управляемый RAG, OOXML ingest

First-class персистентное хранилище для проектных документов, кода, контрактов — всего того, что вы хотите чтобы агент **цитировал, а не пересказывал** из обучения. Коллекции — это tag veneer над общим индексом. OOXML (.docx / .xlsx / .pptx), PDF, HTML, Markdown, исходный код канонизируются при ingest. Гибридный поиск (BM25 + вектор + RRF + MMR); ответы цитируют `doc_id` + offset.

```bash
rsclaw knowledge ingest <путь> --collection контракты
rsclaw knowledge search "прогноз выручки Q3" --collection финансы
```

→ Модель коллекций, ingest pipeline, поиск, API: [docs/kb.md](../kb.md).

---

## Ключевые возможности

- **13+ каналов сообщений**: Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, кастомный webhook
- **15+ LLM-провайдеров**: OpenAI, Anthropic, Gemini, DeepSeek, Qwen, Doubao, Ollama и т.д.
- **Четыре времени жизни агентов**: Main / Named / Sub / Task; четыре бэкенда: Native Rust / Claude Code / OpenCode / ACP
- **36 встроенных инструментов**: файлы, shell, web, автоматизация браузера (CDP), image / video, STT / TTS, computer_use, cron, A2A, память, KB
- **40+ pre-parsed команд**: ноль токенов, sub-миллисекундный ответ
- **Плагины с двойным runtime**: wasm (sandbox) + node/bun/deno (совместимость с OpenClaw)
- **Безопасность exec**: 50+ deny-паттернов, sandbox запись, подписанные skills

---

## Миграция с OpenClaw

```bash
openclaw gateway stop
rsclaw setup          # обнаруживает ~/.openclaw/, предлагает one-click import
rsclaw start
```

`~/.openclaw/` никогда не модифицируется. Оба могут работать параллельно (порты 18888 vs 18789).

---

## Лицензия

Двойная лицензия **MIT** OR **Apache-2.0**. Свободное использование в личных, коммерческих, корпоративных, SaaS или проприетарных продуктах. Модификация и распространение без обязательств copyleft. Детали: [английский README](../../README.md#license).

🦀 Построено на Rust. Вдохновлено сообществом OpenClaw.
