---
name: douyin
description: Douyin (TikTok China) desktop app - browse, search, publish, live, messaging
---

# Douyin Desktop App (抖音)

## App Info
- Bundle: 抖音.app
- UI framework: Electron (ui_tree returns only 3 window buttons, useless)
- Theme: dark
- Window default: ~1296x748 at (72, 47)

## Layout

### Left Sidebar (~160px wide)
Top section:
- 抖音 logo
- 精选 (Featured/Curated) - default view, grid of video thumbnails
- 推荐 (Recommended) - full-screen video feed, scroll to browse
- AI 搜索 (AI Search)

Social section:
- 关注 (Following) - may have notification badge
- 朋友 (Friends)
- 我的 (My profile) - may have red dot

Content section:
- 直播 (Live streams)
- 放映厅 (Theater/Cinema)
- 短剧 (Short dramas/series)

Bottom:
- 下载抖音精选 / 下载 APP
- Settings (gear icon), Help (?), Share icon

### Top Bar
- Center: Search box "搜索你感兴趣的内容" with "Q 搜索" button
- Right: 壁纸 (Wallpaper), 通知 (Notifications, may have badge), 私信 (Messages, may have badge), 投稿 (Post/Upload), User avatar, Pin icon

### Category Tabs (below search bar, in 精选 view)
全部 | 公开课 | 知识 | 体育 | 影视 | 音乐 | 游戏 | 二次元 | 美食 | 汽车 | 生活vlog | 旅行 | 小剧场 | 三农 | 动物 | ...
- Scrollable horizontally with arrow buttons at edges

### Main Content Area

**精选 (Featured) view:**
- Grid layout of video thumbnails (2-3 columns)
- Each thumbnail: preview image, play count, duration, title below
- Click thumbnail to play

**推荐 (Recommended) view:**
- Single full-screen video player
- Right sidebar interaction icons (top to bottom):
  - AI icon (top right corner)
  - User avatar
  - ❤ Like count
  - 💬 Comment count
  - ⭐ Favorite count
  - ↗ Share count
  - 听抖音 (Listen mode)
- Bottom: video info (author, date, description, hashtags)
- Bottom bar: play/pause, progress bar, timestamp, 弹幕 controls, 连播/清屏/智能/倍速/音量/全屏

**Video player controls (bottom bar in 推荐 view):**
- Play/Pause button
- Progress bar with timestamp (e.g., 00:02 / 26:49)
- 弹幕 (Danmaku/bullet comments) toggle
- 发送 (Send comment)
- 连播 (Auto-play next)
- 清屏 (Clean screen)
- 智能 (Smart mode)
- 倍速 (Playback speed)
- 音量 (Volume)
- 全屏 (Fullscreen)

## Operations

### Browse featured videos
1. Click "精选" in left sidebar
2. Grid of video thumbnails loads
3. Scroll down for more
4. Click any thumbnail to play

### Watch recommended feed
1. Click "推荐" in left sidebar
2. Full-screen video starts playing
3. Scroll down or press Down arrow for next video
4. Scroll up or press Up arrow for previous video

### Search for content
1. Click search box at top center
2. Type keywords
3. Press Enter or click "搜索"
4. Results show in main area

### AI Search
1. Click "AI 搜索" in left sidebar
2. AI-powered search interface loads

### Like a video (in 推荐 view)
1. Click the heart icon on right side of video
2. Or double-click on the video

### Comment on a video
1. Click the comment bubble icon on right side
2. Comment panel slides out
3. Type comment in input box at bottom of panel
4. Press Enter to post

### Share a video
1. Click the share icon on right side
2. Share options appear

### Favorite a video
1. Click the star icon on right side

### View live streams
1. Click "直播" in left sidebar
2. Live stream grid loads
3. Click to enter a stream

### Post/Upload a video
1. Click "投稿" in top right bar
2. Upload interface opens (may open in browser)
3. Select video file
4. Fill in title, description, tags
5. Click publish

### Send a direct message
1. Click "私信" in top right bar
2. Message panel opens
3. Select conversation or start new
4. Type message, press Enter

### Check notifications
1. Click "通知" in top right bar
2. Notification panel opens

### View user profile
1. Click on any username/avatar in video
2. Profile page loads with bio, videos, followers

### Follow a user
1. Navigate to user's content
2. Click "关注" (Follow) button near username

### Watch short dramas
1. Click "短剧" in left sidebar
2. Drama series grid loads
3. Click to watch

## Navigation Tips
- In 推荐 view: scroll up/down to switch videos (one at a time, full-screen cards)
- In 精选 view: standard grid scrolling
- Press Space to pause/resume video playback
- Press F or double-click video area for fullscreen
- Press Esc to exit fullscreen

## Tips
- Electron app: ui_tree completely useless, must use screenshot + coordinates
- 推荐 view is full-screen video mode, very different from 精选 grid view
- Right-side interaction icons only visible in 推荐/video player view
- Category tabs only visible in 精选 view
- "投稿" (upload) may redirect to web browser for full upload flow
- Video descriptions and hashtags are at the bottom of the video in 推荐 view
- Danmaku (弹幕) is a unique feature - floating text comments overlay on video
