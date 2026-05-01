---
domain: creator.douyin.com
aliases: [douyin, tiktok-cn]
updated: 2026-04-17
---
## Platform
- Creator backend: https://creator.douyin.com/creator-micro/content/upload
- Video publish: upload redirects to publish page (v1 or v2 route)
- Note publish: image upload -> separate publish page

## Effective Patterns
- Title: contenteditable div, max 30 chars
- Description: `.zone-container[contenteditable="true"]`
- Publish button: `button:has-text("publish")` or `button:has-text("send")`
- Scheduled publish: radio button for scheduled, then date picker
- Tags: input with # prefix, press space after each tag

## Known Issues
- Anti-bot: strict detection, prefer GUI interaction over URL construction
- Two different publish page versions (v1/v2) with different layouts
- Video cover auto-selection may be required before publish enabled
- QR login: scan in Douyin app, cookies persist across sessions
