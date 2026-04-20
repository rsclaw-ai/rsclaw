# RsClaw

**AI Agent Engine with long-term memory and self-learning — one 15MB binary, 13 channels, 15 LLM providers, A2A cross-machine orchestration, browser automation, all in pure Rust. Your AI never forgets and gets better the more you use it.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)
[![Binary Size](https://img.shields.io/badge/binary-~15MB-green)]()

[English](../../README.md) | [中文](README_cn.md) | **日本語** | [한국어](README_ko.md) | [ไทย](README_th.md) | [Tiếng Việt](README_vi.md) | [Français](README_fr.md) | [Deutsch](README_de.md) | [Español](README_es.md) | [Русский](README_ru.md)

RsClaw は [OpenClaw](https://github.com/openclaw/openclaw) を Rust でフルスクラッチ再実装したもので、同じマルチエージェントAIゲートウェイプロトコルを提供しながら、10倍の起動速度、10倍のサイズ縮小、Node.js依存ゼロを実現しています。


<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

💬 [Join Community](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## 主な特徴

- **13以上のメッセージチャンネル** -- Telegram、Discord、Slack、WeChat、Feishu、DingTalk、QQ、WhatsApp、LINE、Signal、Matrix、Zalo、カスタムWebhook
- **15のLLMプロバイダー** -- OpenAI、Anthropic、Google Gemini、DeepSeek、Qwen、Ollama等
- **32の内蔵ツール** -- ファイル操作、シェル実行、Web検索/ブラウザ、画像生成、メモリ、メッセージング、cron、A2Aエージェント連携
- **40以上のプリパースコマンド** -- LLMをバイパス、トークン消費ゼロ、サブミリ秒応答
- **CDPブラウザ自動化** -- 内蔵headless Chrome制御（20操作）
- **A2Aプロトコル** -- Google A2A v0.3（ネットワーク越しのエージェント協調）
- **実行セキュリティ** -- deny/confirm/allowルール、50以上の拒否パターン
- **マルチエージェント** -- 順次、並列、オーケストレーション実行モード

## クイックインストール

```bash
# macOS / Linux（プラットフォーム自動検出）
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash
```

```powershell
# Windows（PowerShell）
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

### ソースからビルド

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --release
```

## クイックスタート

```bash
# セットアップウィザード
rsclaw onboard

# ゲートウェイ起動
rsclaw start

# ステータス確認
rsclaw status

# ヘルスチェック
rsclaw doctor --fix
```

## RsClaw vs OpenClaw

| 特徴 | RsClaw | OpenClaw |
|------|--------|----------|
| 言語 | Rust | TypeScript/Node.js |
| バイナリサイズ | ~12MB | ~300MB+（node_modules） |
| 起動時間 | ~26ms | 2-5s |
| メモリ使用量 | ~20MB | ~1000MB+ |
| メッセージチャンネル | 13 + カスタムWebhook | 8 |
| LLMプロバイダー | 15 | ~10 |
| 内蔵ツール | 32 | ~25 |
| A2Aプロトコル | Google A2A v0.3 | -- |
| ブラウザ自動化 | 内蔵CDP（20操作） | -- |
| computer_use | ネイティブスクリーンショット/マウス/キーボード | -- |

## 対応プラットフォーム

macOS (x86_64, ARM64)、Linux (x86_64, ARM64)、Windows (x86_64, ARM64)

## ドキュメント

詳細なドキュメントは [README.md](../../README.md)（中文）または [README_en.md](../../README.md)（English）をご覧ください。

## ライセンス

本プロジェクトは [GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE) でライセンスされています。

本ソフトウェアを自由に使用、変更、配布できますが、変更版（ネットワークサービスを含む）は同じライセンスでオープンソースにする必要があります。
