---
name: web-scan-login
description: 自主登录需要扫码的网站（京东/淘宝/B站/抖音/微博/邮箱/云服务等26个站点），截取二维码发给用户扫码，完成后持久化登录态
version: 1.0.0
---

# Web Scan Login

你是一个网站扫码登录助手。当用户要求登录某个站点，或你在执行任务时发现需要登录，按以下流程操作。

## 核心流程

1. **检查已保存的登录态**
   - `state load` 对应站点的 state（key 为域名，如 `jd.com`）
   - 导航到目标页，检测是否仍然登录
   - 如果登录态有效 → 直接继续任务
   - 如果失效（跳回登录页、出现登录按钮）→ 进入扫码流程

2. **扫码登录流程**
   - 按下方站点指南导航到登录页
   - 处理跳转，切换到扫码模式（如需要）
   - 等待二维码出现（wait for selector/text, 最多 10s）
   - 截取二维码发给用户（见"二维码截取策略"）
   - 告诉用户："请用 XXX APP 扫码登录"
   - 轮询检测登录成功（间隔 2s，超时 120s）
   - 超时则提示用户二维码可能过期，重新获取二维码

3. **登录成功后收尾**
   - `state save` 持久化（key 为域名）
   - 向用户报告"XXX 登录成功"（一句话）
   - 立即结束登录流程，回到原任务

## 二维码截取策略

优先精准截取，兜底全页截图：

```javascript
// 优先尝试的 selector（按顺序）
const selectors = [
  'canvas[class*="qr" i]',
  'canvas[id*="qr" i]',
  'img[src*="qr"]',
  'img[class*="qr" i]',
  'img[id*="qr" i]',
  '[class*="qrcode" i]',
  '[id*="qrcode" i]',
  '[class*="qr-code" i]',
  '[class*="login-qr" i]',
  '[class*="scan-code" i]',
  'canvas',  // 很多站点用 canvas 渲染二维码
];
```

操作步骤：
1. 用 `evaluate` 执行 JS，按上述 selector 顺序查找二维码元素
2. 找到元素 → 获取其 bounding rect → 用 `screenshot` 截取该区域
3. 未找到 → `screenshot` 截取整个可视区域
4. 将截图发送给用户

## 登录成功检测

通用策略（轮询间隔 2s）：
- **URL 变化**：不再包含 `login`/`passport`/`signin` 等关键词
- **二维码消失**：二维码区域变成"扫码成功"/"确认登录" 等文案
- **用户元素出现**：页面出现头像、用户名、"退出登录" 等元素

## 站点操作指南

### 1. jd.com（京东）
- **登录 URL**: `https://passport.jd.com/new/login.aspx`
- **操作**: 页面加载后，点击"扫码登录" tab
- **QR 提示**: "请使用京东 APP 扫码登录"
- **成功判断**: URL 跳转到 jd.com 主站

### 2. douyin.com（抖音）
- **登录 URL**: `https://www.douyin.com`
- **操作**: 点击页面右上角"登录"按钮，弹窗默认显示二维码
- **QR 提示**: "请使用抖音 APP 扫码登录"
- **成功判断**: 弹窗消失，出现用户头像

### 3. taobao.com（淘宝）
- **登录 URL**: `https://login.taobao.com`
- **操作**: 页面默认显示二维码
- **QR 提示**: "请使用淘宝/支付宝 APP 扫码登录"
- **成功判断**: URL 跳转离开 login.taobao.com

### 4. bilibili.com（B站）
- **登录 URL**: `https://passport.bilibili.com/login`
- **操作**: 页面默认显示二维码
- **QR 提示**: "请使用哔哩哔哩 APP 扫码登录"
- **成功判断**: URL 跳转到 bilibili.com 主站

### 5. doubao.com（豆包）
- **登录 URL**: `https://www.doubao.com`
- **操作**: 点击"登录"按钮，跳转到字节 SSO 页面，默认显示二维码
- **二次跳转**: 是，跳转到 sso.douyin.com 或类似字节认证页
- **QR 提示**: "请使用抖音 APP 扫码登录"
- **成功判断**: URL 跳回 doubao.com 且页面出现用户信息

### 6. jimeng.jianying.com（即梦）
- **登录 URL**: `https://jimeng.jianying.com/ai-tool/home`
- **操作**: 点击"登录"按钮，跳转到字节 SSO 页面
- **二次跳转**: 是，同豆包
- **QR 提示**: "请使用抖音 APP 扫码登录"
- **成功判断**: URL 跳回 jimeng.jianying.com 且页面出现用户信息

### 7. baidu.com（百度）
- **登录 URL**: `https://passport.baidu.com/v2/?login`
- **操作**: 点击"扫码登录" tab 或短信登录旁边的二维码图标
- **QR 提示**: "请使用百度 APP 扫码登录"
- **成功判断**: URL 跳转到 baidu.com 主站

### 8. chat.baidu.com（文心一言）
- **登录 URL**: `https://chat.baidu.com`
- **操作**: 未登录会自动跳转到百度 passport，操作同 baidu.com
- **二次跳转**: 是，跳转到 passport.baidu.com
- **QR 提示**: "请使用百度 APP 扫码登录"
- **成功判断**: URL 跳回 chat.baidu.com

### 9. xiaohongshu.com（小红书）
- **登录 URL**: `https://www.xiaohongshu.com`
- **操作**: 点击右上角"登录"，弹窗默认显示二维码
- **QR 提示**: "请使用小红书 APP 扫码登录"
- **成功判断**: 弹窗消失，出现用户头像

### 10. wx.qq.com（微信网页版）
- **登录 URL**: `https://wx.qq.com`
- **操作**: 页面直接显示大二维码，无需额外点击
- **QR 提示**: "请使用微信扫码登录"
- **成功判断**: 页面出现聊天界面或联系人列表

### 11. feishu.cn（飞书）
- **登录 URL**: `https://passport.feishu.cn/suite/passport/page/login`
- **操作**: 点击"扫码登录" tab
- **QR 提示**: "请使用飞书 APP 扫码登录"
- **成功判断**: URL 跳转到 feishu.cn 应用页

### 12. smzdm.com（什么值得买）
- **登录 URL**: `https://zhiyou.smzdm.com/user/login`
- **操作**: 点击"扫码登录" 或二维码图标
- **二次跳转**: 可能从主站跳到 zhiyou.smzdm.com
- **QR 提示**: "请使用什么值得买 APP 扫码登录"
- **成功判断**: URL 跳转离开登录页

### 13. fanqienovel.com（番茄小说）
- **登录 URL**: `https://fanqienovel.com`
- **操作**: 点击"登录"按钮，跳转到字节 SSO 页面
- **二次跳转**: 是，同豆包/即梦
- **QR 提示**: "请使用抖音 APP 扫码登录"
- **成功判断**: URL 跳回 fanqienovel.com 且出现用户信息

### 14. iqiyi.com（爱奇艺）
- **登录 URL**: `https://passport.iqiyi.com/user/login`
- **操作**: 点击"扫码登录" tab
- **QR 提示**: "请使用爱奇艺 APP 扫码登录"
- **成功判断**: URL 跳转到 iqiyi.com 主站

### 15. youku.com（优酷）
- **登录 URL**: `https://www.youku.com`
- **操作**: 点击"登录"，跳转到阿里巴巴统一登录页，显示二维码
- **二次跳转**: 是，跳转到 login.alibaba.com 或 login.taobao.com
- **QR 提示**: "请使用淘宝/支付宝 APP 扫码登录"
- **成功判断**: URL 跳回 youku.com 且出现用户信息

### 16. v.qq.com（腾讯视频）
- **登录 URL**: `https://v.qq.com`
- **操作**: 点击"登录"，跳转到 QQ 登录页，点击"扫码登录"
- **二次跳转**: 是，跳转到 ssl.xui.ptlogin2.qq.com 或类似
- **QR 提示**: "请使用 QQ/微信扫码登录"
- **成功判断**: URL 跳回 v.qq.com 且出现用户信息

### 17. weibo.com（微博）
- **登录 URL**: `https://passport.weibo.com/sso/signin`
- **操作**: 如果默认不是扫码模式，点击"扫码登录"
- **QR 提示**: "请使用微博 APP 扫码登录"
- **成功判断**: URL 跳转到 weibo.com 主站

### 18. mail.163.com / mail.126.com（网易邮箱）
- **登录 URL**: `https://mail.163.com` 或 `https://mail.126.com`
- **操作**: 点击"扫码登录" tab 或二维码图标
- **QR 提示**: "请使用网易邮箱大师 APP 扫码登录"
- **成功判断**: URL 跳转到邮箱收件箱页面

### 19. mail.qq.com（QQ 邮箱）
- **登录 URL**: `https://mail.qq.com`
- **操作**: 页面显示 QQ 登录，点击"扫码登录"
- **QR 提示**: "请使用 QQ/微信扫码登录"
- **成功判断**: URL 进入邮箱主界面

### 20. exmail.qq.com（腾讯企业邮箱）
- **登录 URL**: `https://exmail.qq.com/login`
- **操作**: 点击"微信扫码登录"
- **QR 提示**: "请使用微信扫码登录"
- **成功判断**: URL 跳转到企业邮箱主界面

### 21. mail.aliyun.com（阿里邮箱企业版）
- **登录 URL**: `https://qiye.aliyun.com`（个人版 mail.aliyun.com 无扫码，需进企业版）
- **操作**: 点击登录框中的"Scan to sign in with Alimail"按钮，显示二维码
- **QR 提示**: "请使用阿里邮箱/钉钉 APP 扫码登录"
- **成功判断**: URL 跳转到企业邮箱收件箱页面

### 22. mail.sina.com.cn（新浪邮箱）
- **登录 URL**: `https://mail.sina.com.cn`
- **操作**: 点击"扫码登录"或微博登录入口
- **QR 提示**: "请使用微博 APP 扫码登录"
- **成功判断**: URL 跳转到邮箱主界面

### 23. cn.aliyun.com（阿里云）
- **登录 URL**: `https://account.aliyun.com/login/login.htm`
- **操作**: 点击页面右上角"阿里云APP"按钮，右侧出现二维码
- **QR 提示**: "请使用阿里云 APP 扫码登录"
- **成功判断**: URL 跳转到 cn.aliyun.com 控制台

### 24. cloud.tencent.com（腾讯云）
- **登录 URL**: `https://cloud.tencent.com/login`
- **操作**: 页面默认显示"微信登录" tab，直接展示二维码，无需额外点击
- **QR 提示**: "请使用微信扫码登录"
- **成功判断**: URL 跳转到 console.cloud.tencent.com

### 25. www.ucloud.cn（UCloud 优刻得）
- **登录 URL**: `https://passport.ucloud.cn/login`
- **操作**: 页面右侧直接显示"扫码登录"区域和二维码，无需额外点击
- **QR 提示**: "请使用 UCloud APP 扫码登录"
- **成功判断**: URL 跳转到 console.ucloud.cn

### 26. www.huaweicloud.com（华为云）
- **登录 URL**: `https://www.huaweicloud.com/intl/zh-cn/`
- **操作**: 点击右上角"登录"，跳转到华为云 auth 页面，页面自动显示扫码二维码（88秒过期，注意及时扫码）
- **二次跳转**: 是，跳转到 auth.huaweicloud.com
- **QR 提示**: "请使用华为云 APP 扫码登录"
- **成功判断**: URL 跳回 www.huaweicloud.com 且出现用户信息

## 重要注意事项

1. **使用 headed 模式**: 扫码登录必须用 `headed: true`，部分站点在 headless 下会被检测拦截
2. **等待页面加载**: 每次导航后用 `wait` 等待页面稳定再操作
3. **弹窗处理**: 部分站点有 cookie 同意弹窗或广告弹窗，先关闭再操作
4. **二维码刷新**: 如果二维码过期（通常 60-120s），页面上会有"刷新"或"点击重新获取"按钮，点击后重新截图发给用户
5. **不要输入密码**: 本 skill 仅处理扫码登录，不涉及账号密码
6. **上下文控制**: 登录是原子操作，完成后立即结束，不做额外页面浏览
7. **适应性操作**: 上述 selector 和步骤是指导性的，站点可能改版。如果按指南操作失败，用 `snapshot` 查看当前页面结构，自适应找到登录入口和二维码
