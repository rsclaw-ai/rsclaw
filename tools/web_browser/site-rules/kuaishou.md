---
domain: cp.kuaishou.com
aliases: [kuaishou, kwai]
updated: 2026-04-17
---
## Platform
- Creator backend: https://cp.kuaishou.com/article/publish/video
- Uses Ant Design UI components

## Effective Patterns
- Date picker: `.ant-picker-input` for scheduled publish
- Time format: YYYY-MM-DD HH:MM:SS (with seconds)
- Publish flow: upload -> fill form -> publish

## Known Issues
- Tutorial overlay (Joyride) blocks interaction on first visit, must dismiss
- Guide overlay: `div[id^="react-joyride-step"]` -> find skip/close button
