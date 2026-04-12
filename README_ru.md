# RsClaw

**High-performance Multi-Agent AI gateway with native OpenClaw A2A orchestration.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)
[![Binary Size](https://img.shields.io/badge/binary-~12MB-green)]()

[English](README.md) | [中文](README_cn.md) | [日本語](README_ja.md) | [한국어](README_ko.md) | [ไทย](README_th.md) | [Tiếng Việt](README_vi.md) | [Français](README_fr.md) | [Deutsch](README_de.md) | [Español](README_es.md) | **Русский**

RsClaw -- это полная переработка [OpenClaw](https://github.com/openclaw/openclaw) на Rust, предоставляющая тот же протокол мультиагентного ИИ-шлюза с 10-кратным ускорением запуска, 10-кратным уменьшением размера и нулевой зависимостью от Node.js.

---

## Основные возможности

- **13+ каналов сообщений** -- Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, пользовательский Webhook
- **15 провайдеров LLM** -- OpenAI, Anthropic, Google Gemini, DeepSeek, Qwen, Ollama и др.
- **32 встроенных инструмента** -- Файлы, Shell, Веб-поиск/Браузер, Генерация изображений, Память, Сообщения, cron, A2A
- **40+ команд PreParse** -- Обходят LLM, ноль токенов, ответ менее миллисекунды
- **Автоматизация браузера CDP** -- Встроенное управление headless Chrome (20 действий)
- **Протокол A2A** -- Google A2A v0.3 (межсетевое взаимодействие агентов)
- **Безопасность выполнения** -- Правила deny/confirm/allow, 50+ шаблонов отказа

## Быстрая установка

```bash
# macOS / Linux (автоопределение платформы)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

### Сборка из исходного кода

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --release
```

## Быстрый старт

```bash
rsclaw onboard    # Мастер настройки
rsclaw start      # Запуск шлюза
rsclaw status     # Проверка статуса
rsclaw doctor --fix  # Проверка здоровья
```

## Поддерживаемые платформы

macOS (x86_64, ARM64), Linux (x86_64, ARM64), Windows (x86_64, ARM64)

## Документация

Полная документация в [README.md](README.md) (中文) или [README_en.md](README_en.md) (English).

## Лицензия

Проект распространяется под лицензией [GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE).

Вы можете свободно использовать, изменять и распространять это ПО, но любая модифицированная версия (включая сетевые сервисы) должна быть открыта под той же лицензией.
