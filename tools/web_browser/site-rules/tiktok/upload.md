<!-- Native rsclaw web_browser format. Original procedural code adapted from
     github.com/browser-use/browser-harness (MIT). Field-tested patterns and
     selectors below are unchanged from upstream. -->

# TikTok Studio — Upload Video

URL: `https://www.tiktok.com/tiktokstudio/upload?from=upload&lang=en` (always
append `&lang=en`)

## Prerequisites

- Logged into TikTok in Chrome (use the persistent profile)
- Video file on local disk (mp4, <50MB)

## Stale draft banner

TikTok shows "A video you were editing wasn't saved" if a previous upload was
abandoned. Dismiss it:

1. Find the banner Discard button (y < 300 on the page)
2. `action=clickAt x=<x> y=<y>` on it
3. A confirmation modal appears — find the red Discard button (y > 300) and `action=clickAt x=<x> y=<y>`
4. Repeat if multiple stale drafts are stacked

## Upload flow

### 1. Attach file

```
action=upload selector='input[type="file"]' path='/path/to/video.mp4'
action=wait ms=12000   # processing takes ~10s for 5-10MB
```

### 2. Caption

TikTok pre-fills caption with the filename. Clear it first:

```
action=evaluate code="document.querySelector('div[contenteditable=\"true\"][role=\"combobox\"]').focus()"
action=press key=End
# Repeat 25× to clear the prefilled filename
action=press key=Backspace
... (×25)
action=type text="your caption here #hashtag1 #hashtag2"
action=press key=Escape       # dismiss hashtag suggestions
action=clickAt x=700 y=50     # click away to deselect
```

Verify:
```
action=evaluate code="document.querySelector('div[contenteditable=\"true\"][role=\"combobox\"]').innerText"
```

### 3. Schedule

Click the Schedule radio label:
```
action=evaluate code="(()=>{var l=document.querySelectorAll('label');for(var i=0;i<l.length;i++){if(l[i].textContent.trim()==='Schedule'){l[i].click();break}}})()"
```

**Time picker** — uses a scroll-wheel list, NOT a native select. Each
`action=scroll dy=32` steps +1 unit, `dy=-32` steps -1 unit.

```
# 1. ScrollIntoView and open the time picker
action=evaluate code="document.querySelector('input[type=\"time\"]').scrollIntoView({block:'center'})"
action=clickAt x=<time_input_x> y=<time_input_y>

# 2. Default time is in the input's value attribute (read it, e.g. 13:05)
# 3. Compute step counts in your reasoning. Then scroll hour column (left, x≈349):
action=scroll x=349 y=<dropdown_y> dy=32   # +1 hour per call, repeat (target_hour - default_hour) times

# 4. Scroll minute column (right, x≈437):
action=scroll x=437 y=<dropdown_y> dy=32   # +5 minutes per call, repeat (target_min - default_min) // 5 times

# 5. Close and verify
action=press key=Escape
```

**Date picker** — click the date input, then click the target day number span.

### 4. AI-generated content disclosure

Under "Show more" section. Toggle is `[aria-checked]` inside the "AI-generated
content" parent.

```
# Expand settings
action=evaluate code="(()=>{const s=[...document.querySelectorAll('span')].find(e=>e.textContent.trim()==='Show more');s&&s.click();})()"

# ScrollIntoView the toggle
action=evaluate code="(()=>{const s=[...document.querySelectorAll('span')].find(e=>e.textContent.toLowerCase().includes('ai-generated content'));s&&s.scrollIntoView({block:'center'});})()"

# Read state, then clickAt the toggle if false
# A "Turn on" confirmation dialog may appear — clickAt to confirm
```

### 5. Submit

Scroll the Schedule button into view, then `action=clickAt`. After success,
page redirects to `/tiktokstudio/content`.

```
action=evaluate code="(()=>{const b=[...document.querySelectorAll('button')].find(e=>e.offsetWidth>100 && e.textContent.includes('Schedule'));b&&b.scrollIntoView({block:'center'});})()"
action=clickAt x=<button_x> y=<button_y>
action=wait ms=6000
action=get_url   # confirm contains "content"
```

## Gotchas

- **JS `.click()` doesn't work on TikTok's time picker items** — must use `action=clickAt`.
- **Time picker uses virtual scroll** — `action=scroll dy=32` changes value, NOT regular DOM scroll.
- **Caption contenteditable appends on type** — always clear with `End + Backspace ×25` first, never set `innerHTML` (breaks React state).
- **beforeunload dialog** blocks navigation if upload is in progress — `action=accept_dialog` (see `_VOCABULARY.md` for unhandled-dialog handling, or use `evaluate` with `Page.handleJavaScriptDialog` if exposed).
- **Schedule button text** is "Schedule" only after the Schedule radio is selected (otherwise "Post").
- **"Show more" section** expands the page and pushes the time picker off-viewport — collapse it before adjusting time, expand after.
- **Unicode narrow no-break space** (char 8239) appears between time and AM/PM in scheduled post listings — use `.indexOf('12:30')` not exact string match.
