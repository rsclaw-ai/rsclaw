# Skill Vocabulary — read this BEFORE any other site-rule

Most files in this directory were imported from
[browser-use/browser-harness](https://github.com/browser-use/browser-harness)
(MIT) and were originally executed by their Python harness. **You read them
as reference; you execute through rsclaw's `web_browser` tool actions.**

This page maps browser-harness helpers → rsclaw `web_browser` actions and
documents the few patterns that don't translate 1:1.

## Treat skill code as pseudocode

The Python in each skill is *what worked in their harness*. For you it's
high-fidelity pseudocode: extract the intent (URL pattern, selector,
sequence, wait points, traps) and execute via `web_browser` actions.

## Browser-automation helpers → rsclaw actions

| browser-harness helper | rsclaw `web_browser` action |
|---|---|
| `goto_url(url)` | `action=open url=...` (first call) or `action=navigate url=...` (already in a tab) |
| `new_tab(url)` | `action=new_tab url=...` |
| `switch_tab(idx \| match)` | `action=switch_tab` |
| `page_info()` | `action=get_url` + `action=get_title` (combine) |
| `click_at_xy(x, y, button=, clicks=)` | `action=clickAt x=X y=Y button=left clicks=N` |
| `type_text("...")` | `action=type text='...'` |
| `press_key("Enter", modifiers=4)` | `action=press key='Enter' modifiers='Meta'` (1=Alt, 2=Ctrl, 4=Meta, 8=Shift; combine with `+`) |
| `scroll(x, y, dy=-300)` | `action=scroll x=X y=Y dy=-300` (or use `action=scrollintoview ref=@e1` for an element) |
| `js("...")` (no return) | `action=evaluate code='...'` |
| `js("(()=>{...; return X})()")` | `action=evaluate code='...'` — returned value comes back in tool result |
| `wait(s)` | `action=wait ms=s*1000` |
| `wait_for_load()` | `action=wait target='networkidle'` |
| `upload_file(selector, path)` | `action=upload selector='...' path='...'` |
| `capture_screenshot(path, full=, max_dim=)` | `action=screenshot` (returns image_path) |
| `cdp("Page.navigate", url=...)` | Use the high-level action above; only fall back to raw CDP if no equivalent exists. Raw CDP is not exposed — pick the closest action. |
| `drain_events()` | not exposed; use `action=console` for log-style messages |

## Selector & element discovery

Skills use plain CSS / XPath selectors with `document.querySelector`. Two
ways to act on them in rsclaw:

1. **By selector directly** — many actions accept a `selector=` arg
   (`fill`, `upload`, `clickAt selector=`, etc.).
2. **By ref** — `action=snapshot` returns interactive elements with refs
   like `@e1`, `@e10`. Most actions also accept `ref=@eN`. Refs change
   after every page mutation, so re-snapshot after click/fill/type.

Semantic locators when refs change rapidly:
- `action=getbytext value='Submit'`
- `action=getbyrole value='button'`
- `action=getbylabel value='Email'`

## HTTP / private-API calls

Many skills bypass the DOM and hit a site's private JSON API directly via:

```python
http_get(url, headers={...})
json.loads(...)
```

In rsclaw, fetch via the **`web_fetch` tool** (not `web_browser`):

| browser-harness | rsclaw |
|---|---|
| `http_get(url, headers={...})` | `web_fetch url='...' headers='...'` |
| `json.loads(resp)` then dict access | `web_fetch` already returns parsed text/JSON; index into the result |

When skills say "the page calls an XHR to `/api/v2/foo` — call it directly
with auth cookie", you do that via `web_fetch` with `cookies` from the
current browser session (run `action=cookies` in `web_browser` first to
extract them).

## Python data ops

`re.findall`, `json.loads`, `.strip()`, `.split()`, list comprehensions,
etc. are the agent's own to translate — extract data from `web_fetch` /
`web_browser action=evaluate` result, then process in your reasoning step
or via `exec` tool if heavy.

## What doesn't translate

| browser-harness | rsclaw |
|---|---|
| `agent_helpers.py` edits | Not supported — we don't have a writable scratch module. Use the `tools_file` to write site-specific notes back to `site-rules/<host>.md` (self-authoring). |
| Direct CDP calls (`cdp("Network.enable")`) | Not exposed; if a skill *requires* a CDP detail we don't expose, mention it in the new site-rule and find a high-level workaround. |
| `BU_NAME` / multi-profile remote browsers | Not yet exposed in rsclaw; all sessions share one Chrome. |

## Path layout

- `<site>/<task>.md` — single workflow on a single site (browser-harness layout, kept as-is)
- `<site>.md` — flat, single-file site rule (rsclaw legacy; both layouts coexist)

When you `open` a URL whose host matches `site-rules/<host>/` (any subfile)
or `site-rules/<host>.md`, read the relevant file BEFORE touching the
page — saves 5+ snapshot/click iterations and avoids stale-selector
breakage.

## Self-authoring

If you discover a non-obvious mechanic that wasn't in any skill, write
it back to `site-rules/<host>/<task>.md` (or extend an existing file)
using rsclaw's native action vocabulary. Don't paste pixel coordinates;
describe how to *locate* the target (selector / `getbyrole` / visible
text). The harness gets better only because agents file what they learn.
