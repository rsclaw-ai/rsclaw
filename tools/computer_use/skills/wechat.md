---
name: wechat
description: WeChat (微信) desktop client - locate contacts/groups, send/read messages, search and add friends
---

# WeChat Desktop App

## App Info
- Bundle: WeChat.app (macOS) / WeChat.exe (Windows)
- UI framework: native shell + embedded webviews (ui_tree gives partial coverage on macOS, almost nothing on Windows — rely on screenshot + coordinates)
- Theme: light by default, follows system on macOS
- Required state: must be logged in. If a QR code is on screen, do NOT scan automatically — tell the user to scan, then wait.

## Window Layout

```
+---------+---------------------------------+
| left    | right                           |
| sidebar | (empty until a chat is opened)  |
| (60px)  |                                 |
+---------+---------------------------------+
| chat    | conversation view               |
| list    |   * messages area               |
| (260px) |   * input box (bottom)          |
|         |   * send button (bottom-right)  |
+---------+---------------------------------+
```

- **Left sidebar (60px)**: avatar, search icon, tab icons (聊天 / 通讯录 / 收藏 / 朋友圈 / 看一看 / 视频号)
- **Search bar**: top of chat list. Type here to search contacts / groups / chat history.
- **Chat list (260px)**: shows conversations. Red badge with number = unread count.
- **Conversation view (right)**: chat title bar at top, scrollable messages, input box bottom.
- **Input box**: bottom of conversation view, has emoji / file / screenshot icons + a "发送" button on the right (visible after typing).

## Operations

### Activate WeChat
- macOS: `open -a WeChat` via Bash, OR cmd+space → type "WeChat" → Return
- Windows: win key → type "WeChat" → Return
- After activate, screenshot to confirm the main window is in front.

### Locate a contact or group (canonical step before sending)

1. Click the search bar at the top of the chat list, OR keyboard:
   - macOS: cmd+f (when WeChat is focused)
   - Windows: ctrl+f
2. Type query (Chinese name, WeChat ID, or group name):
   ```json
   {"action": "type", "text": "张三"}
   ```
3. Wait 500-1000ms for results to render, then screenshot.
4. Result groups appear under category headers: 联系人 / 群聊 / 聊天记录 / 公众号. Pick the right category.
5. **Disambiguation rules:**
   - Same name appearing under both 联系人 and 群聊 → use the user's intent; if unclear, screenshot and ask the user which one.
   - Match marked "添加为朋友" or under category 网络结果 = NOT in your contacts. Do not message them. Use the "search and add friend" flow below.
   - If only "聊天记录" matches show up, the contact/group exists but the search didn't find a name match — refine the query.
6. Click the target row to open the conversation. The right pane should now show the chat with the title bar matching the name you wanted.

### Send a message to a contact

1. Locate the contact (steps above) and confirm the right pane title.
2. Click anywhere inside the input box at the bottom to focus it.
3. Type the message:
   ```json
   {"action": "type", "text": "明天下午三点开会"}
   ```
4. **Multi-line**: WeChat sends on Enter. For a newline use `Shift+Enter`. Avoid putting `\n` in `type` text — it sends mid-message.
5. Press Enter (or click 发送) to send:
   ```json
   {"action": "key", "text": "Return"}
   ```
6. Screenshot. Confirm the most recent green bubble at the bottom contains your text. If it does not, the input was sent to the wrong window — re-locate and retry.

### Send a message to a group (with @ mention)

1. Locate the group (steps above) and open it.
2. Click the input box.
3. Type `@` only:
   ```json
   {"action": "type", "text": "@"}
   ```
4. Wait ~300ms for the member picker popup. Type the first 2-3 chars of the member's name:
   ```json
   {"action": "type", "text": "张"}
   ```
5. Press Return / Enter or click the matching name in the popup. The mention becomes a styled chip in the input box.
6. Continue typing the message body, then Enter to send.

### Read the latest messages

1. Open the chat (locate steps above, or click an unread row in the chat list).
2. Press End (or scroll the messages area to the bottom) to ensure the newest message is visible.
3. Screenshot the conversation pane.
4. **Bubble identification**:
   - Green bubble on the right = your messages — ignore.
   - White/light bubble on the left = the other party — read these.
   - In groups, the sender's display name is rendered above their bubble.
   - Centered gray text = system message (joined / left / recalled). Usually ignore.
5. **Non-text messages**:
   - Image / sticker → shows a thumbnail; OCR will not give the conversation text. Report "对方发了一张图片" and skip OCR.
   - Voice → shows a duration bubble. Report "对方发了一段语音"; transcription is out of scope here.
   - File → shows filename + size. Note it; do not auto-download.
6. Quote the most recent inbound bubble back to the user with the sender's name (for groups) or contact name (for 1-on-1).

### Reply to an incoming message

1. Read latest messages (steps above).
2. Decide whether to reply:
   - Direct question to you / @ you in a group → reply
   - Generic group chatter, broadcasts, bot notifications → do not auto-reply, ask the user
3. If replying in a group and you need to mention the sender, follow the @ mention flow.
4. Type the reply, Enter to send.
5. Screenshot to confirm the reply bubble appears at the bottom.

### Search and add friend

Use this when you searched a person and they appeared under "网络结果 / 添加为朋友" — meaning they're not yet in your contacts.

1. Open search bar (cmd+f / ctrl+f).
2. Type the person's WeChat ID, phone number, or full Chinese name:
   ```json
   {"action": "type", "text": "wxid_abc123"}
   ```
3. Screenshot results. WeChat ID searches surface a "搜索：xxx" row at the bottom of the list — click it to perform a network search.
4. Click the matching profile in the network-result panel.
5. The profile detail dialog opens. Click the "添加到通讯录" button (usually green/blue, prominent).
6. A "申请添加朋友" dialog appears with a verification message field:
   - **Default text** is something like "我是XXX". Replace with the user-provided greeting.
   - Optional: "设置备注 / 设置标签 / 设为星标" — only set if the user asked.
7. Click "发送" to send the friend request.
8. Screenshot. The dialog closes; the contact is now in pending state. Inform the user: "好友申请已发送给 XX，对方通过后会出现在通讯录"。
9. **Do not** repeatedly send requests to the same person — WeChat will throttle and may flag the account.

### Switch back to chat list

- macOS: cmd+1 jumps to the 聊天 tab (top of left sidebar)
- Windows: click the chat-bubble icon at the top of the left sidebar
- Useful after replying to one chat and you want to handle other unread messages.

## Tips

- **Confirm before send**: every send action is essentially permanent (recall window is 2 minutes and unreliable). When the user gives you a person and a message, screenshot the open conversation and confirm the title matches before pressing Enter.
- **Search-bar trap**: typing in the search bar without first clicking a result does NOT enter a chat. The text stays as a search query. You must click the result row.
- **@-mention trap**: literal "@张三" without the popup selection is plain text — does not notify. Always type "@" first, wait for the popup, then pick.
- **Default Enter**: Enter sends; Shift+Enter inserts a newline. The setting is configurable in WeChat preferences but assume the default.
- **Wrong-window typing**: if the input box wasn't focused, your text may have been typed into the search bar or the wrong chat. Always screenshot once after typing, before pressing Enter.
- **Group disambiguation**: search "张三" can return both contact 张三 and group "张三家庭群". Read the category headers to pick correctly.
- **Login state**: a screenshot showing a QR code means you're logged out. Do not try to scan or click around — tell the user, wait for them to scan.
- **Avoid clicking links** in received messages — they may open in WeChat's built-in browser and capture focus. If a link must be opened, ask the user first.
- **Throttling**: do not script bursts of friend requests or bulk DMs. WeChat anti-spam will lock the account.

## Frequency of screenshots

- After activating the window: yes
- After search query: yes (need to read results)
- After clicking a result to open chat: yes (verify the right chat opened)
- During typing: no
- Before pressing Enter: yes if the recipient is critical
- After Enter: yes (verify the bubble appeared)

## What NOT to do

- Do not scan QR codes, even if you can see them.
- Do not click the green "添加到通讯录" button without explicit user intent.
- Do not auto-reply to broadcasts, group bots, or unknown senders.
- Do not download files or click links without asking.
- Do not assume the active conversation is the one the user meant — verify the title bar.
