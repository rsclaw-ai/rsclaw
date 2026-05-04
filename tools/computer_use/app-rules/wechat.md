---
name: wechat
description: WeChat (微信) desktop client - send messages and monitor group chats for auto-reply via desktop automation
---

# WeChat Desktop App

Scope includes two modes:

1. **Send mode** (original): find the target chat → click into it → type message → Enter to send.
2. **Monitor + reply mode** (NEW): scan the chat list for unread messages → enter the group → read recent conversation → decide if reply needed → type and send reply.

Nothing else (no @-mentions, no add-friend, no adding bot to group) — WeChat personal account cannot be added as a bot to groups; all group interaction is via desktop automation.

## App Info
- macOS: WeChat.app
- Windows: WeChat.exe
- Native shell. **DO NOT call ui_tree** — its accessibility tree is shallow and you will incorrectly conclude "no elements", then give up. Always use `screenshot` + coordinate clicking.
- Must be logged in. If a QR code is on screen, stop and tell the user to scan.

## Trigger

Use this skill in two scenarios:

1. **Direct computer_use execution** — The agent is already in a computer_use session and needs to send a WeChat message or monitor a group.
2. **Cross-channel command** — The user sends a command from any channel (Feishu, Slack, Telegram, etc.) such as:
   - "check WeChat group", "微信群里看看", "看看微信有没有新消息"
   - "reply to WeChat messages", "回复一下微信群的消息"
   - The user mentions a specific group name like "RsClaw研发群"
   
   When receiving such a command, activate computer_use (if not already active) and run the Monitor + reply workflow below.

## Operations

### Send a message to a contact / group

1. **Activate WeChat.** If the window is hidden or minimized, `activate` alone may not raise it. Use this robust sequence:
   ```json
   {"tool": "exec", "command": "osascript -e 'tell application \"WeChat\" to activate' -e 'delay 0.3' -e 'tell application \"WeChat\" to reopen' -e 'delay 0.3' -e 'tell application \"System Events\" to tell process \"WeChat\" to set frontmost to true'"}
   ```
   If the window still doesn't appear (e.g. after the app was hidden with Cmd+H), fall back to Spotlight:
   ```json
   {"action": "key", "key": "cmd+space"}
   ```
   `wait 300ms`, `type "WeChat"`, `wait 300ms`, `key "Return"`.
   Then `wait 800ms` and `screenshot`.

2. **Verify WeChat is actually frontmost and identify the current view.** Look at the screenshot carefully:
   - The macOS menu bar should say "WeChat", and the WeChat main window should be the dominant content.
   - **Identify which view you are in:**
     - **Chat list view**: Left column shows a list of chat rows (avatar + name + last message). Right pane may be empty or show a grey "WeChat" placeholder. This is where you need to be.
     - **Chat window view**: Right pane shows a conversation with message bubbles (green on right = your messages, white/grey on left = others). Title bar at top shows a contact/group name. An input box is at the bottom. **If you see this view, you are INSIDE a specific chat — you must return to the chat list first (see below).**
   - **If you see any other app on top, or any modal popup blocking the window, STOP and tell the user. Do not start clicking.**

   **How to return to the chat list from inside a chat:**
   - **Click anywhere in the left column** (the chat list area) to return focus there. Do NOT press Esc — it does nothing in WeChat.
   - Or click the back button if visible in the top-left of the right pane.
   - After clicking, `wait 300ms`, re-activate WeChat, `screenshot` to confirm you are now in the chat list view.

3. **Locate the target chat in the chat list (left column).**
   - The chat list is the leftmost column showing rows: avatar on the left, name + last message on the right.
   - Scan the visible rows for the target name.
   - If the target name is **visible**: identify the row's vertical center coordinate from the screenshot. Click only on the **name/text area** — avoid clicking on the avatar (which opens a contact card). After clicking, proceed to step 4.
   - **If NOT visible, use search (do NOT type randomly in the window):**
     1. Press `cmd+f` (macOS) or `ctrl+f` (Windows) to open the chat search box at the top of the chat list.
     2. `type` the exact target name (e.g. "RsClaw研发群"). The `type` action auto-pastes CJK text via clipboard.
     3. `wait 1000ms` for results to appear, then `screenshot`.
     4. Look at the dropdown results under the **联系人** or **群聊** category header. Find the row matching the target name.
     5. **Click that row** to enter the chat. **Do NOT press Return** — Return on the search box does not enter the chat; you must click the matching result row.
     6. After clicking, proceed to step 4.

4. **Verify the chat opened.** After the click, `wait 500ms`, re-activate WeChat (focus may have shifted), `wait 200ms`, `screenshot`. Look at the **right pane title bar** — it must show the target name. If not, the click landed wrong; do not type.

5. **Click the input box.** It's the wide horizontal bar at the bottom of the right pane, with emoji / file / image icons on the left. Click somewhere in the middle of that bar. `wait 300ms`.

6. **Type the message.** The `type` action automatically uses clipboard paste for non-ASCII text — CJK works fine.
   ```json
   {"action": "type", "text": "your message here"}
   ```
   `wait 500ms`, re-activate, `wait 200ms`, `screenshot`. Verify the input box now contains the text.

7. **Press Enter to send.** WeChat default: Enter sends, Shift+Enter newlines.
   ```json
   {"action": "key", "key": "Return"}
   ```
   `wait 1000ms`, re-activate, `screenshot`. Verify a new green bubble appeared at the bottom of the chat with the text you sent. The input box should now be empty.

   **Alternative (macOS, faster):** Once the chat is open and the input box is known to be focused, you can use AppleScript within the WeChat process to type and send in one step without intermediate screenshots:
   ```json
   {"tool": "exec", "command": "osascript -e 'tell application \"System Events\"' -e 'tell process \"WeChat\"' -e 'click at {800, 730}' -e 'delay 0.2' -e 'keystroke \"hello from computer_use\"' -e 'delay 0.2' -e 'key code 36' -e 'end tell' -e 'end tell'"}
   ```
   `key code 36` is the Return key. Adjust `{800, 730}` to your window's input box location (bottom center of the right pane).

   ⚠️ **CJK / Chinese text:** `keystroke` cannot input Chinese characters. For Chinese replies, use clipboard paste instead:
   ```json
   {"tool": "exec", "command": "echo '你的中文回复' | pbcopy && osascript -e 'tell application \"System Events\"' -e 'tell process \"WeChat\"' -e 'click at {800, 730}' -e 'delay 0.2' -e 'keystroke \"v\" using command down' -e 'delay 0.2' -e 'key code 36' -e 'end tell' -e 'end tell'"}
   ```

### Monitor group chats and auto-reply

WeChat personal accounts cannot be added as bots to groups. Use desktop automation to monitor the chat list, detect unread messages in target groups, read conversation context, and reply when appropriate.

**Pre-condition:** The user tells you which group names to monitor (e.g., "RsClaw研发群"). Keep this list in mind for the session.

1. **Activate WeChat** using the same robust sequence as Send mode (step 1).

2. **Screenshot the chat list.** The left column shows all chats. Look for these unread indicators:
   - A **red badge with a number** on the right side of a chat row = unread messages
   - A **red dot** (without number) = unread messages
   - The chat name or last-message text may appear **bold** = unread
   - Compare group names against the target list. If a target group has no unread indicator, skip it.

3. **Click the target group row** in the chat list (same as Send mode step 3 — click the name area, not the avatar). `wait 500ms`, re-activate, screenshot. Verify the right pane title bar shows the group name.

4. **Read the conversation.** Look at the right pane (message area). Recent messages appear from top (older) to bottom (newer). The bot's own messages are green bubbles on the right; others' messages are white/grey bubbles on the left. Identify:
   - Who sent the most recent messages
   - Whether the message is a question or asks for help
   - Whether the bot was explicitly mentioned (e.g., @nickname, or the question is clearly directed at the bot)
   - Whether the bot has already replied to this thread (green bubble at bottom)

   💡 **For clearer text reading**, take a region screenshot of just the right pane conversation area:
   ```json
   {"tool": "exec", "command": "osascript -e 'tell application \"System Events\" to tell process \"WeChat\" to return (position of window 1) as string'"}
   ```
   The right pane starts around `win_x + 270` and is about `win_w - 270` wide. Capture the conversation area (excluding title bar and input box):
   ```json
   {"tool": "exec", "command": "screencapture -R 500,150,600,520 /tmp/chat_area.png"}
   ```
   Adjust x/y based on the actual window position from the screenshot. Read the region screenshot for clearer message text.

5. **Decide whether to reply.** Reply ONLY if ALL of these are true:
   - The most recent message is from someone else (not the bot)
   - The message **explicitly @mentions the bot by name** (e.g., "@RsClaw助手", "@螃蟹") OR clearly addresses the bot (e.g., "螃蟹，帮我...")
   - The message looks like a question or request for help
   - The bot has NOT already replied to this specific message (no green bubble directly below it)
   
   ⚠️ **Do NOT reply to general questions that don't mention the bot.** If someone asks "今天天气怎么样？" without @bot, ignore it. If they ask "@RsClaw助手 今天天气怎么样？", reply.
   
   If any condition is false, do not reply. Skip to step 7.

6. **Reply via Quote (引用).** In group chats, always use Quote so members know which message you are replying to.

   **Method A: Keyboard Navigation (Recommended — reliable regardless of menu position)**
   - Right-click the target message bubble with pynput. A context menu appears.
   - The menu items for a plain text message are (top to bottom):
     1. Copy
     2. Enlarge
     3. Translate
     4. Search
     5. Forward...
     6. Add to Favorites
     7. Select...
     8. Reminder
     9. **Quote** ← target
     10. Sticky
     11. Recall
     12. Delete
   - After the menu appears (`wait 400ms`), press the **Down arrow 9 times** to reach "Quote", then press **Enter**.
   - AppleScript one-liner for the navigation:
     ```json
     {"tool": "exec", "command": "python3 -c \"from pynput.mouse import Controller, Button; c=Controller(); c.position=(900,400); c.click(Button.right)\" && osascript -e 'delay 0.4' -e 'tell application \"System Events\"' -e 'repeat 9 times' -e 'key code 125' -e 'delay 0.05' -e 'end repeat' -e 'key code 36' -e 'end tell'"}
     ```
     Adjust `(900,400)` to the center of the target message bubble in the right pane.

   **Method B: Coordinate Click (Faster, but menu position varies with message location)**
   - Right-click the message, `wait 300ms`.
   - The menu appears near the click point; "Quote" is roughly 180-220 px below the menu top.
   - ⚠️ Only use this if keyboard navigation is confirmed not to work for your WeChat version.

   After Quote is selected:
   - The quoted message appears as a grey block in the input box.
   - **Type your reply** after the grey quote block. For Chinese text, use clipboard paste:
     ```json
     {"tool": "exec", "command": "echo '你的中文回复' | pbcopy && osascript -e 'tell application \"System Events\"' -e 'tell process \"WeChat\"' -e 'keystroke \"v\" using command down' -e 'end tell' -e 'end tell'"}
     ```
   - `wait 500ms`, screenshot, verify the input box contains both the grey quote block and your text.
   - **Press Enter** to send. `wait 1000ms`, screenshot, verify the green bubble appeared with the quote + reply.

   ⚠️ **Fallback:** If Quote fails after 2 attempts (screenshot shows no grey quote block in input box), fall back to **direct reply with @mention**: start your reply with `@sender_name` to indicate who you're replying to.

7. **Return to chat list for next group.** Click anywhere in the chat list column (left side) to return focus there, or use `cmd+1` (macOS) to jump to the first chat. Screenshot again and repeat from step 2 for the next target group with unread messages.

8. **When done monitoring all target groups, report briefly:** which groups were checked, whether any replies were sent, and a one-line summary of what was replied to.

### Deep scan: reply to all unanswered messages from today

Use this when the user explicitly asks to "check all today's messages" or "scroll up and reply to everything unanswered".

After entering the group (step 3 above), you are at the **bottom** of the conversation (newest messages). Do not reply immediately — first collect all unanswered messages.

1. **Read the visible messages** in the right pane. Work from bottom to top within the current view. For each message that is from someone else (white/grey bubble on the left):
   - Check whether there is a **green bubble (your reply)** between this message and the **next user's message** below it
   - If NO green bubble exists, this message is **unanswered** — note its content and approximate position

2. **Scroll up to load earlier messages.** First click the center of the right pane message area to focus it, then press Page Up to scroll:
   ```json
   {"tool": "exec", "command": "osascript -e 'tell application \"System Events\"' -e 'tell process \"WeChat\"' -e 'click at {850, 400}' -e 'delay 0.2' -e 'key code 116' -e 'end tell' -e 'end tell'"}
   ```
   `key code 116` is Page Up. `wait 600ms`, re-activate, screenshot. Read the newly revealed messages and mark any unanswered ones.

3. **Repeat scrolling** until you see a grey timestamp that says **"昨天"** or a date like **"5月2日"** — that means you've reached yesterday's messages. Stop scrolling.

4. **Reply from oldest to newest via Quote.** You now have a list of unanswered messages. Start with the **oldest** (top-most) one:
   - If the target message is not visible, click the message area and use `key code 121` (Page Down) to scroll down and bring it into view
   - **Right-click** the target message bubble → click **"引用"** (Quote) → type reply after the grey quote block → press Enter to send
   - `wait 1000ms`, screenshot, verify the green bubble contains the quoted message + your reply
   - **Then scroll back up** to find the next unanswered message and repeat
   - If Quote menu clicking fails after 2 attempts, use **direct reply with @mention** as fallback

5. **Pace yourself.** If there are more than 3 unanswered messages, reply to the most important ones (questions, @mentions) first. Skip casual chatter unless the user explicitly said to reply to everything.

## Critical rules

- **Re-activate WeChat between every action.** `screencapture` captures whatever app is frontmost AT THE MOMENT of the call — not the app you ran `activate` on five seconds ago. Any user keyboard activity, system notification, or background event can shift focus. Wrap each screenshot and each input action with a fresh activate to be sure.

- **Trust the screenshot, not your assumption.** After every click, re-screenshot and verify visually. If the right pane title doesn't match what you expected, do not proceed — re-screenshot, identify what actually opened, and either retry the click or report the mismatch.

- **Never claim success when you can't verify it.** If the final screenshot doesn't show your message bubble at the bottom of the chat, the send failed. Say so. Do not write "✅ 发送成功" based on intent rather than evidence.

## Avoid

- `ui_tree` — returns nothing useful for WeChat.
- Clicking on avatars — opens contact cards, blocks the chat list.
- Pressing Return / Enter on a search dropdown header — does not enter a chat.
- Cmd+N — opens "Create Group Chat" dialog, not a normal chat. Does not help.
- `open -a WeChat` alone — opens the app but may not bring already-running windows forward.
