# RsClaw

> **記憶し、学習し、マシン間でルーティングできる AI エージェントエンジン。**
> 21MB の Rust バイナリ · A2A hub-spoke フリート · 三層記憶 · ベクトル + BM25 ナレッジベース · 13 チャンネル · 15 LLM プロバイダー · OpenClaw drop-in 置き換え。

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![Release](https://img.shields.io/github/v/release/rsclaw-ai/rsclaw)](https://github.com/rsclaw-ai/rsclaw/releases)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

[🇺🇸 English](../../README.md) · [🇨🇳 中文](README_cn.md) · **🇯🇵 日本語** · [🇰🇷 한국어](README_ko.md) · [もっと ▾](.)

<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

ほとんどの AI エージェントはチャット欄に縛られたステートレスなプロセスです。**RsClaw はフリートです**：各ノードが構造化されたメモリを永続化し、プライベートなナレッジベースを索引し、[Google A2A v1.0](https://a2a-protocol.org/) を話します。ラップトップで打った一言が GPU spoke で画像生成、フリートノードで RAG、リモートのパートナーエージェントで専門タスクへ並列にファンアウトし、ひとつのストリーミング応答として戻ってきます。

21 MB、~28 MB RAM、シングルバイナリ。純粋な Rust。Node も Python もなし。

💬 [コミュニティ参加](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## インストール

### Homebrew（macOS / Linux）— 推奨

```bash
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # デスクトップアプリ（macOS DMG）
```

### その他

```bash
# Cargo
cargo install rsclaw

# ワンライナー（macOS / Linux）
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows
irm https://app.rsclaw.ai/scripts/install.ps1 | iex

# https://github.com/rsclaw-ai/rsclaw/releases からバイナリ取得
```

### 初回起動

```bash
rsclaw setup          # ~/.rsclaw/ を初期化
rsclaw onboard        # 対話ウィザード:プロバイダー、チャンネル、埋め込みモデル
rsclaw start
```

初回起動時、ローカル埋め込みモデル（BGE-small-zh、約 91 MB）が `~/.rsclaw/models/` にダウンロードされます。再開可能;safetensors を事前配置すればスキップ可。デスクトップ版は同梱済み。

---

## A2A — フリート級エージェント間ルーティング

RsClaw は [Google A2A v1.0 仕様](https://a2a-protocol.org/latest/specification/) を完全実装 — streaming、push 通知、タスク永続化、cancel、INPUT_REQUIRED 中断、11 個の JSON-RPC メソッドすべて — に加え、**ファーストクラスの hub-spoke リレー** を備えており、異機種のマシンフリートを単一の論理エージェントに変えます。

### なぜ A2A がフラッグシップ機能か

- **1 つの gateway、背後に複数バックエンド**。Hub が能力ごとに該当 spoke へルーティング — GPU マシンで画像 / 動画、大メモリマシンで RAG、パートナーマシンで専有ツール。
- **すべての spoke は LLM からはローカルツールに見える**（`agent_<peer-id>`）。記述から自動的にルーティングされ、オーケストレーションコードを書く必要なし。
- **NAT、ファイアウォール、中国本土の網状況にも耐性**：リレーは各 spoke から 1 本の永続 outbound WebSocket。inbound ポートは不要。

### トポロジー

```
        ユーザー（chat / channel / curl）
              │
              ▼
       ┌──────────────┐
       │  Hub agent   │  ← 公開インターネット、A2A v1.0 endpoint
       │   (router)   │
       └──────┬───────┘
        WS リレー
        (spoke ごとに
         1 つの永続接続)
              │
   ┌──────────┼──────────┐
   ▼          ▼          ▼
spoke-mac  spoke-aihub  spoke-partner
(あなたの    (2×4090     (サードパーティ
 ラップトップ)  GPU)       gateway)
```

各 spoke は **relay spoke モード** で動く `rsclaw gateway run` です。Hub の config で spoke を A2A peer として宣言すると、hub 上の LLM はそれらを `agent_spoke_mac`、`agent_spoke_aihub` のようなツールとして見て、能力記述から自動的にルーティングします。

Spoke 設定（これだけ）：

```json5
{
  gateway: {
    a2a: {
      relay: {
        mode: "spoke",
        nodeId: "spoke-aihub",
        relays: [
          "wss://hub.example.com/api/v1/a2a/relay/ws",
          "wss://backup.example.com/api/v1/a2a/relay/ws",   // primary-standby
        ],
        privateKey: "<keypair>",
      },
    },
  },
}
```

Hub 設定 — peer を宣言、LLM が記述で振り分け：

```json5
{
  agents: {
    a2a: [
      { id: "spoke_aihub",
        url: "http://localhost:18889",          // hub が自分自身に話す
        remoteAgentId: "spoke-aihub/main",
        description: "GPU マルチメディア生成: t2i / i2v / 数字人 / TTS。\
                      トリガー: 生成 / 描画 / 動画 / 音声 / 数字人。" },
      { id: "spoke_mac",
        url: "http://localhost:18889",
        remoteAgentId: "spoke-mac/main",
        description: "汎用チャット + ブラウザ自動化 + 抖音 / 微信 / 飞书。" },
    ],
  },
}
```

ユーザーが「**aihub で猫の画像を生成して**」と打つ → hub LLM が `agent_spoke_aihub` を選ぶ → リレーが転送 → aihub spoke が 4090 で `aihub-t2i` を実行 → 画像パスがリレー経由でストリーミングで戻る。

→ 完全なプロトコル仕様、hub-spoke 運用、identity & ACL、トンネルレシピ：[docs/a2a.md](../a2a.md)。

---

## メモリ — 三層、減衰対応、ハイブリッド検索

「save_memory」ツールを手動で呼ぶ必要のない長期メモリ。関連する各ターンで、ランタイムは：

1. **抽出** — ユーザーメッセージから永続的なシグナルを構造化ドキュメントへ蒸留（entity / preference / fact / procedure / relationship / lesson / failure）。flash モデル経由、**元言語を保持**（日本語入力 → 日本語保存、翻訳しない）。
2. **階層化**：
   - **Core** — 同一性レベル（名前、連絡先、ピン留めされた事実）。減衰下限 0.9、降格なし。
   - **Working** — アクティブなコンテキスト。標準指数減衰;頻繁な再呼び出しで Peripheral から昇格。
   - **Peripheral** — 低シグナル。急速に減衰、自動降格、時間で剪定。
3. **減衰** — **Weibull 引き伸ばし指数** + 層ごとに異なる β — 最近 + 頻繁 + 重要なドキュメントが高スコア、古い + 無視されたものは沈下。
4. **検索** — **ハイブリッド検索**：BM25 キーワード（tantivy）+ ベクトル余弦（hnsw_rs）、RRF で融合。関連する全ターンで自動的に LLM コンテキストへ注入 — 手動 recall 不要。

### 埋め込みティア

| ティア | 埋め込みモデル | レイテンシ | いつ |
|---|---|---|---|
| **ローカル** | BGE-small-zh-v1.5（Candle、91 MB） | ~5 ms / doc | デフォルト。デスクトップ同梱、CLI は初回起動時に自動 DL。 |
| **リモート** | Qwen3-Embedding-0.6B（1024 次元）on llama.cpp | ~30 ms / doc | より高品質。`memory.embedder.remote_url` を設定。 |

→ 階層数式、抽出器プロンプト設計、埋め込み切替、HTTP API：[docs/memory.md](../memory.md)。

---

## ナレッジベース — 管理された RAG、OOXML 取込、引用付きスニペット出力

セッションメモリとは別の、ファーストクラスの永続化された知識ストア。用途：プロジェクト文書、参考資料、コードベース、議事録、契約書 — エージェントに**訓練データの要約ではなく引用させたい**ものすべて。

- **コレクション** — 単一の embedding インデックス上のタグベニア。デスクトップ UI または HTTP API で作成 / 一覧 / 削除;per-collection ストアの overhead はゼロ。
- **取込** — デスクトップアプリでドラッグ＆ドロップ、または `POST /api/v1/knowledge/collections/<id>/docs`。プレーンテキスト、Markdown、PDF、**OOXML**（.docx / .xlsx / .pptx）、HTML 対応。
- **検索** — メモリと同じハイブリッド BM25 + ベクトルパイプライン、コレクション単位スコープ。
- **デフォルトで引用付き** — エージェントの `knowledge_base` ツールはスニペットを doc-id + offset 付きで返すので、応答は引用できる。

```bash
rsclaw knowledge ingest <path> --collection 議事録
rsclaw knowledge search "Q3 売上予測" --collection 財務報告

# チャット内 — クエリがコレクションにマッチすると agent が自動的に knowledge_base ツールを使う
"Q3 財務報告によると、粗利率はどう?"
```

→ コレクションモデル、取込パイプライン、検索（BM25 + ベクトル + RRF + MMR）、CLI / HTTP API：[docs/kb.md](../kb.md)。

---

## エージェント — 4 種のライフタイム、4 種のバックエンド

| タイプ | 作成元 | 永続性 | 終了 |
|------|-----------|----------|-----------|
| **Main** | システム | 永久 | 終了不可 |
| **Named** | ユーザー / config | 再起動後も保持 | ユーザーのみ |
| **Sub** | LLM `agent_spawn` | セッション | 作成者 |
| **Task** | LLM `agent_task` | ワンショット | 戻り時に自動削除 |

各エージェントは独立にバックエンドを選択：**Native Rust**（デフォルト、最速）、**Claude Code**（Claude Agent SDK + ACP）、**OpenCode**、**任意の ACP 準拠エージェント**。

---

## チャンネル（13 + カスタム）

WeChat 個人 · Feishu / Lark · WeCom · QQ Bot · DingTalk · Telegram · Discord · Slack · WhatsApp · Signal · LINE / Zalo · Matrix · カスタム webhook（`/hooks/{name}`）。

各チャンネル：DM / グループ ACL、ペアリングコード（8 桁、1 時間 TTL）、ヘルスモニタリング、リトライ、ストリーミング、ファイルアップロード確認ゲート。

---

## LLM プロバイダー（15+）

Qwen · DeepSeek · Kimi · Zhipu（GLM）· MiniMax · Doubao（ByteDance）· SiliconFlow · GateRouter · OpenRouter · Anthropic · OpenAI · Gemini · xAI（Grok）· Groq · Ollama · OpenAI 互換 endpoint 全般。

---

## 設定

```json5
{
  gateway: { port: 18888 },
  models: {
    providers: {
      doubao: { apiKey: "${DOUBAO_API_KEY}" },
      ollama: { baseUrl: "http://localhost:11434" },
    },
  },
  agents: {
    defaults: { model: { primary: "doubao/doubao-seed-1-6-pro" } },
    list: [{ id: "main", default: true }],
  },
}
```

すべての文字列は `${VAR}` 環境変数置換に対応。優先順位：CLI flag > `$RSCLAW_BASE_DIR/rsclaw.json5` > `~/.rsclaw/rsclaw.json5` > `./rsclaw.json5`。

---

## OpenClaw からの移行

```bash
openclaw gateway stop
rsclaw setup          # ~/.openclaw/ を検出、ワンクリックインポートを提案
rsclaw start
```

`~/.openclaw/` への書き込みは行いません;新データは `~/.rsclaw/` へ。両方を別ポート（18888 vs 18789）で並行稼働可能。

---

## ライセンス

**MIT** OR **Apache-2.0** のデュアルライセンス。個人、商用、エンタープライズ、SaaS、プロプライエタリ製品で自由に使用可能。改変・再配布も copyleft の義務なし。詳細：[英語 README](../../README.md#license)。

🦀 Rust で構築。OpenClaw コミュニティに敬意。
