---
name: football-register
description: 世界杯竞猜联赛报名 参赛 加入联赛 我要参加 报名世界杯 join league register;邀请好友 拿邀请链接 invite;以及查我的积分 余额 我第几名 我的排名 我的战绩 我的段位 my rank balance my score。用户在微信/渠道里说这些时,按其渠道身份处理。
version: 1.2.0
icon: "🎟️"
author: "@rsclaw"
---

# 世界杯联赛报名 + 查个人战绩

按用户的**渠道身份**(微信 openid 等,自动注入,非用户自报)做两件自助:报名、查自己积分。

## 查我的积分 / 我第几名(用户问"我的积分/我的排名/我的战绩/我第几"时)
exec(身份自动从渠道取,不用问、不用传):
```
leaguetool mystats
```
输出 `{name, stats:{rank,...,grade,awards,onBoard}, account:{balance,available,bets,wins}}`,回用户(**积分余额是主指标**):
- 已报名:
  > 数据流,你现在 **{balance} 积分**(可用 {available}),押注 {bets} 场赢 {wins} 场。{onBoard → "积分榜第 {rank}/{total}"}{awards 非空 → ",现在能拿:" + awards}
- 报错 `NOT_REGISTERED` → "你还没报名,发『报名』先加入,白拿初始积分。"

# 世界杯联赛报名

用户在微信(或其他渠道)说"报名/参赛/加入世界杯联赛"时,给他发参赛 token。

## 铁律(防 Sybil / 防冒充)
- **身份由渠道决定,不由用户说了算**。`leaguetool connect` 自动取渠道鉴权身份
  (微信 openid 等,来自 `RSCLAW_CALLER_ID` 环境变量)。**绝不要问用户的 id,也绝不传 --nodeId/--id。**
- **一个微信号永远一个参赛号**:同一个人再报名,拿到的是同一个 nodeId(token 会重置)。这是注册端掐死 Sybil 的根本。

## 流程
用户说报名 → exec(把用户昵称传 --name,身份自动从渠道取)。
**若用户说的是「报名 <暗号>」**(从 `/live?ref=<暗号>` 链接来的),把暗号传 `--ref`,邀请人和他各得邀请奖励:
```
leaguetool connect --name "<用户昵称>" --ref "<暗号>"
```
(没暗号就不带 `--ref`。)输出 `{nodeId, token, name, new, invited}`。回复用户:
- `new:true`(新报名):
  > 报名成功!🎟️ 你已**白拿初始积分**,可以开始押注了{invited → ",邀请奖励也到账 🎉"}。
  > 你的参赛号 `<nodeId>`。发「我的积分」看余额,发「邀请」拿你的专属邀请链接拉好友领积分。
- `new:false`(已报过):
  > 你已经报过名了(参赛号 `<nodeId>`)。发「我的积分」看余额。

## 邀请好友(用户说"邀请/邀请链接/拉好友")
先 `leaguetool mystats` 拿到自己的 `nodeId`(或复用报名时的),回用户专属链接:
> 🎟️ 把这条发给好友,他报名后**你俩各得邀请积分**:
> `${LEAGUE_API_BASE}/live?ref=<你的nodeId>`
> 好友点开按提示对我说「报名 <你的nodeId>」就行。**拉越多本金越多,不封顶。**

## 需要的环境(~/.rsclaw/.env)
```
LEAGUE_API_BASE=http://<host>:8080
LEAGUE_ADMIN_KEY=<football 的管理员 key,connect 用>
```
(`RSCLAW_CALLER_ID` / `RSCLAW_CALLER_CHANNEL` 由 gateway 自动注入,不用配。)

## 边界
报名只在有渠道身份的会话里有效(微信/飞书私聊等)。`RSCLAW_CALLER_ID` 为空 = 拿不到可信身份,connect 会拒,如实告诉用户"请在微信里直接对我说报名"。
