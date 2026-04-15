# RsClaw

**AI Automation Manager with One-Click OpenClaw Migration & Native Long-Term Memory.**

[![Rust](https://img.shields.io/badge/Rust-1.91%20Edition%202024-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)
[![Binary Size](https://img.shields.io/badge/binary-~12MB-green)]()

[English](../../README.md) | [中文](README_cn.md) | [日本語](README_ja.md) | [한국어](README_ko.md) | **ไทย** | [Tiếng Việt](README_vi.md) | [Français](README_fr.md) | [Deutsch](README_de.md) | [Español](README_es.md) | [Русский](README_ru.md)

RsClaw คือการเขียนใหม่ทั้งหมดของ [OpenClaw](https://github.com/openclaw/openclaw) ด้วย Rust ให้โปรโตคอล AI Gateway แบบมัลติเอเจนต์เดียวกัน แต่เริ่มต้นเร็วขึ้น 10 เท่า ขนาดเล็กลง 10 เท่า และไม่พึ่งพา Node.js

---

## คุณสมบัติหลัก

- **13+ ช่องทางข้อความ** -- Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, Custom Webhook
- **15 ผู้ให้บริการ LLM** -- OpenAI, Anthropic, Google Gemini, DeepSeek, Qwen, Ollama ฯลฯ
- **32 เครื่องมือในตัว** -- จัดการไฟล์, Shell, ค้นหาเว็บ/เบราว์เซอร์, สร้างภาพ, หน่วยความจำ, ส่งข้อความ, cron, A2A
- **40+ คำสั่ง PreParse** -- ข้าม LLM, ไม่เสียโทเค็น, ตอบสนองต่ำกว่ามิลลิวินาที
- **ระบบอัตโนมัติเบราว์เซอร์ CDP** -- ควบคุม headless Chrome ในตัว (20 การดำเนินการ)
- **โปรโตคอล A2A** -- Google A2A v0.3 (การทำงานร่วมกันของเอเจนต์ข้ามเครือข่าย)
- **ความปลอดภัยในการดำเนินการ** -- กฎ deny/confirm/allow, 50+ รูปแบบการปฏิเสธ

## ติดตั้งอย่างรวดเร็ว

```bash
# macOS / Linux (ตรวจจับแพลตฟอร์มอัตโนมัติ)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

### สร้างจากซอร์สโค้ด

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
cargo build --release
```

## เริ่มต้นอย่างรวดเร็ว

```bash
rsclaw onboard    # ตัวช่วยตั้งค่า
rsclaw start      # เริ่ม Gateway
rsclaw status     # ตรวจสอบสถานะ
rsclaw doctor --fix  # ตรวจสุขภาพ
```

## แพลตฟอร์มที่รองรับ

macOS (x86_64, ARM64), Linux (x86_64, ARM64), Windows (x86_64, ARM64)

## เอกสาร

เอกสารฉบับเต็มอยู่ที่ [README.md](../../README.md) (中文) หรือ [README_en.md](../../README.md) (English)

## สัญญาอนุญาต

โปรเจกต์นี้ใช้สัญญาอนุญาต [GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE)

คุณสามารถใช้ แก้ไข และแจกจ่ายซอฟต์แวร์นี้ได้อย่างอิสระ แต่เวอร์ชันที่แก้ไข (รวมถึงบริการเครือข่าย) ต้องเปิดซอร์สภายใต้สัญญาอนุญาตเดียวกัน
