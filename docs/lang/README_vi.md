# RsClaw

> **Một AI agent engine biết ghi nhớ, học hỏi và định tuyến giữa các máy.**
> Rust binary 15MB · Cụm A2A hub-spoke · Bộ nhớ ba lớp · Knowledge base vector + BM25 · 13 kênh · 15 nhà cung cấp LLM · Thay thế drop-in cho OpenClaw.

[![GitHub Stars](https://img.shields.io/github/stars/rsclaw-ai/rsclaw?style=flat&logo=github)](https://github.com/rsclaw-ai/rsclaw/stargazers)
[![Crates.io](https://img.shields.io/crates/v/rsclaw?style=flat&logo=rust)](https://crates.io/crates/rsclaw)
[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue)](../../README.md#license)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust)](https://www.rust-lang.org/)

[🇺🇸 English](../../README.md) · [🇨🇳 中文](README_cn.md) · [🇯🇵 日本語](README_ja.md) · [🇰🇷 한국어](README_ko.md) · [ไทย](README_th.md) · **Tiếng Việt** · [Français](README_fr.md) · [Deutsch](README_de.md) · [Español](README_es.md) · [Русский](README_ru.md)

<p align="center">
  <img src="../images/en.gif" alt="RsClaw Preview" width="800" />
</p>

Phần lớn AI agent là các process không trạng thái dán chặt vào hộp chat. **RsClaw là một cụm máy (fleet)**: mỗi node lưu trữ memory có cấu trúc, đánh index knowledge base riêng và nói [Google A2A v1.0](https://a2a-protocol.org/). Một yêu cầu gõ trên laptop của bạn có thể fan-out tới GPU spoke để tạo ảnh, fleet node để RAG và partner agent từ xa cho tác vụ chuyên biệt — tất cả trả về như một luồng streaming duy nhất.

15 MB, ~20 MB RAM, binary tĩnh đơn lẻ. Rust thuần. Không Node, không Python.

💬 [Tham gia cộng đồng](https://rsclaw.ai/en/community) — WeChat / Feishu / QQ / Telegram

---

## Cài đặt

```bash
# Homebrew (macOS / Linux) — khuyên dùng
brew tap rsclaw-ai/tap
brew install rsclaw            # CLI
brew install --cask rsclaw     # App desktop (macOS DMG)

# Cargo
cargo install rsclaw

# Một dòng (macOS / Linux)
curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash

# Windows
irm https://app.rsclaw.ai/scripts/install.ps1 | iex
```

```bash
rsclaw setup          # khởi tạo ~/.rsclaw/
rsclaw onboard        # wizard tương tác: provider, channel, embedder
rsclaw start
```

---

## A2A — Định tuyến agent-tới-agent cấp fleet

RsClaw triển khai đầy đủ [đặc tả Google A2A v1.0](https://a2a-protocol.org/latest/specification/) — streaming, push notification, task persistence, cancel, INPUT_REQUIRED interrupt, đủ cả 11 phương thức JSON-RPC — cộng thêm **hub-spoke relay hạng nhất** hợp nhất cụm máy không đồng nhất thành một logical agent duy nhất. Mỗi spoke giữ một kết nối WebSocket outbound bền tới hub — không cần inbound port phía spoke. Hoạt động sau NAT, firewall và điều kiện mạng đại lục Trung Quốc.

→ Bề mặt protocol đầy đủ, vận hành hub-spoke, identity & ACL, công thức tunnel: [docs/a2a.md](../a2a.md).

---

## Memory — ba tầng, nhận biết suy giảm, recall lai

Bộ nhớ dài hạn bạn không bao giờ phải quản lý thủ công. Mỗi turn liên quan, runtime trích xuất tín hiệu bền vững thành các docs có cấu trúc (entity / preference / fact / procedure / relationship / lesson / failure), phân tầng theo **Core / Working / Peripheral** với suy giảm **Weibull stretched-exponential** từng tầng, và recall thông qua **tìm kiếm lai BM25 + vector** (hợp nhất RRF). Ngôn ngữ gốc được giữ nguyên — tiếng Việt vào, tiếng Việt ra.

→ Toán học của tầng, thiết kế prompt extractor, hoán đổi embedder, HTTP API: [docs/memory.md](../memory.md).

---

## Knowledge base — RAG được quản lý, ingest OOXML

Kho lưu trữ bền hạng nhất cho tài liệu dự án, code, hợp đồng — bất cứ thứ gì bạn muốn agent **trích dẫn thay vì tóm tắt** từ training. Collections là tag veneer trên một index chung. OOXML (.docx / .xlsx / .pptx), PDF, HTML, Markdown, source code được canonicalize khi ingest. Tìm kiếm lai (BM25 + vector + RRF + MMR); câu trả lời trích dẫn `doc_id` + offset.

```bash
rsclaw knowledge ingest <đường-dẫn> --collection hop-dong
rsclaw knowledge search "dự báo doanh thu Q3" --collection tai-chinh
```

→ Mô hình collections, pipeline ingest, search, API: [docs/kb.md](../kb.md).

---

## Tính năng chính

- **13+ kênh nhắn tin**: Telegram, Discord, Slack, WeChat, Feishu, DingTalk, QQ, WhatsApp, LINE, Signal, Matrix, Zalo, webhook tùy chỉnh
- **15+ nhà cung cấp LLM**: OpenAI, Anthropic, Gemini, DeepSeek, Qwen, Doubao, Ollama, v.v.
- **4 vòng đời agent**: Main / Named / Sub / Task; 4 backend: Native Rust / Claude Code / OpenCode / ACP
- **36 công cụ tích hợp**: files, shell, web, browser automation (CDP), image / video, STT / TTS, computer_use, cron, A2A, memory, KB
- **40+ lệnh pre-parsed**: zero token, phản hồi sub-millisecond
- **Plugin dual-runtime**: wasm (sandbox) + node/bun/deno (tương thích OpenClaw)
- **An toàn exec**: 50+ deny pattern, sandbox write, skills có chữ ký

---

## Migration từ OpenClaw

```bash
openclaw gateway stop
rsclaw setup          # phát hiện ~/.openclaw/, đề nghị import một lần nhấp
rsclaw start
```

`~/.openclaw/` không bao giờ bị sửa đổi. Cả hai có thể chạy song song (cổng 18888 vs 18789).

---

## Giấy phép

Cấp phép kép **MIT** HOẶC **Apache-2.0**. Tự do sử dụng trong sản phẩm cá nhân, thương mại, doanh nghiệp, SaaS hoặc proprietary. Sửa đổi và phân phối lại không có nghĩa vụ copyleft. Chi tiết: [README tiếng Anh](../../README.md#license).

🦀 Xây dựng bằng Rust. Lấy cảm hứng từ cộng đồng OpenClaw.
