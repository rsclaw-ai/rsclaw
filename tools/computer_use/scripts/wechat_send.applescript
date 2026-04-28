-- WeChat desktop send-message helper.
--
-- Usage (via osascript):
--   osascript wechat_send.applescript "<chat name>" "<message>"
--
-- Bypasses computer_use vision-coordinate guessing entirely by driving
-- WeChat through its accessibility tree. Reliable on macOS WeChat 4.x.
--
-- Returns "ok: row <N>" on success, or "error: <reason>" on failure.

on run argv
  if (count of argv) < 2 then
    return "error: usage: wechat_send.applescript <chat_name> <message>"
  end if
  set targetName to item 1 of argv
  set msg to item 2 of argv

  tell application "WeChat" to activate
  delay 0.3

  tell application "System Events" to tell process "WeChat"
    set frontmost to true
    if (count of windows) is 0 then return "error: no WeChat window"
    perform action "AXRaise" of (window 1)

    try
      set sg to first UI element of window 1 whose role is "AXSplitGroup"
      set sa to first UI element of sg whose role is "AXScrollArea"
      set tbl to first UI element of sa whose role is "AXTable"
    on error
      return "error: chat list not found in AX tree"
    end try

    -- find the row whose inner AXRow title starts with the target name
    set foundIdx to 0
    repeat with i from 1 to (count of rows of tbl)
      try
        set rw to row i of tbl
        set ec to entire contents of rw
        repeat with elem in ec
          try
            if role of elem is "AXRow" then
              set t to title of elem
              if t is not missing value then
                set AppleScript's text item delimiters to ","
                set chatName to text item 1 of t
                set AppleScript's text item delimiters to ""
                if chatName is targetName then
                  set foundIdx to i
                  exit repeat
                end if
              end if
            end if
          end try
        end repeat
        if foundIdx is not 0 then exit repeat
      end try
    end repeat

    if foundIdx is 0 then return "error: chat not found: " & targetName

    -- select the row -> opens the conversation in the right pane
    set selected of (row foundIdx of tbl) to true
    delay 0.5

    -- paste the message via clipboard (handles CJK reliably; AppleScript
    -- `keystroke` with non-ASCII goes through input methods and gets mangled)
    set the clipboard to msg
    delay 0.1
    keystroke "v" using command down
    delay 0.3

    -- send (Return key)
    key code 36

    return "ok: row " & foundIdx
  end tell
end run
