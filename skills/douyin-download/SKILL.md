---
name: douyin-download
description: 下载抖音视频，用 web_browser 打开视频页面提取真实下载链接
version: 3.0.0
---

# 抖音视频下载

当用户要求下载抖音视频时，按以下步骤执行。web_browser 工具的调用格式如下：

## web_browser 调用格式

- 打开页面：`{"action":"open","url":"页面URL"}`
- 等待：`{"action":"wait","ms":5000}`
- 执行JS：`{"action":"evaluate","js":"JS代码"}`
- 截图：`{"action":"screenshot"}`

## 下载流程

1. **打开视频页面**
   ```json
   {"action":"open","url":"<用户提供的抖音视频URL>"}
   ```

2. **等待视频加载（5秒）**
   ```json
   {"action":"wait","ms":5000}
   ```

3. **提取视频真实 URL**
   ```json
   {"action":"evaluate","js":"document.querySelector('video')?.src"}
   ```
   如果返回空或 blob URL，再执行：
   ```json
   {"action":"evaluate","js":"JSON.stringify(Array.from(document.querySelectorAll('video,source')).map(e=>e.src||e.currentSrc).filter(s=>s&&s.startsWith('http')))"}
   ```

4. **下载视频文件**（用 exec 工具，选择对应平台命令）
   - macOS/Linux：`curl -L -o ~/Downloads/douyin_$(date +%Y%m%d_%H%M%S).mp4 "真实URL"`
   - Windows：`powershell -Command "Invoke-WebRequest -Uri '真实URL' -OutFile \"$env:USERPROFILE\Downloads\douyin_$(Get-Date -Format yyyyMMddHHmmss).mp4\""`

5. **告知用户**下载完成的文件路径和大小

## 注意事项

- 如果页面需要登录，用 `{"action":"screenshot"}` 截图发给用户扫码
- 如果 video src 是 blob:// 开头，无法直接下载，告知用户该视频受保护
- 使用已保存的 douyin.com 登录态（rsclaw profile）
