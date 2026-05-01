<!-- Native rsclaw web_browser format. Original procedural code adapted from
     github.com/browser-use/browser-harness (MIT). Field-tested patterns and
     selectors below are unchanged from upstream. -->

# Amazon — Product Search & Data Extraction

Field-tested against amazon.com on 2025-04-18 using a logged-in Chrome session.
No CAPTCHA or bot detection was triggered during any test run.

## Navigation

### Direct search URL (fastest, always use this)
```
action=open url=https://www.amazon.com/s?k=mechanical+keyboard
action=wait target=networkidle
action=wait ms=2000        # dynamic content needs ~2s after readyState=complete
```

### Search box typing (use when you need category filtering)
```
action=open url=https://www.amazon.com
action=wait target=networkidle
action=wait ms=1000
action=evaluate code="document.querySelector('#twotabsearchtextbox').focus(); document.querySelector('#twotabsearchtextbox').click();"
action=wait ms=300
action=type text="wireless mouse"
action=wait ms=300
action=press key=Enter
action=wait target=networkidle
action=wait ms=2000
```

### Direct product page
```
# URL pattern: /dp/{ASIN}  or  /dp/{ASIN}?th=1 (Amazon may redirect to add ?th=1)
action=navigate url=https://www.amazon.com/dp/B08Z6X4NK3
action=wait target=networkidle
action=wait ms=2000
```

## Session gotcha

**Always use `action=new_tab` when opening Amazon for the first time in a
session.** `action=navigate` can silently fail to navigate if the current
tab resists (observed when our pool reused a stale Chrome tab). Safe pattern:

```
action=new_tab url=https://www.amazon.com/s?k=mechanical+keyboard
action=wait target=networkidle
action=wait ms=2000
```

After that, `action=navigate` works fine within the same Amazon session.

## Search results extraction

### Container selector
`[data-component-type="s-search-result"]` — confirmed working, yields ~22
results per page.

### Full extraction (field-tested)
```
action=evaluate code="JSON.stringify(Array.from(document.querySelectorAll('[data-component-type=\"s-search-result\"]')).map(el => ({
  asin: el.getAttribute('data-asin'),
  title: el.querySelector('h2 span')?.innerText?.trim(),
  price: el.querySelector('.a-price .a-offscreen')?.innerText,
  list_price: el.querySelector('.a-text-price .a-offscreen')?.innerText,
  rating: el.querySelector('[aria-label*=\"out of 5 stars\"]')?.getAttribute('aria-label')?.split(' ')[0],
  reviews: el.querySelector('[aria-label*=\"ratings\"]')?.getAttribute('aria-label'),
  is_sponsored: !!el.querySelector('.puis-sponsored-label-text'),
  url: el.querySelector('h2 a')?.href
})))"
```

### Field notes
- **`asin`**: `data-asin` attribute on the container div — always present, matches the `/dp/{ASIN}` URL.
- **`title`**: `h2 span` works consistently. `h2 a.a-link-normal span` also works.
- **`price`**: `.a-price .a-offscreen` returns the formatted string e.g. `"$69.99"`. Use this, not `.a-price-whole`.
- **`list_price`**: `.a-text-price .a-offscreen` — only present when item is on sale (was/now pricing).
- **`rating`**: Use `aria-label` on `[aria-label*="out of 5 stars"]` — gives `"4.5 out of 5 stars, rating details"`, split on space for the number.
- **`reviews`**: Use `[aria-label*="ratings"]` attribute — gives `"1,514 ratings"`. Do NOT use `.a-size-base.s-underline-text` — that element exists on sponsored results and shows "Xbox" (a cross-sell widget text).
- **`is_sponsored`**: `.puis-sponsored-label-text` is present on sponsored listings; first 2-3 results are usually sponsored.
- **`url`**: `h2 a` href — contains the full `/dp/{ASIN}/...` URL.

## Product detail page extraction

### Confirmed selectors (field-tested on B08Z6X4NK3)
```
action=evaluate code="JSON.stringify({
  title: document.querySelector('#productTitle')?.innerText?.trim(),
  price: (function() {
    var whole = document.querySelector('.a-price-whole')?.innerText?.replace(/[\\n.]/g,'');
    var frac  = document.querySelector('.a-price-fraction')?.innerText;
    return (whole && frac) ? '$' + whole + '.' + frac
         : document.querySelector('.a-price .a-offscreen')?.innerText || null;
  })(),
  list_price: document.querySelector('.basisPrice .a-offscreen')?.innerText,
  rating: document.querySelector('#acrPopover')?.getAttribute('title'),
  review_count: document.querySelector('#acrCustomerReviewText')?.innerText,
  availability: document.querySelector('#availability span')?.innerText?.trim(),
  brand: document.querySelector('#bylineInfo')?.innerText?.trim(),
  asin: document.querySelector('input[name=\"ASIN\"]')?.value,
  bullet_points: Array.from(document.querySelectorAll('#feature-bullets li span.a-list-item')).map(e => e.innerText?.trim()).filter(t => t)
})"
```

### Price field notes
- `#priceblock_ourprice` and `#priceblock_dealprice` are **legacy** — they return `null` on modern product pages.
- Construct price from `.a-price-whole` + `.a-price-fraction` (both stripped of `\n` and `.`).
- As a fallback: first `.a-price .a-offscreen` on the page also works (confirmed `$69.99`).
- `list_price` from `.basisPrice .a-offscreen` shows the crossed-out "was" price when a discount exists.

## Best Sellers page

URL: `https://www.amazon.com/Best-Sellers-{Category}/zgbs/{slug}/`
e.g. `https://www.amazon.com/Best-Sellers-Electronics/zgbs/electronics/`

### DOM structure (2025)
`.zg-item-immersion` **does not exist** — Amazon migrated to CSS modules. Use
`[data-asin]` anchored on `[id="gridItemRoot"]`:

```
action=navigate url=https://www.amazon.com/Best-Sellers-Electronics/zgbs/electronics/
action=wait target=networkidle
action=wait ms=2000

action=evaluate code="JSON.stringify(Array.from(document.querySelectorAll('[data-asin]')).map(el => {
  var container = el.closest('[id=\"gridItemRoot\"]') || el;
  return {
    asin: el.getAttribute('data-asin'),
    rank: container.querySelector('[class*=\"zg-bdg-text\"]')?.innerText,
    title: container.querySelector('img[alt]')?.getAttribute('alt'),
    price: container.querySelector('.p13n-sc-price, .a-size-base.a-color-price')?.innerText,
    url: 'https://www.amazon.com/dp/' + el.getAttribute('data-asin')
  }
}).filter(r => r.rank))"
```

Note: Title comes from the product image `alt` attribute — the text title
elements use obfuscated CSS module class names that change between deployments.

## Pagination

```
# Get next page URL directly
action=evaluate code="document.querySelector('.s-pagination-next')?.href"
# If returned URL is non-empty:
action=navigate url=<that URL>
action=wait target=networkidle
action=wait ms=2000

# Or construct by page number
action=navigate url=https://www.amazon.com/s?k=wireless+mouse&page=2
```

## Result count

```
action=evaluate code="document.querySelector('[data-component-type=\"s-result-info-bar\"] h1')?.innerText?.trim()"
# Returns e.g.: '1-16 of over 40,000 results for "wireless mouse"\nSort by:\n...'
# Extract just the count: split('\n')[0] in your reasoning step.
```

## CAPTCHA detection

No CAPTCHA was encountered during testing with a logged-in Chrome session. To
detect defensively:

```
action=evaluate code="(() => { var t = (document.body.innerText || '').slice(0,500).toLowerCase(); var u = location.href.toLowerCase(); return t.includes('captcha') || t.includes('enter the characters') || t.includes('sorry, we just need to make sure') || u.includes('captcha') || u.includes('validatecaptcha'); })()"
# If true → stop, surface to the user. Don't auto-retry.
```

Amazon may serve a CAPTCHA on fresh/anonymous sessions. Using the browser's
existing logged-in session avoids this in practice.

## Gotchas

- **`navigate` silent failure**: On first visit, use `new_tab` instead. After the tab is on Amazon, `navigate` works.
- **`.zg-item-immersion` is gone**: Best Sellers page uses CSS module classes (obfuscated). Use `[data-asin]` + `img[alt]` for title.
- **`.a-size-base.s-underline-text` is unreliable for review count**: On sponsored results it shows unrelated text (e.g. "Xbox"). Use `[aria-label*="ratings"]` instead.
- **`#priceblock_ourprice` is legacy**: Returns `null` on modern pages. Construct from `.a-price-whole` + `.a-price-fraction`.
- **Sponsored results appear first**: First 2-3 results are almost always `is_sponsored: true`. Filter them out with `!el.querySelector('.puis-sponsored-label-text')` when you need organic results.
- **`data-asin` can be empty string on non-product rows**: Filter with `.filter(r => r.asin)`.
- **Price split DOM**: `.a-price-whole` innerText includes a trailing `\n.` — strip it: `.replace(/[\n.]/g,'')`.
- **ASIN from URL**: Use `/dp/([A-Z0-9]{10})/` regex on the product URL. `data-asin` on search results is always the canonical ASIN.
- **`?th=1` redirect**: Amazon appends `?th=1` (and sometimes `?psc=1`) to product URLs after redirect. This is normal — `input[name="ASIN"]` always has the clean ASIN.
- **Wait 2s after `target=networkidle`**: Amazon search results load the listing cards asynchronously. networkidle fires before cards render. A hard 2s wait is required.
