---
name: tonghuashun
description: Tonghuashun (同花顺) stock trading desktop app - quotes, charts, trading, analysis
---

# Tonghuashun Desktop — Domain Knowledge & Pitfalls

## App framework

- Bundle: `同花顺.app`
- UI: **Native macOS (NOT Electron).** `ui_tree` returns ~200 elements
  — sidebar buttons, tab labels, and table sub-tabs are detectable.
  Unlike Doubao/Douyin, you CAN partially rely on ui_tree here.
- Sidebar buttons carry a generic "按钮" label but precise coordinates
  — use y-position to identify which one.
- Theme: dark (black background, red/green stock colors).

## Critical color convention (Chinese market)

**红涨绿跌** — RED = price UP, GREEN = price DOWN. This is the opposite
of Western markets and is the #1 source of misreading screenshots. Never
interpret red as a loss.

- 涨停 = daily limit up (+10%, +5% for ST stocks, +20% for ChiNext/STAR)
- 跌停 = daily limit down (−10% / −5% / −20% under same rules)
- "两融" marker = stock is margin-trading eligible.

## Chart timeframes and indicators

Chart period tabs: `分时 / 日K / 周K / 月K / 120分 / 60分 / 30分`.
`分时` = intraday line chart (NOT candlestick); all others are K-line
(candlestick) charts.

Sub-chart indicators (below main K-line):
- **VOL** — volume bars (default).
- **MACD** — MACD(12,26,9) with DIFF/DEA/histogram.
- **KDJ** — KDJ stochastic.
- **主力资金** — institutional money flow.
- **大单净量** — net flow of large orders.
- **筹码融资** — chip distribution & margin data.
- **成交额** — turnover amount (in money, not shares).
- **均线** — moving averages overlay.

Secondary indicator tabs: `量价组合 / 主力意图 / 机构驾驶 / 筹码`.

## Analysis modes (bottom of individual-stock page)

Pre-set "lenses" that re-skin the chart/indicators:

- **经典模式** — standard chart + indicators.
- **超短定制** — short-term trading focus.
- **巴菲特理念** — value investing metrics.
- **趋势投机** — trend following signals.
- **董事长视角** — management/insider perspective.

Pick the mode that matches the user's investment style before reading
the screen.

## Keyboard shortcuts (much faster than UI clicks)

- `F5` — refresh quotes.
- `F10` (or "同花顺F10" tab) — stock fundamentals (financials,
  shareholders, dividends).
- `Enter` on a stock row — open individual-stock detail page.
- `Esc` — go back.
- Number-then-Enter for quick navigation:
  - `01+Enter` Shanghai A-shares ranking
  - `02+Enter` Shenzhen A-shares ranking
  - `03+Enter` Shanghai B-shares
  - `04+Enter` Shenzhen B-shares
  - `06+Enter` Growth Enterprise Board (创业板)
  - `60+Enter` All A-shares ranking
- Direct stock code input — type 6-digit code in search box.

## Non-obvious knowledge

- Bottom status bar always shows real-time prices for 沪指 / 深指 /
  创指 / 科创 — use it as a quick market-temperature check.
- Real-time data refreshes continuously; screenshots are point-in-time.
  If a user asks for "current price", re-screenshot rather than trust
  a 10s-old image.
- "诊股" tab runs AI-powered analysis on the displayed stock — useful
  for one-line summaries.
- "智能选股" / "选股" run the stock screener.
- Right-click on chart area opens a context menu with: 删除自选股,
  加入分组, 显示K线异动, 显示形态洞察, 切换坐标, 翻转坐标, 叠加品种,
  color markers, 文字标记.
