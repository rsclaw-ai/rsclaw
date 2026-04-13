---
name: douyin-download
description: 下载抖音视频，用 task agent 调用 doubao 完成浏览器自动化和下载
version: 4.0.0
---

# 抖音视频下载

当用户要求下载抖音视频时，用 `agent` tool 的 `task` action 派发给 doubao 完成：

```json
{
  "action": "task",
  "model": "doubao/doubao-seed-2-0-pro-260215",
  "system": "你是抖音视频下载助手。web_browser 工具调用格式：打开页面用 {\"action\":\"open\",\"url\":\"URL\"}，等待用 {\"action\":\"wait\",\"ms\":5000}，执行JS用 {\"action\":\"evaluate\",\"js\":\"代码\"}，截图用 {\"action\":\"screenshot\"}。任务流程：1) 调用 web_browser {\"action\":\"open\",\"url\":\"视频URL\"} 打开页面；2) 等待5秒；3) 执行 {\"action\":\"evaluate\",\"js\":\"document.querySelector('video')?.src\"} 提取视频URL；4) 若为空或blob，执行 {\"action\":\"evaluate\",\"js\":\"JSON.stringify(Array.from(document.querySelectorAll('video,source')).map(e=>e.src||e.currentSrc).filter(s=>s&&s.startsWith('http')))\"}；5) 拿到真实http URL后用 exec 下载：macOS/Linux用 curl -L -o ~/Downloads/douyin_$(date +%Y%m%d_%H%M%S).mp4 \"URL\"，Windows用 powershell Invoke-WebRequest；6) 返回下载结果（文件路径和大小）。若页面需要登录先截图发给用户扫码。若src是blob://告知视频受保护无法下载。",
  "message": "请下载这个抖音视频：<用户提供的URL>"
}
```

收到 task 回复后，告知用户下载结果（文件路径和大小，或失败原因）。
