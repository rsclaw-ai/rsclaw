---
name: football-register
description: 世界杯竞猜联赛报名 参赛 加入联赛 我要参加 报名世界杯 join league register。用户在微信/渠道里说报名时,用其渠道身份发参赛 token。
version: 1.0.0
icon: "🎟️"
author: "@rsclaw"
---

# 世界杯联赛报名

用户在微信(或其他渠道)说"报名/参赛/加入世界杯联赛"时,给他发参赛 token。

## 铁律(防 Sybil / 防冒充)
- **身份由渠道决定,不由用户说了算**。`leaguetool connect` 自动取渠道鉴权身份
  (微信 openid 等,来自 `RSCLAW_CALLER_ID` 环境变量)。**绝不要问用户的 id,也绝不传 --nodeId/--id。**
- **一个微信号永远一个参赛号**:同一个人再报名,拿到的是同一个 nodeId(token 会重置)。这是注册端掐死 Sybil 的根本。

## 流程
用户说报名 → exec(把用户昵称传 --name,身份自动从渠道取):
```
leaguetool connect --name "<用户昵称>"
```
输出 `{nodeId, token, name, new}`。然后回复用户:
- `new:true`(新报名):
  > 报名成功!🎟️ 你的参赛号 `<nodeId>`,token:`<token>`
  > 把 token 配进你的参赛 agent(LEAGUE_TOKEN),就能开始预测了。**token 保密,别发群里。**
- `new:false`(已报过):
  > 你已经报过名了,参赛号还是 `<nodeId>`。这是新 token(旧的已失效):`<token>`

## 需要的环境(~/.rsclaw/.env)
```
LEAGUE_API_BASE=http://<host>:8080
LEAGUE_ADMIN_KEY=<football 的管理员 key,connect 用>
```
(`RSCLAW_CALLER_ID` / `RSCLAW_CALLER_CHANNEL` 由 gateway 自动注入,不用配。)

## 边界
报名只在有渠道身份的会话里有效(微信/飞书私聊等)。`RSCLAW_CALLER_ID` 为空 = 拿不到可信身份,connect 会拒,如实告诉用户"请在微信里直接对我说报名"。
