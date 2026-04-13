---
name: douyin-download
description: 下载抖音视频，动态创建子 agent 完成浏览器自动化和下载任务
version: 2.1.0
---

# 抖音视频下载

当用户提供抖音视频链接或要求下载抖音视频时，按以下流程操作。

## 流程

### 第一步：创建下载子 agent

用 `agent` tool spawn 一个专门的下载子 agent：

```json
{
  "action": "spawn",
  "id": "douyin-downloader",
  "model": "doubao/doubao-seed-2-0-pro-260215",
  "system": "你是抖音视频下载助手。web_browser 工具调用格式：打开页面用 {\"action\":\"open\",\"url\":\"URL\"}，截图用 {\"action\":\"screenshot\"}，执行JS用 {\"action\":\"evaluate\",\"js\":\"代码\"}，等待用 {\"action\":\"wait\",\"ms\":5000}。接到任务后：1) 调用 web_browser {\"action\":\"open\",\"url\":\"视频URL\"} 打开页面；2) 等待5秒 {\"action\":\"wait\",\"ms\":5000}；3) 调用 {\"action\":\"evaluate\",\"js\":\"document.querySelector('video')?.src\"} 提取 video src；4) 如果 src 为空或是 blob，执行 {\"action\":\"evaluate\",\"js\":\"JSON.stringify(Array.from(document.querySelectorAll('video,source')).map(e=>e.src||e.currentSrc).filter(s=>s&&s.startsWith('http')))\"} 获取所有视频URL；5) 拿到真实 http URL 后用 exec 下载：macOS/Linux 用 curl -L -o ~/Downloads/douyin_$(date +%Y%m%d_%H%M%S).mp4 \"URL\"，Windows 用 powershell Invoke-WebRequest，通用备选 python3 -c \"import urllib.request; urllib.request.urlretrieve('URL','path')\"；6) 返回下载结果（文件路径和大小）。如果页面需要登录先用 {\"action\":\"screenshot\"} 截图发给用户扫码。"
}
```

### 第二步：发送下载任务

用 `session` tool 发送任务给子 agent：

```json
{
  "action": "send",
  "agentId": "douyin-downloader",
  "message": "请下载这个抖音视频：<用户提供的URL>"
}
```

### 第三步：汇报结果

收到子 agent 回复后，告知用户：
- 下载成功：文件保存在哪里、大小多少
- 下载失败：原因是什么

## 注意事项

- 子 agent 复用已保存的 douyin.com 登录态（web_browser 使用 rsclaw profile）
- 如果视频是加密 blob URL 无法直接下载，告知用户"该视频受保护"
- 下载完成后不需要 kill 子 agent，它会自动待机复用
- 如果用户要批量下载，对每个链接都用 `session send` 发给同一个子 agent 处理
