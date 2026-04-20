# RsClaw

**AI Agent Engine with long-term memory and self-learning — one 15MB binary, 13 channels, 15 LLM providers, A2A cross-machine orchestration, browser automation, all in pure Rust. Your AI never forgets and gets better the more you use it.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Binary Size](https://img.shields.io/badge/binary-~15MB-green)]()

[English](../../README.md) | [中文](README_cn.md) | [日本語](README_ja.md) | [한국어](README_ko.md) | [ไทย](README_th.md) | **Tiếng Việt** | [Français](README_fr.md) | [Deutsch](README_de.md) | [Español](README_es.md) | [Русский](README_ru.md)

RsClaw la ban viet lai hoan toan cua [OpenClaw](https://github.com/openclaw/openclaw) bang Rust, cung cap cung giao thuc AI Gateway da tac tu nhung khoi dong nhanh hon 10 lan, kich thuoc nho hon 10 lan va khong phu thuoc Node.js.


<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

💬 [Join Community](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Tinh nang chinh

- **13+ kenh nhan tin** -- Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, Custom Webhook
- **15 nha cung cap LLM** -- OpenAI, Anthropic, Google Gemini, DeepSeek, Qwen, Ollama v.v.
- **32 cong cu tich hop** -- Quan ly file, Shell, Tim kiem web/Trinh duyet, Tao anh, Bo nho, Nhan tin, cron, A2A
- **40+ lenh PreParse** -- Bo qua LLM, khong ton token, phan hoi duoi mili giay
- **Tu dong hoa trinh duyet CDP** -- Dieu khien headless Chrome tich hop (20 thao tac)
- **Giao thuc A2A** -- Google A2A v0.3 (hop tac tac tu xuyen mang)
- **Bao mat thuc thi** -- Quy tac deny/confirm/allow, 50+ mau tu choi

## Cai dat nhanh

```bash
# macOS / Linux (tu dong phat hien nen tang)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

### Xay dung tu ma nguon

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --release
```

## Bat dau nhanh

```bash
rsclaw onboard    # Trinh huong dan cai dat
rsclaw start      # Khoi dong Gateway
rsclaw status     # Kiem tra trang thai
rsclaw doctor --fix  # Kiem tra suc khoe
```

## Nen tang ho tro

macOS (x86_64, ARM64), Linux (x86_64, ARM64), Windows (x86_64, ARM64)

## Tai lieu

Tai lieu chi tiet tai [README.md](../../README.md) (中文) hoac [README_en.md](../../README.md) (English).

## Giay phep

Dự án này được cấp phép kép MIT OR Apache-2.0. Xem [README tiếng Anh](../../README.md#license) để biết chi tiết.

Ban co the tu do su dung, chinh sua va phan phoi phan mem nay, nhung cac phien ban da chinh sua (bao gom dich vu mang) phai duoc ma nguon mo theo cung giay phep.
