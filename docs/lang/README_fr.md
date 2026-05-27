# RsClaw

> **Un moteur d'agents IA qui se souvient, apprend et route entre machines.**
> Binaire Rust de 15 Mo · Flotte A2A hub-spoke · Mémoire à trois niveaux · Base de connaissances vecteur + BM25 · 13 canaux · 15 fournisseurs LLM · Remplacement drop-in d'OpenClaw.

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

[🇺🇸 English](../../README.md) · [🇨🇳 中文](README_cn.md) · [🇯🇵 日本語](README_ja.md) · [🇰🇷 한국어](README_ko.md) · [ไทย](README_th.md) · [Tiếng Việt](README_vi.md) · **Français** · [Deutsch](README_de.md) · [Español](README_es.md) · [Русский](README_ru.md)

<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

La plupart des agents IA sont des processus sans état collés à une fenêtre de chat. **RsClaw est une flotte** : chaque nœud persiste une mémoire structurée, indexe une base de connaissances privée et parle [Google A2A v1.0](https://a2a-protocol.org/). Une requête tapée sur votre portable peut se déployer vers un spoke GPU pour la génération d'image, un nœud de flotte pour le RAG et un agent partenaire distant pour une tâche spécialisée — le tout retournant comme un flux de réponse unique.

15 Mo, ~20 Mo de RAM, binaire statique unique. Rust pur. Pas de Node, pas de Python.

💬 [Rejoindre la communauté](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Installation

```bash
# Homebrew (macOS / Linux) — recommandé
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # App de bureau (macOS DMG)

# Cargo
cargo install rsclaw

# Une ligne (macOS / Linux)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

```bash
rsclaw setup          # initialiser ~/.rsclaw/
rsclaw onboard        # assistant interactif : fournisseur, canal, embedder
rsclaw start
```

---

## A2A — Routage agent-à-agent niveau flotte

RsClaw implémente la spécification complète [Google A2A v1.0](https://a2a-protocol.org/latest/specification/) — streaming, notifications push, persistance de tâches, cancel, interruptions INPUT_REQUIRED, les 11 méthodes JSON-RPC — plus un **relai hub-spoke de première classe** qui fusionne une flotte de machines hétérogènes en un seul agent logique. Chaque spoke maintient une connexion WebSocket sortante persistante vers le hub — aucun port entrant requis côté spoke. Fonctionne derrière NAT, pare-feu et conditions réseau de Chine continentale.

→ Surface complète du protocole, opérations hub-spoke, identité & ACL, recettes de tunnel : [docs/a2a.md](../a2a.md).

---

## Mémoire — trois niveaux, conscient du déclin, recherche hybride

Mémoire à long terme que vous n'avez jamais à gérer manuellement. À chaque tour pertinent, le runtime extrait des signaux durables en docs structurés (entity / preference / fact / procedure / relationship / lesson / failure), les classe en niveaux **Core / Working / Peripheral** avec un déclin **Weibull exponentiel étiré** par niveau, et les récupère via **recherche hybride BM25 + vecteur** (fusionnée par RRF). La langue originale est préservée — français en entrée, français en sortie.

→ Mathématiques des niveaux, conception du prompt extracteur, changement d'embedder, API HTTP : [docs/memory.md](../memory.md).

---

## Base de connaissances — RAG géré, ingestion OOXML

Stockage persistant de première classe pour documents de projet, code, contrats — tout ce que vous voulez que l'agent **cite plutôt que résume** depuis l'entraînement. Les collections sont une couche de tags sur un index partagé. OOXML (.docx / .xlsx / .pptx), PDF, HTML, Markdown, code source sont canonicalisés à l'ingestion. Recherche hybride (BM25 + vecteur + RRF + MMR) ; les réponses citent `doc_id` + offset.

```bash
rsclaw knowledge ingest <chemin> --collection contrats
rsclaw knowledge search "prévision revenus Q3" --collection finances
```

→ Modèle de collections, pipeline d'ingestion, recherche, API : [docs/kb.md](../kb.md).

---

## Fonctionnalités clés

- **13+ canaux de messagerie** : Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, webhook personnalisé
- **15+ fournisseurs LLM** : OpenAI, Anthropic, Gemini, DeepSeek, Qwen, Doubao, Ollama, etc.
- **Quatre durées de vie d'agent** : Main / Named / Sub / Task ; quatre backends : Native Rust / Claude Code / OpenCode / ACP
- **36 outils intégrés** : fichiers, shell, web, automatisation navigateur (CDP), image / vidéo, STT / TTS, computer_use, cron, A2A, mémoire, KB
- **40+ commandes pré-parsées** : zéro token, réponse sub-milliseconde
- **Plugin dual-runtime** : wasm (sandbox) + node/bun/deno (compatible OpenClaw)
- **Sécurité d'exécution** : 50+ patrons de deny, écriture sandbox, skills signées

---

## Migration depuis OpenClaw

```bash
openclaw gateway stop
rsclaw setup          # détecte ~/.openclaw/, propose un import en un clic
rsclaw start
```

`~/.openclaw/` n'est jamais modifié. Les deux peuvent fonctionner en parallèle (ports 18888 vs 18789).

---

## Licence

Double licence sous **MIT** OR **Apache-2.0**. Utilisation libre dans produits personnels, commerciaux, entreprises, SaaS ou propriétaires. Modification et redistribution sans obligation copyleft. Détails : [README anglais](../../README.md#license).

🦀 Construit en Rust. Inspiré par la communauté OpenClaw.
