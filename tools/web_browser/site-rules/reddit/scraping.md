<!-- Native rsclaw web_browser format. Original procedural code adapted from
     github.com/browser-use/browser-harness (MIT). Field-tested patterns and
     selectors below are unchanged from upstream. -->

# Reddit — Scraping & Post Extraction

Reddit's "new" web UI (`reddit.com`) is a Lit / web-components SPA built around
custom elements (`shreddit-post`, `shreddit-comment`, `faceplate-*`). This makes
DOM extraction unusually reliable — the custom element tags are stable and
exposed on the element itself (no hashed class names).

Use the browser when you're logged in (private subreddits, NSFW gates,
rate-limit avoidance). For fully public content, the JSON API path below is
faster.

## URL patterns

- Full post: `https://www.reddit.com/r/<sub>/comments/<id>/<slug>/`
- Share short-link: `https://www.reddit.com/r/<sub>/s/<hash>` — redirects to the full URL once the page loads. `action=new_tab` + `action=wait target=networkidle` is enough; by the time you read `location.href` it will be the canonical one.
- Old Reddit: append `/.json` to any post URL for anonymous JSON: `https://www.reddit.com/r/<sub>/comments/<id>/.json`.
- Old UI (simpler DOM, no web components): `https://old.reddit.com/r/<sub>/comments/<id>/` — useful fallback when `shreddit-*` selectors change.

## Path 1: JSON API (fastest for public posts)

Use the `web_fetch` tool, NOT `web_browser`:

```
web_fetch url=https://www.reddit.com/r/cursor/comments/1l0u9y7/<slug>/.json headers='{"User-Agent":"Mozilla/5.0"}'
```

Then parse in your reasoning step:
- `data[0].data.children[0].data` → post fields: `title`, `selftext`, `author`, `score`, `num_comments`, `created_utc`, `url`, `permalink`
- `data[1].data.children` → list of `{ kind: "t1", data: {...} }` (comment) or `{ kind: "more" }` (placeholder)

Fails on:
- Private / quarantined subreddits (401)
- NSFW posts without an authenticated session
- Anti-scraping 429s under load — back off or switch to the browser path

## Path 2: Browser DOM extraction (logged-in)

Core selector: every post renders inside a single `<shreddit-post>` custom
element. Top-level comments are `<shreddit-comment depth="0">`.

```
action=new_tab url=https://www.reddit.com/r/vibecoding/comments/1kwuqpz/
action=wait target=networkidle
action=wait ms=3000        # SPA still hydrating after networkidle

# Scroll to force comment tree lazy-load (twice, ~2000px each)
action=scroll x=500 y=500 dy=2000
action=wait ms=1000
action=scroll x=500 y=500 dy=2000
action=wait ms=1000

action=evaluate code="(()=>{
  const postEl = document.querySelector('shreddit-post');
  if(!postEl) return null;
  const title = (postEl.querySelector('h1, [slot=\"title\"]')||{}).innerText?.trim() || '';
  const bodyEl = postEl.querySelector('[slot=\"text-body\"] .md, [slot=\"text-body\"]');
  const body = bodyEl ? bodyEl.innerText.trim() : '';
  const author = (postEl.querySelector('[slot=\"authorName\"] a, a[data-testid=\"post_author_link\"]')||{}).innerText?.trim() || '';
  const subM = location.pathname.match(/^\\/r\\/([^\\/]+)/);
  const subreddit = subM ? subM[1] : '';
  const scoreEl = postEl.querySelector('faceplate-number');
  const score = scoreEl ? scoreEl.getAttribute('number') || scoreEl.innerText : '';
  const comments = [];
  for(const c of document.querySelectorAll('shreddit-comment[depth=\"0\"]')){
    const cBodyEl = c.querySelector('[slot=\"comment\"] .md, [slot=\"comment\"]');
    const cBody = cBodyEl ? cBodyEl.innerText.trim() : '';
    if(!cBody) continue;
    comments.push({
      author: c.getAttribute('author') || '',
      score: c.getAttribute('score') || '',
      body: cBody
    });
    if(comments.length >= 10) break;
  }
  return JSON.stringify({subreddit, title, author, score, body, comments, url: location.href});
})()"
```

### Key selectors

| Target                 | Selector                                                              | Notes                                                                                                   |
| ---------------------- | --------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| Post container         | `shreddit-post`                                                       | One per post page. Attributes include `post-title`, `post-id`, `subreddit-name`, `author`.              |
| Post title             | `shreddit-post h1` or `[slot="title"]`                                | H1 is also the page title.                                                                              |
| Post text body         | `shreddit-post [slot="text-body"] .md`                                | `.md` is the rendered markdown container. For link posts this selector returns null (there is no body). |
| Post author name       | `[slot="authorName"] a`                                               | Plain text.                                                                                             |
| Vote score             | `shreddit-post faceplate-number`                                      | Read the `number` attribute (digit string) — `innerText` is abbreviated ("1.2k").                       |
| Top-level comment      | `shreddit-comment[depth="0"]`                                         | Depth is an attribute — `depth="1"` is a reply, etc.                                                    |
| Comment body           | `shreddit-comment [slot="comment"] .md`                               | Same pattern as post body.                                                                              |
| Comment author / score | `shreddit-comment` attributes: `author`, `score`, `created-timestamp` | Use `getAttribute`, not DOM descendants.                                                                |

### Share links

`/s/<hash>` URLs redirect before the SPA mounts. You don't need to resolve them
manually — just `action=new_tab` + `action=wait target=networkidle` +
`action=wait ms=2000`, then `action=get_url` for the canonical path.

### Comment tree lazy-loading

New Reddit renders only the initial visible comments. To get more, **scroll
twice**. `ensureReplies` / `more` placeholders exist but clicking them is
brittle; scroll is the most reliable trigger. For a deep thread, loop scroll +
wait until `shreddit-comment` count stabilizes between passes.

### Login / gate detection

```
action=evaluate code="(()=>{
  const loginWall = !!document.querySelector('a[href*=\"/login\"], [data-testid=\"login-button\"]');
  const ageGate = !!document.querySelector('[data-testid=\"nsfw-gate\"], shreddit-interstitial');
  return JSON.stringify({loginWall, ageGate});
})()"
```

If `ageGate` is true and you are logged in but haven't opted into NSFW content,
the gate blocks extraction — toggle NSFW in account settings, not
programmatically.

## Gotchas

- **`faceplate-number.innerText` is abbreviated** ("1.2k", "16.6k"). Always prefer `getAttribute('number')` for the exact digit count.
- **`shreddit-comment` is a custom element, not a `<div>`.** CSS descendant selectors still work, but older jQuery-style parent traversals may not — stick to standard DOM.
- **`depth="0"` is a string attribute.** `[depth="0"]` in a CSS selector works; `depth=0` (no quotes) also works in the newer parser, but the quoted form is safest.
- **Collapsed comments render with body still in the DOM, but behind `expando-button`.** The `.md` selector still grabs the text — you don't need to expand.
- **Post body can be empty.** For link posts or image posts, `[slot="text-body"]` doesn't exist; null-check before reading `.innerText`.
- **`wait target=networkidle` is not enough.** Reddit sometimes paints the post skeleton before the content hydrates. Add `wait ms=2000`–`wait ms=3000` after networkidle, or retry reads when `shreddit-post` is null.
- **Share URLs (`/s/<hash>`) can't be deep-linked into a comment.** They always land at the post top. If the original raindrop captured `/s/...`, the in-DOM permalink (read from `location.href` after load) is the canonical URL worth storing.
- **Old Reddit (`old.reddit.com`) is a separate DOM** — no `shreddit-*` elements, no `faceplate-*`. If your login session was established on new Reddit, `old.reddit.com` will still honor the cookie.
- **For NSFW or quarantined subs**, the browser path requires your account to have opted in. The JSON API requires OAuth with appropriate scope.
- **`[slot="text-body"] .md .md`** — Reddit occasionally double-wraps; the selector `[slot="text-body"] .md` is the outer one and is what you want. Using `[slot="text-body"]` alone works too, but may include meta text.
