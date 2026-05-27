# RsClaw

> **기억하고, 학습하고, 머신 간 라우팅하는 AI 에이전트 엔진.**
> 15MB Rust 바이너리 · A2A hub-spoke 플릿 · 3-tier 메모리 · 벡터 + BM25 지식 베이스 · 13 채널 · 15 LLM 프로바이더 · OpenClaw drop-in 교체.

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![Release](https://img.shields.io/github/v/release/rsclaw-ai/rsclaw)](https://github.com/rsclaw-ai/rsclaw/releases)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

[🇺🇸 English](../../README.md) · [🇨🇳 中文](README_cn.md) · [🇯🇵 日本語](README_ja.md) · **🇰🇷 한국어** · [더 보기 ▾](.)

<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

대부분의 AI 에이전트는 채팅 박스에 묶인 stateless 프로세스입니다. **RsClaw는 플릿입니다**: 각 노드가 구조화된 메모리를 영속화하고, 프라이빗 지식 베이스를 인덱싱하며, [Google A2A v1.0](https://a2a-protocol.org/)를 구사합니다. 노트북에서 친 한 마디가 GPU spoke 이미지 생성, 플릿 노드의 RAG, 원격 파트너 에이전트의 전문 작업으로 동시에 팬-아웃되어 하나의 스트리밍 응답으로 돌아옵니다.

15 MB, ~20 MB RAM, 단일 정적 바이너리. 순수 Rust. Node, Python 없음.

💬 [커뮤니티 참가](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## 설치

### Homebrew (macOS / Linux) — 권장

```bash
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # 데스크톱 앱 (macOS DMG)
```

### 그 외

```bash
cargo install rsclaw
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash      # macOS / Linux
irm https://app.rsclaw.ai/scripts/install.ps1 | iex             # Windows
```

### 첫 실행

```bash
rsclaw setup          # ~/.rsclaw/ 초기화
rsclaw onboard        # 대화형 마법사
rsclaw start
```

---

## A2A — 플릿급 에이전트 간 라우팅

RsClaw는 [Google A2A v1.0 명세](https://a2a-protocol.org/latest/specification/)를 완전 구현하며 (streaming, push 알림, 작업 영속화, cancel, INPUT_REQUIRED — 11개 JSON-RPC 메서드 모두), 거기에 **first-class hub-spoke relay**를 더해 이종 머신 플릿을 하나의 논리 에이전트로 만듭니다.

- **하나의 gateway, 뒤에 여러 백엔드**. Hub가 능력별로 spoke에 라우팅.
- **모든 spoke가 LLM에게는 로컬 도구**로 보임 (`agent_<peer-id>`). orchestration 코드 불필요.
- **NAT / 방화벽 / 중국 본토 네트워크 대응**: relay는 spoke에서 1개 영속 outbound WebSocket.

```
        사용자
          ▼
       Hub agent ← public A2A endpoint
          │
        WS relay
          │
   ┌──────┼──────┐
spoke-mac  spoke-aihub  spoke-partner
```

→ 전체 프로토콜, hub-spoke 운영, identity & ACL, 터널 레시피: [docs/a2a.md](../a2a.md).

---

## 메모리 — 3-tier, 감쇠 인식, 하이브리드 리콜

자동화된 장기 메모리. 매 관련 턴마다:

1. **추출** — 지속 가능한 신호를 구조화 doc으로 (entity/preference/fact/procedure/relationship/lesson/failure). **원어 보존** (한국어 → 한국어).
2. **계층화** — Core (정체성 사실, 강등 없음) / Working (활성) / Peripheral (저신호, 빠른 감쇠).
3. **감쇠** — Weibull stretched-exponential, 계층별 β.
4. **리콜** — BM25 + 벡터 코사인, RRF 융합. 모든 관련 턴에서 LLM 컨텍스트에 자동 주입.

임베더: BGE-small-zh (로컬, 91MB, 기본) 또는 Qwen3-Embedding-0.6B (원격 llama.cpp).

→ 계층 수식, 추출기 prompt, 임베더 교체, HTTP API: [docs/memory.md](../memory.md).

---

## 지식 베이스 — 관리되는 RAG, OOXML 인제스트

세션 메모리와 분리된 영속 지식 저장소. 컬렉션은 단일 임베딩 인덱스 위의 tag veneer. 텍스트, Markdown, PDF, OOXML (.docx / .xlsx / .pptx), HTML 지원. 메모리와 동일한 BM25 + 벡터 + RRF + MMR 검색. 응답은 doc-id + offset 인용.

```bash
rsclaw knowledge ingest <path> --collection 회의록
rsclaw knowledge search "Q3 매출" --collection 재무
```

→ 컬렉션 모델, 인제스트 파이프라인, 검색, API: [docs/kb.md](../kb.md).

---

## 에이전트

| 타입 | 생성자 | 영속성 |
|---|---|---|
| **Main** | 시스템 | 영원 (종료 불가) |
| **Named** | 사용자 / config | 재시작 후 유지 |
| **Sub** | LLM `agent_spawn` | 세션 |
| **Task** | LLM `agent_task` | 단발 |

백엔드 선택: Native Rust (기본) / Claude Code (Claude Agent SDK + ACP) / OpenCode / ACP 호환 모두.

---

## 채널 (13 + 커스텀)

WeChat 개인 · Feishu · WeCom · QQ Bot · DingTalk · Telegram · Discord · Slack · WhatsApp · Signal · LINE / Zalo · Matrix · 커스텀 webhook.

---

## LLM 프로바이더 (15+)

Qwen · DeepSeek · Kimi · Zhipu (GLM) · MiniMax · Doubao · SiliconFlow · GateRouter · OpenRouter · Anthropic · OpenAI · Gemini · xAI · Groq · Ollama · OpenAI 호환 endpoint 전반.

---

## OpenClaw에서 마이그레이션

```bash
openclaw gateway stop
rsclaw setup          # ~/.openclaw/ 감지, 원클릭 임포트
rsclaw start
```

---

## 라이선스

**MIT** OR **Apache-2.0** 듀얼 라이선스. 개인 / 상용 / 엔터프라이즈 / SaaS / 프로프라이어터리 자유 사용. copyleft 의무 없음. → [영어 README](../../README.md#license).

🦀 Rust로 빌드. OpenClaw 커뮤니티에 감사.
