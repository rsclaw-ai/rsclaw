---
name: wechat
description: WeChat (微信) desktop client automation - monitor groups, auto-reply, cross-channel trigger. ALWAYS use computer_use action=ui_tars for GUI ops.
---

# WeChat Desktop Automation

## CRITICAL — Use UI-TARS for all GUI operations

For ANY WeChat GUI operation (opening the app, clicking, searching, scrolling, typing, right-clicking), **ALWAYS call `computer_use action=ui_tars`** with a natural-language instruction describing the goal.

Do NOT manually call `screenshot` + individual `click`/`type`/`key` actions. The UI-TARS vision model handles screenshot analysis, element detection, and coordinate prediction internally.

Good example:
```json
{"tool": "computer_use", "action": "ui_tars", "instruction": "Open WeChat, search for 'RsClaw研发群', enter the chat, read the last 5 messages, and report sender names and contents."}
```

## Trigger — When to activate WeChat

### 1. Direct request
User explicitly asks about WeChat:
- "send message to 文件传输助手"
- "check WeChat group messages"
- "微信群里看看", "看看微信有没有新消息"
- "reply to WeChat messages", "回复一下微信群的消息"

### 2. Cross-channel trigger (NEW)
User sends a command from ANY channel (Feishu, Slack, Telegram, etc.) with WeChat-related intent:
- Keywords: "check WeChat", "reply to WeChat", "微信群里看看", "看看微信群", "回一下微信", "微信看看"
- The user may mention a specific group name (e.g., "RsClaw研发群", "文件传输助手")
- Action: recognize the intent → call `computer_use action=ui_tars` with the target group name

## Monitor + Reply Mode

### Step 1: Check messages
Call `computer_use action=ui_tars` with instruction like:
"Open WeChat, check the group chat named 'GROUP_NAME' for new messages. Read the last 5-10 messages and report the sender names and message contents."

### Step 2: Decide whether to reply

**ONLY reply if the message @mentions the bot or is clearly directed at the bot.**

Check conditions in order:
1. The message contains `@` followed by the bot's name or nickname (e.g., `@RsClaw助手`, `@螃蟹`, `@助手`)
2. The message starts with the bot's nickname (e.g., `螃蟹，帮我...`, `助手，请问...`)
3. The message is a direct question clearly addressed to the bot in the group context

If NONE of the above match → **do NOT reply.** Just report the messages to the user.

⚠️ **Do NOT reply to general questions that don't mention the bot.** If someone asks "今天天气怎么样？" without @bot, ignore it. If they ask "@RsClaw助手 今天天气怎么样？", reply.

### Step 3: Reply with Quote (引用)

When replying to a specific message in a group, ALWAYS use Quote so members know which message you are replying to.

**Method: Keyboard navigation (preferred — avoids coordinate-clicking the menu)**
WeChat's right-click context menu is hard to hit by coordinates and causes loop clicks. Always use keyboard navigation.

Include in your ui_tars instruction:
"Right-click the message bubble from [sender] that says '[message text]'. Wait 400ms for the context menu to appear. Then press the Down arrow key **9 times**, then press Return to select Quote (引用). Wait 300ms, then type the reply."

UI-TARS action sequence for this:
1. `right_single(start_box='...')` on the message bubble
2. `wait()` for 0.5s
3. `hotkey(key='down')` repeated 9 times (or `type(content='')` with 9 newlines if down key is unavailable)
4. `hotkey(key='return')`

**Fallback: Manual exec (if UI-TARS fails)**
```json
{"tool": "exec", "command": "python3 -c \"from pynput.mouse import Controller, Button; c=Controller(); c.position=(X,Y); c.click(Button.right)\" && osascript -e 'delay 0.4' -e 'tell application \"System Events\"' -e 'repeat 9 times' -e 'key code 125' -e 'delay 0.05' -e 'end repeat' -e 'key code 36' -e 'end tell'"}
```
Replace `(X,Y)` with the message bubble center coordinates from the ui_tars result.

After Quote is selected, the quoted message appears as a grey block in the input box. Type your reply after it.

**For CJK / Chinese text**, paste via clipboard:
```json
{"tool": "exec", "command": "echo 'YOUR_MESSAGE' | pbcopy && osascript -e 'tell application \"System Events\"' -e 'tell process \"WeChat\"' -e 'keystroke \"v\" using command down' -e 'end tell' -e 'end tell'"}
```

Then press Enter to send.

### Step 4: Verify the send
After sending, ask ui_tars to verify: "Check if the message was sent successfully. The reply should appear as a green bubble at the bottom of the chat."

## Deep scan: reply to all unanswered messages from today

When the user asks to "check all today's messages" or "reply to everything unanswered":

1. Call `computer_use action=ui_tars` to open the group and read messages
2. Collect all messages from today that have no green bubble (your reply) below them
3. For each unanswered message that @mentions the bot, reply via Quote (oldest first)
4. If there are more than 3 unanswered messages, prioritize @mentions and questions. Skip casual chatter unless the user said to reply to everything.

## Pre-condition
WeChat must be logged in. If a QR code is visible, stop and ask the user to scan it first.

## Critical rules
- **Always use `computer_use action=ui_tars` for GUI operations.** Never manually screenshot+click.
- **Do NOT reply to messages that do not @mention the bot.**
- **Never claim success without verification.** Ask ui_tars to confirm the message was sent.
- **Do NOT share screenshots with the user.** Report outcomes in plain text only.
