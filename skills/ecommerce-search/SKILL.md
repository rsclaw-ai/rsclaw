---
name: ecommerce-search
description: 在京东/淘宝/天猫/抖音商城搜索商品，获取 top-k 结果（名称、价格、链接、销量）
version: 1.0.0
icon: "🛒"
author: "@rsclaw"
---

# 电商搜索

你是一个电商比价助手。当用户要求搜索商品、比价、找最低价时，按以下流程操作。

## 核心流程

1. **判断平台** — 用户指定了就用指定的，否则默认搜京东（不需要登录）
2. **检查登录态** — 淘宝/天猫/抖音需要登录，先检查 state
3. **搜索并提取结果** — 导航到搜索页，用 JS 提取商品列表
4. **格式化返回** — 表格形式展示 top-k 结果

## 平台操作指南

### 京东（jd.com）— 无需登录

**搜索步骤：**
1. `open` 导航到 `https://search.jd.com/Search?keyword={query}&enc=utf-8`
2. `wait` 3000ms（等待商品列表渲染）
3. `evaluate` 执行提取脚本（见下方）

**提取脚本：**
```javascript
(function(){
  var items = [];
  document.querySelectorAll('#J_goodsList .gl-item, .gl-warp .gl-item, [data-sku]').forEach(function(el, i){
    if(i >= 20) return;
    var name = (el.querySelector('.p-name a em, .p-name a') || {}).textContent || '';
    var price = (el.querySelector('.p-price strong i, .p-price .J_price') || {}).textContent || '';
    var link = (el.querySelector('.p-name a') || {}).href || '';
    var shop = (el.querySelector('.p-shop a, .p-shopnum a') || {}).textContent || '';
    var commit = (el.querySelector('.p-commit a') || {}).textContent || '';
    if(name.trim()) items.push({name: name.trim().substring(0, 80), price: price.trim(), link: link, shop: shop.trim(), sales: commit.trim()});
  });
  return JSON.stringify(items);
})()
```

**如果上述选择器失败（京东改版）：**
1. `snapshot` 获取页面结构
2. 查找包含商品信息的列表容器
3. 自适应编写提取 JS

### 淘宝（taobao.com）— 需要登录

**登录检查：**
1. `state load` key=`taobao.com`
2. `open` 导航到 `https://s.taobao.com/search?q=test`
3. 如果 URL 被重定向到 `login.taobao.com` → 触发 `web-scan-login` skill 登录
4. 如果页面正常显示搜索结果 → 登录态有效

**搜索步骤：**
1. `open` 导航到 `https://s.taobao.com/search?q={query}&s=0`
2. `wait` 3000ms
3. 处理可能的验证滑块（如果出现 `nc-container` 或 `baxia-dialog`）
4. `evaluate` 执行提取脚本

**提取脚本：**
```javascript
(function(){
  var items = [];
  // 新版淘宝
  document.querySelectorAll('[class*="Card--"] a[href*="item.taobao"], [class*="Card--"] a[href*="detail.tmall"], .Content--content a[href*="item"]').forEach(function(el, i){
    if(i >= 20) return;
    var card = el.closest('[class*="Card--"]') || el.parentElement;
    var name = (card.querySelector('[class*="Title--"], [class*="title"]') || {}).textContent || '';
    var price = (card.querySelector('[class*="Price--"], [class*="price"]') || {}).textContent || '';
    var shop = (card.querySelector('[class*="Shop--"], [class*="shop"]') || {}).textContent || '';
    var sales = (card.querySelector('[class*="Sale--"], [class*="sale"], [class*="realSales"]') || {}).textContent || '';
    var link = el.href || '';
    if(name.trim()) items.push({name: name.trim().substring(0, 80), price: price.trim(), link: link.split('&')[0], shop: shop.trim(), sales: sales.trim()});
  });
  // 旧版兜底
  if(items.length === 0) {
    document.querySelectorAll('.items .item, .m-itemlist .items .item').forEach(function(el, i){
      if(i >= 20) return;
      var name = (el.querySelector('.title a, .J_ClickStat') || {}).textContent || '';
      var price = (el.querySelector('.price strong, .g_price strong') || {}).textContent || '';
      var link = (el.querySelector('.title a, .J_ClickStat') || {}).href || '';
      var shop = (el.querySelector('.shop a') || {}).textContent || '';
      var sales = (el.querySelector('.deal-cnt') || {}).textContent || '';
      if(name.trim()) items.push({name: name.trim().substring(0, 80), price: price, link: link, shop: shop.trim(), sales: sales.trim()});
    });
  }
  return JSON.stringify(items);
})()
```

### 天猫（tmall.com）— 需要登录（共享淘宝登录态）

**搜索步骤：**
1. 确认淘宝登录态（天猫和淘宝共享登录）
2. `open` 导航到 `https://list.tmall.com/search_product.htm?q={query}`
3. `wait` 3000ms
4. `evaluate` 执行提取脚本

**提取脚本：**
```javascript
(function(){
  var items = [];
  document.querySelectorAll('.product, [class*="Product"], [data-id]').forEach(function(el, i){
    if(i >= 20) return;
    var name = (el.querySelector('.productTitle a, [class*="Title"] a, .product-title') || {}).textContent || '';
    var price = (el.querySelector('.productPrice em, [class*="Price"] em, .product-price') || {}).textContent || '';
    var link = (el.querySelector('.productTitle a, [class*="Title"] a') || {}).href || '';
    var shop = (el.querySelector('.productShop a, [class*="Shop"]') || {}).textContent || '';
    var sales = (el.querySelector('.productStatus span, [class*="Sale"]') || {}).textContent || '';
    if(name.trim()) items.push({name: name.trim().substring(0, 80), price: price.trim(), link: link, shop: shop.trim(), sales: sales.trim()});
  });
  return JSON.stringify(items);
})()
```

### 抖音商城（douyin.com/mall）— 需要登录

**登录检查：**
1. `state load` key=`douyin.com`
2. `open` 导航到 `https://www.douyin.com`
3. 如果页面出现登录弹窗或未登录状态 → 触发 `web-scan-login` skill
4. 登录后 `state save` key=`douyin.com`

**搜索步骤：**
1. `open` 导航到 `https://www.douyin.com/search/{query}?type=general`
2. `wait` 3000ms
3. 点击"商品" tab（如果有）：`click` 包含"商品"文字的 tab
4. `wait` 2000ms
5. `evaluate` 执行提取脚本

**提取脚本：**
```javascript
(function(){
  var items = [];
  document.querySelectorAll('[class*="goods"], [class*="product"], [class*="card"]').forEach(function(el, i){
    if(i >= 20) return;
    var name = '';
    var price = '';
    var link = '';
    var sales = '';
    // 尝试多种选择器
    el.querySelectorAll('a').forEach(function(a){
      if(a.href && a.href.includes('mall') && !link) link = a.href;
    });
    var texts = el.innerText.split('\n').filter(function(t){ return t.trim(); });
    texts.forEach(function(t){
      t = t.trim();
      if(!name && t.length > 5 && t.length < 100 && !/^\d|^¥|^￥|已售/.test(t)) name = t;
      if(!price && /^[¥￥]?\d+\.?\d*$/.test(t.replace(/[¥￥,]/g, ''))) price = t;
      if(!sales && /已售|销量|付款/.test(t)) sales = t;
    });
    if(name) items.push({name: name.substring(0, 80), price: price, link: link, shop: '', sales: sales});
  });
  return JSON.stringify(items);
})()
```

## 结果格式

搜索完成后，以表格形式展示：

```
| # | 商品名称 | 价格 | 店铺 | 销量 |
|---|---------|------|------|------|
| 1 | xxx     | ¥xx  | xxx  | xxx  |
```

附上商品链接方便用户点击查看。

## 多平台比价

当用户要求"比价"或"哪个最便宜"时：
1. 依次在京东、淘宝搜索同一关键词
2. 提取各平台 top-5 结果
3. 按价格排序，标注平台来源
4. 给出购买建议

## 翻页

如果用户要求更多结果：
- 京东：`open` `https://search.jd.com/Search?keyword={query}&page={2n-1}`（page=1,3,5,7...）
- 淘宝：`open` `https://s.taobao.com/search?q={query}&s={n*44}`（s=0,44,88...）
- 天猫：`open` URL 加 `&s={n*60}`
- 抖音：向下滚动 `evaluate` `window.scrollTo(0, document.body.scrollHeight)` 然后 `wait` 2000ms

## 重要注意事项

1. **自适应**: 电商站点频繁改版，上述 JS 选择器是参考。如果提取到 0 条结果，先 `snapshot` 查看页面结构，根据实际 DOM 调整选择器
2. **反爬处理**: 如果页面显示验证码或空白，`wait` 5000ms 后重试。连续失败 3 次则告知用户
3. **headed 模式**: 淘宝/抖音建议用 `headed: true`，headless 更容易被检测
4. **价格过滤**: 用户如果指定价格范围，在 URL 参数中添加（京东: `&ev=exprice_{min}-{max}`，淘宝: `&filter=reserve_price[{min},{max}]`）
5. **排序**: 京东排序参数 `&psort=3`(价格升序) `&psort=4`(价格降序) `&psort=5`(销量)
6. **编码**: 搜索关键词需要 URL 编码，使用 `encodeURIComponent`
