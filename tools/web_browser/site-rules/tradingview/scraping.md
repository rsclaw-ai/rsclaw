<!-- Native rsclaw web_browser format. Original procedural code adapted from
     github.com/browser-use/browser-harness (MIT). Field-tested patterns and
     selectors below are unchanged from upstream. -->

# TradingView — Scraping & Data Extraction

`https://www.tradingview.com` — charting platform with multiple internal REST
APIs. Stock/crypto/forex screener and symbol search work without auth. **Use
`web_fetch` for everything except chart screenshots / auth-gated pages**.

## Do this first

**Use the scanner API for bulk screener data — one POST, no browser, full
column control.**

```
web_fetch \
  url=https://scanner.tradingview.com/america/scan \
  method=POST \
  headers='{"Content-Type":"application/json","User-Agent":"Mozilla/5.0"}' \
  body='<JSON payload, see below>'
```

**No auth, no Referer, no cookies required for the scanner.** Responses arrive
in ~200ms.

## Common workflows

### Top stocks by market cap (screener)

Payload:
```json
{
  "filter": [],
  "options": {"lang": "en"},
  "columns": ["name", "close", "change", "volume", "market_cap_basic"],
  "sort": {"sortBy": "market_cap_basic", "sortOrder": "desc"},
  "range": [0, 10]
}
```

Call:
```
web_fetch \
  url=https://scanner.tradingview.com/america/scan \
  method=POST \
  headers='{"Content-Type":"application/json","User-Agent":"Mozilla/5.0"}' \
  body=<the JSON above>
```

Response shape:
- `resp.totalCount` ≈ 19549 (all US-listed instruments)
- `resp.data` is a list of `{"s": "NASDAQ:NVDA", "d": [col0, col1, ...]}`
- `"d"` values align positionally with `"columns"` in your payload — zip them in your reasoning step:
  ```
  for item in resp.data:
    row = dict(zip(columns, item.d))
    # row["close"], row["change"], etc.
  ```

**Critical**: `"d"` is a plain positional array — index 0 = columns[0], index 1
= columns[1], etc. There are no keys in the row data itself.

### Pagination

`range` is half-open. Page 1: `[0, 20]`. Page 2: `[20, 40]`. (And `[0, 10]`
returns rows 0–9 = 10 rows total.)

### Filtering stocks

```json
{
  "filter": [
    {"left": "market_cap_basic", "operation": "greater", "right": 10000000000},
    {"left": "volume",           "operation": "greater", "right": 5000000},
    {"left": "change",           "operation": "in_range", "right": [2, 10]},
    {"left": "exchange",         "operation": "equal",   "right": "NASDAQ"},
    {"left": "sector",           "operation": "equal",   "right": "Electronic Technology"}
  ],
  "columns": ["name", "close", "change", "volume", "market_cap_basic",
              "description", "sector", "industry"],
  "sort": {"sortBy": "market_cap_basic", "sortOrder": "desc"},
  "range": [0, 20]
}
```

Valid filter operations: `greater`, `less`, `equal`, `in_range` (right = [min,
max]), `match` (substring on `name`).

Sector names use TradingView taxonomy (not GICS). Confirmed working values:
- `"Electronic Technology"` — NVDA, AAPL, TSM
- `"Technology Services"` — MSFT, GOOGL, META
- `"Finance"`, `"Health Technology"`, `"Consumer Non-Durables"`

### Tested valid column names

```
# Price & volume
"name"                     # ticker (e.g. "AAPL")
"description"              # full name ("Apple Inc.")
"close"                    # last price
"open", "high", "low"
"volume"
"change"                   # % change today
"change_abs"               # absolute price change
"change|1M"                # 1-month % change (also: |6M, |1Y)
"High.1M", "High.6M"       # period high
"High.All", "Low.All"      # all-time high/low
"price_52_week_high"       # confirmed works
"price_52_week_low"        # confirmed works
"premarket_change"         # pre-market %
"postmarket_change"        # after-hours %
"gap"                      # overnight gap %
"change_from_open_abs"     # intraday move from open
"average_volume_10d_calc"  # 10-day avg volume
"relative_volume_10d_calc" # relative volume vs 10-day avg
"relative_volume_intraday|5"  # intraday relative vol (5m bars)

# Fundamentals
"market_cap_basic"          # market cap in USD
"earnings_per_share_diluted_ttm"  # EPS TTM
"price_earnings_ttm"        # P/E TTM
"P/E"                       # P/E (snapshot)
"dividends_yield"           # dividend yield %
"beta_1_year"               # beta
"float_shares_outstanding"  # float shares

# Technical ratings & indicators
"Recommend.All"   # composite rating: -1 (strong sell) to +1 (strong buy)
"RSI"             # RSI 14
"MACD.macd"       # MACD line

# Classification
"sector", "industry", "country", "exchange"
"type"    # "stock", "fund", "dr" (depository receipt), etc.

# NOTE: "52_week_high" / "52_week_low" are INVALID — use "price_52_week_high" / "price_52_week_low"
# NOTE: "EPS_diluted_net" is INVALID — use "earnings_per_share_diluted_ttm"
```

Bad columns return HTTP 400 with `{"error": "Unknown field \"X\""}`.

### Other scanner markets

`market` path segment options (confirmed working):
- `america`  — US equities (~19,549 instruments)
- `crypto`   — crypto across exchanges (~56,455 instruments)
- `forex`    — FX pairs (~6,401 instruments)
- `futures`  — futures (~53,947 instruments)

Crypto example payload:
```json
{
  "filter": [],
  "columns": ["name", "close", "change", "volume", "market_cap_calc"],
  "sort": {"sortBy": "market_cap_calc", "sortOrder": "desc"},
  "range": [0, 10]
}
```
Call: `web_fetch url=https://scanner.tradingview.com/crypto/scan ...`

### Symbol search (requires Origin header)

```
web_fetch \
  url='https://symbol-search.tradingview.com/symbol_search/v3/?text=AAPL&hl=1&exchange=&lang=en&search_type=undefined&domain=production' \
  headers='{"User-Agent":"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36","Origin":"https://www.tradingview.com"}'
```

`symbol-search.tradingview.com` requires `Origin: https://www.tradingview.com`.
Referer alone is not enough. The scanner API does NOT need Origin or Referer.

Response shape:
- `result.symbols_remaining` ≈ 137 (quota counter)
- `result.symbols` = list of up to 50 matches
- Per-symbol keys: `symbol, description, type, exchange, country, currency_code, cusip, isin, cik_code, logoid, provider_id, source_id, is_primary_listing, typespecs`

Filter by exchange and type:
```
# Exact match on NASDAQ:AAPL
url='.../symbol_search/v3/?text=AAPL&exchange=NASDAQ&search_type=stock&...'
```

### News headlines for a symbol

```
web_fetch \
  url='https://news-headlines.tradingview.com/v2/view/headlines/symbol?symbol=NASDAQ:AAPL&client=web&streaming=false&lang=en&limit=10' \
  headers='{"User-Agent":"Mozilla/5.0"}'
```

Response: `data.items` — list of news items. Per-item keys: `id, title,
provider, sourceLogoId, published (unix ts), source, urgency, link, permission,
relatedSymbols, storyPath`.

No auth or special headers needed. Returns up to 200 items per request.

### Published trading ideas feed

```
web_fetch \
  url='https://www.tradingview.com/api/v1/ideas/?lang=en&sort=trending&page=1' \
  headers='{"User-Agent":"Mozilla/5.0"}'
# Optional: append &symbol=NASDAQ:AAPL
```

Valid sort values (others return 400): `trending`, `recent`, `latest_popular`,
`week_popular`, `suggested`, `recent_extended`, `picked_time`.

Response shape:
- `data.count` always 1000 (soft cap)
- `data.page_size` = 20
- `data.page_count` = 50
- `data.results` = list of ideas
- Per-idea keys: `id, name, description, created_at, chart_url, views_count, likes_count, comments_count, is_video, is_education, is_hot, symbol (dict with name/exchange/type/interval/direction), user (dict with username/is_pro/badges), image (big/middle URLs)`
- `idea.symbol.direction`: 1=long, 2=short, 0=neutral

## API summary table

| Endpoint | Auth | Headers needed | Speed |
|---|---|---|---|
| `scanner.tradingview.com/{market}/scan` | None | None | ~200ms |
| `symbol-search.tradingview.com/symbol_search/v3/` | None | `Origin: https://www.tradingview.com` | ~150ms |
| `symbol-search.tradingview.com/symbol_search/` (v1) | None | `Origin: https://www.tradingview.com` | ~100ms |
| `news-headlines.tradingview.com/v2/view/headlines/symbol` | None | None | ~400ms |
| `www.tradingview.com/api/v1/ideas/` | None | None | ~300ms |
| `data.tradingview.com/quotes/` | None | None | **Dead** — connection refused |
| `economic-calendar.tradingview.com/events` | Yes | — | HTTP 403 |

## Gotchas

- **Scanner `range` is half-open**: `[0, 10]` returns rows 0–9 (10 rows total). `[10, 20]` for the next page.
- **Column order is critical**: the `"d"` array in each result row is positional — it exactly mirrors your `"columns"` array. Always zip them.
- **`data.tradingview.com/quotes/` is dead**: closes the connection without a response. Use the scanner API instead for real-time quotes.
- **Scanner needs no Referer**: `scanner.tradingview.com` works with just `User-Agent`. The symbol-search subdomain checks `Origin` (CORS enforcement on the server side).
- **Symbol search highlights**: the v3 endpoint wraps matched text in `<em>` tags (e.g. `"<em>AAPL</em>"`). Strip them in your reasoning step.
- **Ideas sort validation**: only specific values work. `sort=popular` returns 400.
- **Ideas count cap**: the API always reports `count=1000` regardless of actual corpus size. With `page_size=20`, max pages is 50.
- **Scanner server is AWS CloudFront** (`X-Amz-Cf-Pop` header) with a custom `Server: tv` — no Cloudflare. No anti-bot on the scanner subdomain. Main `www.tradingview.com` is a React SPA with `window.initData = {}` (empty — no embedded data). All data is loaded via API calls after hydration.
- **Rate limits**: No 429s observed in testing. 5 concurrent scanner calls complete in ~1s. Symbol search returns `symbols_remaining` in the response (counts against some quota — varies 90–180 across calls but never blocks). Observed no blocking after 15 rapid calls in a row.
- **Sector names**: use TradingView's own taxonomy, not GICS. "Technology" does not exist — use `Electronic Technology` (hardware/semis) or `Technology Services` (software/internet).

## When to use the browser

The charting UI (`/chart/`), symbol detail pages (`/symbols/NASDAQ-AAPL/`), and
the ideas page (`/ideas/`) are React SPAs — their visible data comes from the
APIs above, not embedded HTML. Use the browser only when you need visual chart
screenshots or data from auth-gated pages (watchlists, portfolio, paper
trading).

```
# Only if you need a chart screenshot:
action=open url=https://www.tradingview.com/chart/?symbol=NASDAQ:AAPL
action=wait target=networkidle
action=wait ms=3000          # chart renders asynchronously
action=screenshot
```
