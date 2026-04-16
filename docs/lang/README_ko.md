# RsClaw

**AI Automation Manager with One-Click OpenClaw Migration & Native Long-Term Memory.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)
[![Binary Size](https://img.shields.io/badge/binary-~15MB-green)]()

[English](../../README.md) | [中文](README_cn.md) | [日本語](README_ja.md) | **한국어** | [ไทย](README_th.md) | [Tiếng Việt](README_vi.md) | [Français](README_fr.md) | [Deutsch](README_de.md) | [Español](README_es.md) | [Русский](README_ru.md)

RsClaw는 [OpenClaw](https://github.com/openclaw/openclaw)를 Rust로 완전히 새로 작성한 것으로, 동일한 멀티에이전트 AI 게이트웨이 프로토콜을 제공하면서 10배 빠른 시작, 10배 작은 크기, Node.js 의존성 제로를 달성했습니다.


<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

---

## 주요 기능

- **13개 이상의 메시지 채널** -- Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, 커스텀 Webhook
- **15개 LLM 프로바이더** -- OpenAI, Anthropic, Google Gemini, DeepSeek, Qwen, Ollama 등
- **32개 내장 도구** -- 파일 작업, 셸 실행, 웹 검색/브라우저, 이미지 생성, 메모리, 메시징, cron, A2A 에이전트 협업
- **40개 이상의 프리파스 명령** -- LLM 바이패스, 토큰 소비 제로, 밀리초 미만 응답
- **CDP 브라우저 자동화** -- 내장 headless Chrome 제어 (20개 동작)
- **A2A 프로토콜** -- Google A2A v0.3 (네트워크 간 에이전트 협업)
- **실행 보안** -- deny/confirm/allow 규칙, 50개 이상의 거부 패턴
- **멀티에이전트** -- 순차, 병렬, 오케스트레이션 실행 모드

## 빠른 설치

```bash
# macOS / Linux (플랫폼 자동 감지)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

### 소스에서 빌드

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --release
```

## 빠른 시작

```bash
# 설정 마법사
rsclaw onboard

# 게이트웨이 시작
rsclaw start

# 상태 확인
rsclaw status

# 상태 점검
rsclaw doctor --fix
```

## RsClaw vs OpenClaw

| 특징 | RsClaw | OpenClaw |
|------|--------|----------|
| 언어 | Rust | TypeScript/Node.js |
| 바이너리 크기 | ~12MB | ~300MB+ (node_modules) |
| 시작 시간 | ~26ms | 2-5s |
| 메모리 사용량 | ~20MB | ~1000MB+ |
| 메시지 채널 | 13 + 커스텀 Webhook | 8 |
| LLM 프로바이더 | 15 | ~10 |
| 내장 도구 | 32 | ~25 |
| A2A 프로토콜 | Google A2A v0.3 | -- |
| 브라우저 자동화 | 내장 CDP (20개 동작) | -- |
| computer_use | 네이티브 스크린샷/마우스/키보드 | -- |

## 지원 플랫폼

macOS (x86_64, ARM64), Linux (x86_64, ARM64), Windows (x86_64, ARM64)

## 문서

자세한 문서는 [README.md](../../README.md) (中文) 또는 [README_en.md](../../README.md) (English)를 참조하세요.

## 라이선스

이 프로젝트는 [GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE)으로 라이선스됩니다.

이 소프트웨어를 자유롭게 사용, 수정, 배포할 수 있지만, 수정된 버전(네트워크 서비스 포함)은 동일한 라이선스로 오픈소스해야 합니다.
