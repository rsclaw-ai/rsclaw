---
domain: creator.xiaohongshu.com
aliases: [xiaohongshu, xhs, little-red-book]
updated: 2026-04-17
---
## Platform
- Video: https://creator.xiaohongshu.com/publish/publish?target=video
- Note/images: ?target=image (up to 30 images per note)
- Success page: URL matches **/publish/success?**

## Effective Patterns
- Upload then fill title, description, tags
- Success detection: wait for redirect to success URL

## Known Issues
- Very strict anti-crawl, always use web_browser (not web_fetch)
- xsec_token mechanism in URLs, do not manually construct URLs
- QR login: switch to QR panel first (click switch image element)
