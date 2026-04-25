---
name: tonghuashun
description: Tonghuashun (同花顺) stock trading desktop app - quotes, charts, trading, analysis
---

# Tonghuashun Desktop App (同花顺)

## App Info
- Bundle: 同花顺.app
- UI framework: Native macOS (NOT Electron)
- ui_tree: Returns ~200 elements - usable for sidebar buttons and tab labels
- Theme: dark (black/red/green stock colors)
- Window default: fullscreen 1440x816

## Layout

### Left Sidebar (~55px wide, icon + text vertically)
From top to bottom:
- 同花顺 logo (红色扑克牌)
- 自选 (Watchlist) - default view
- 个股 (Individual stock)
- 行情 (Market quotes)
- 全球 (Global markets)
- 期货 (Futures)
- 交易 (Trading)
- 选股 (Stock screener)
- 发现 (Discover)
- 决策 (Decision/Strategy)

### Top Bar
- Left: 同花顺 logo, mail icon
- Center: Search box "行情 / 功能 / 问句" with "问 AI" button
- Right: notification icons, settings, user

### Tab Bar (below top bar, context-dependent)

**In 自选 view:**
```
自选 | 智能选股 | 全景看盘 | 多股报价 | 关联报价 | 短线擒龙 | +
```

**Sub-tabs (below tab bar, in 自选):**
```
自选股 | 价投 | 今日关注 | 特别关注 | 持仓股 | 龙头股 | 昨日涨停 | 今日涨停 | 昨日二板 | ...
```

### Main Content Area (自选 view)

**Left panel - Stock table (~780px wide):**
- Columns: 筛选 | 代码 | 名称 | 涨幅% | 现价 | 分类标示 | 量比 | 涨速% | 换手% | 涨停原因
- Rows: numbered stock list with color coding (red=up, green=down)
- Click row to select stock and update right panel
- Double-click row to enter individual stock detail view

**Right panel - Stock detail (~660px wide):**
- Header: stock name, code, current price, change
- Tab row: 一自选 | 简况 | 资料 | 社区
- Below tabs: 大事提醒 section with announcements
- Mini chart: 分时走势图 (intraday line chart)
- Chart period tabs: 分时 | 日K | 周K | 月K | 120分 | 60分 | 30分
- Below mini chart: 关联指数 info
- Volume chart at bottom

### Bottom Status Bar
Real-time index ticker:
```
沪指 xxxx.xx | 深指 xxxxx.xx | 创指 xxxx.xx | 科创 xxxx.xx | AI炒股攻略 | 反馈 | CN HH:MM:SS
```

## Operations

### View watchlist
1. Click "自选" in left sidebar (first icon below logo)
2. Stock table loads with your watchlist
3. Click any stock row to see details in right panel

### Search for a stock
1. Click search box at top center (or press keyboard shortcut)
2. Type stock code (e.g., 000565) or name (e.g., 渝三峡)
3. Select from dropdown results
4. Stock detail view loads

### Ask AI about stocks
1. Click "问 AI" button next to search box
2. Type natural language question about stocks/market
3. AI response appears

### Enter individual stock chart (个股详情页)
1. Click "个股" in left sidebar, OR
2. Type stock code in search box and press Enter
3. Full chart page loads with: left stock list, center K-line chart, right order book

**个股详情页 layout:**

Top tab bar:
```
自选股 | 盯盘 | 闪时 | 分时 | 日K | 周K | 月K | 120分 | 60分 | ... | 同花顺F10 | 诊股 | 显示 | 画线
```

Center - main chart area:
- K-line/candlestick chart with MA overlays (MA5/MA10/MA20/MA30)
- Shows price axis on right, time axis on bottom
- Below K-line: volume bars + selected indicator sub-chart (default: MACD)

MA indicator line at chart top:
```
MA MA5:x.xx MA10:x.xx MA20:x.xx MA30:x.xx [前复权 checkbox]
```

Indicator selector bar (below chart, above news):
```
VOL | 成交额 | 均线 | 大单净量 | MACD | KDJ | 主力资金 | 筹码融资
```

Secondary tabs (below indicators):
```
量价组合 | 主力意图 | 机构驾驶 | 筹码
```

News/info tabs (below indicator area):
```
新闻资讯 | 关联股票 | 持股基金 | 关联期商 | 快速交易 | 股市便签
```

Right panel - order book (盘口):
- Tabs: 盘口 | 资料 | 社区 + 买入/卖出 buttons + 展开
- 买卖档位与明细: 5-level bid/ask (买一~买五, 卖一~卖五) with price/volume/time
- Below: 行情数据 (开盘/昨收/最高/最低/换手/量比/金额)
- 所属板块 info

Bottom of page:
```
经典模式 | 超短定制 | 巴菲特理念 | 趋势投机 | 董事长视角 | +
```

### Switch chart timeframe
1. Click period tabs in top bar: 分时 | 日K | 周K | 月K | 120分 | 60分
2. Chart redraws with selected timeframe
3. "分时" shows intraday line chart, others show K-line/candlestick

### Switch chart indicators (副图指标)
1. Click indicator name in the indicator selector bar below the chart
2. Available indicators:
   - **VOL** - Volume bars (default)
   - **成交额** - Turnover amount
   - **均线** - Moving averages
   - **大单净量** - Large order net volume
   - **MACD** - MACD(12,26,9) with DIFF/DEA/histogram
   - **KDJ** - KDJ stochastic
   - **主力资金** - Institutional money flow
   - **筹码融资** - Chip distribution / margin data
3. Secondary indicator tabs: 量价组合 | 主力意图 | 机构驾驶 | 筹码

### Use drawing tools (画线)
1. Click "画线" tab in top tab bar (rightmost)
2. Drawing toolbar appears (trend lines, horizontal lines, channels, etc.)
3. Click and drag on chart to draw
4. Right-click drawing to edit/delete

### Show/hide chart overlays (显示)
1. Click "显示" tab in top tab bar (next to 画线)
2. Toggle K-line annotations, pattern detection, etc.

### Chart right-click menu
Right-click on chart area to access:
- 删除自选股 (Remove from watchlist)
- 加入分组 > (Add to group)
- 显示K线异动 (Show K-line anomalies)
- 显示形态洞察 (Show pattern recognition)
- 切换坐标 > (Switch coordinate type)
- 翻转坐标 (Flip coordinates)
- 叠加品种 (Overlay another symbol)
- Color markers (红/橙/黄/绿/蓝/紫)
- 文字标记 (Text annotation)
- 股市便签 (Stock notes)

### View stock fundamentals (F10)
1. Click "同花顺F10" tab in top bar, OR press F10
2. Loads detailed fundamentals: financials, shareholders, dividends, etc.

### Run stock diagnosis (诊股)
1. Click "诊股" tab in top bar
2. AI-powered stock analysis and rating

### Switch analysis mode
1. Click mode buttons at bottom of individual stock page:
   - **经典模式** - Standard chart + indicators
   - **超短定制** - Short-term trading focus
   - **巴菲特理念** - Value investing metrics
   - **趋势投机** - Trend following signals
   - **董事长视角** - Management/insider perspective
   - **+** - Add custom mode

### View market overview
1. Click "行情" in left sidebar
2. Market indices, sector performance, rankings load

### Use stock screener
1. Click "选股" in left sidebar
2. Or click "智能选股" tab in top bar
3. Set screening criteria
4. View filtered results

### View global markets
1. Click "全球" in left sidebar
2. International indices, forex, commodities load

### View futures
1. Click "期货" in left sidebar
2. Futures contracts and quotes load

### Open trading
1. Click "交易" in left sidebar
2. Trading panel opens (may require login/authentication)
3. Enter order: stock code, price, quantity
4. Confirm and submit

### View full chart (全景看盘)
1. Click "全景看盘" tab in top bar
2. Multi-panel market overview with sector heatmaps

### Short-term strategy (短线擒龙)
1. Click "短线擒龙" tab in top bar
2. Short-term momentum stock picks

### Add stock to watchlist
1. Search for stock
2. Click the star/自选 button on stock detail page

### Sort stock table
1. Click any column header in stock table (涨幅%, 现价, 量比, etc.)
2. Toggles ascending/descending sort

## Keyboard Shortcuts
- F5: Refresh quotes
- F10: Stock fundamentals
- Enter on stock row: Enter individual stock view
- Esc: Go back
- Number keys: Quick stock code input
- 01+Enter: Shanghai A-shares ranking
- 02+Enter: Shenzhen A-shares ranking
- 03+Enter: Shanghai B-shares
- 04+Enter: Shenzhen B-shares
- 06+Enter: Growth Enterprise Board (创业板)
- 60+Enter: All A-shares ranking

## Tips
- Native macOS app: ui_tree returns ~200 elements, sidebar buttons and tab labels are detectable
- Sidebar buttons have generic "按钮" label but precise coordinates — use y-position to identify
- Stock table sub-tabs (自选股/价投/今日关注) have text labels in ui_tree
- Color coding: red = price up, green = price down (Chinese market convention, opposite of Western)
- 涨停 = daily limit up (+10%), 跌停 = daily limit down (-10%)
- Real-time data updates continuously; screenshots capture a moment in time
- "两融" marker on stocks means margin trading eligible
- Bottom status bar always shows major index real-time prices
