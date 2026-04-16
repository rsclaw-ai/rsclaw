# RsClaw

**AI Automation Manager with One-Click OpenClaw Migration & Native Long-Term Memory.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)
[![Binary Size](https://img.shields.io/badge/binary-~15MB-green)]()

[English](../../README.md) | [中文](README_cn.md) | [日本語](README_ja.md) | [한국어](README_ko.md) | [ไทย](README_th.md) | [Tiếng Việt](README_vi.md) | **Français** | [Deutsch](README_de.md) | [Español](README_es.md) | [Русский](README_ru.md)

RsClaw est une reecriture complete d'[OpenClaw](https://github.com/openclaw/openclaw) en Rust, offrant le meme protocole de passerelle IA multi-agents avec un demarrage 10x plus rapide, une taille 10x plus petite et zero dependance Node.js.


<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

---

## Fonctionnalites principales

- **13+ canaux de messagerie** -- Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, Webhook personnalise
- **15 fournisseurs LLM** -- OpenAI, Anthropic, Google Gemini, DeepSeek, Qwen, Ollama, etc.
- **32 outils integres** -- Fichiers, Shell, Recherche web/Navigateur, Generation d'images, Memoire, Messagerie, cron, A2A
- **40+ commandes PreParse** -- Contournent le LLM, zero token, reponse sub-milliseconde
- **Automatisation navigateur CDP** -- Controle headless Chrome integre (20 actions)
- **Protocole A2A** -- Google A2A v0.3 (collaboration inter-agents sur le reseau)
- **Securite d'execution** -- Regles deny/confirm/allow, 50+ modeles de refus

## Installation rapide

```bash
# macOS / Linux (detection automatique de la plateforme)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

### Compiler depuis les sources

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --release
```

## Demarrage rapide

```bash
rsclaw onboard    # Assistant de configuration
rsclaw start      # Demarrer la passerelle
rsclaw status     # Verifier l'etat
rsclaw doctor --fix  # Verification de sante
```

## Plateformes supportees

macOS (x86_64, ARM64), Linux (x86_64, ARM64), Windows (x86_64, ARM64)

## Documentation

Documentation complete dans [README.md](../../README.md) (中文) ou [README_en.md](../../README.md) (English).

## Licence

Ce projet est sous licence [GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE).

Vous etes libre d'utiliser, modifier et distribuer ce logiciel, mais toute version modifiee (y compris les services reseau) doit etre publiee en open source sous la meme licence.
