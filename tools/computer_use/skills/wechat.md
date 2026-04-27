---
name: wechat
description: WeChat (微信) desktop client - find a chat in the list, click into it, type text, press Enter to send
---

# WeChat Desktop App

Minimal scope: **find the target chat in the chat list → click into it → type message → Enter to send.** Nothing else (no @-mentions, no add-friend, no message reading) — those are out of scope for this skill.

## App Info
- macOS: WeChat.app
- Windows: WeChat.exe
- Native shell. **DO NOT call ui_tree** — its accessibility tree is shallow and you will incorrectly conclude "no elements", then give up. Always use `screenshot` + coordinate clicking.
- Must be logged in. If a QR code is on screen, stop and tell the user to scan.

## Operations

### Send a message to a contact / group

1. **Activate WeChat** (use the `exec` tool, not `open -a` which doesn't always raise the window):
   ```json
   {"tool": "exec", "command": "osascript -e 'tell application \"WeChat\" to activate' -e 'delay 0.3' -e 'tell application \"System Events\" to tell process \"WeChat\" to set frontmost to true' -e 'tell application \"System Events\" to tell process \"WeChat\" to perform action \"AXRaise\" of (window 1)'"}
   ```
   Then `wait 800ms` and `screenshot`.

2. **Verify WeChat is actually frontmost.** Look at the screenshot. The macOS menu bar should say "WeChat", and the WeChat main window should be the dominant content. **If you see any other app on top, or any modal popup blocking the chat list, STOP and tell the user. Do not start clicking.**

3. **Locate the target chat in the list (left column).**
   - The chat list is the second column from the left (after the icon sidebar). Each row is a single chat: avatar on the left, name + last message on the right.
   - If the target name is **visible** in the list: identify the row's vertical center coordinate from the screenshot. Click only on the **name area** — avoid clicking on the avatar (which can pop a contact card).
   - If **not visible**: open search with `cmd+f` (macOS) or `ctrl+f` (Windows), `type` the target name (the type action handles CJK automatically via clipboard paste), wait 800ms, screenshot, find the matching row in the dropdown under the 联系人 / 群聊 category header, click that row. **Do not press Return** on the search result — Return on the search header doesn't enter the chat; you must click the row.

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
