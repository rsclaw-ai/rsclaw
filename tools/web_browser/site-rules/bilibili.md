---
domain: www.bilibili.com
aliases: [bilibili, b-site]
updated: 2026-04-17
---
## Platform
- Video upload via biliup CLI tool (Rust binary, not browser)
- Install: `rsclaw tools install biliup` or download from GitHub

## Effective Patterns
- Login: `biliup login` (interactive QR code in terminal)
- Upload: `biliup upload <file> --title <t> --desc <d> --tid <category> --tags t1,t2`
- Category ID (tid) is required: e.g. 249 for lifestyle
- Credential refresh: `biliup renew`

## Known Issues
- Browser automation not recommended (complex anti-bot)
- biliup binary auto-downloads for current platform
- Cookie files stored at cookies/bilibili_<account>.json
