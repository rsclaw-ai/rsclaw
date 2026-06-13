---
name: football-admin
description: 世界杯竞猜联赛 主办后台:报名 参赛 加入联赛、查我的积分 余额 排名 战绩 段位、邀请好友拿邀请链接、以及替真人下注 押注(我押X 2-1 押100)。仅联赛主办 bot 用(需 leaguetool + admin key)。用户在微信/飞书说这些时按其渠道身份处理。
version: 1.0.0
icon: "🎟️"
author: "@rsclaw"
---

# Football Admin · 联赛主办后台(报名 + 代下注 + 查积分)

**只有联赛主办 bot 用**(装了 `leaguetool`、配了 admin key)。用 `leaguetool`(exec)替真人操作,**身份从渠道自动注入**(`RSCLAW_CALLER_ID`,微信 openid 等)。

## 铁律(防 Sybil / 防冒充)
- **身份由渠道决定,不由用户说了算**。leaguetool 自动取渠道身份。**绝不问用户 id,绝不传 --nodeId/--id。**
- **一个微信号永远一个参赛号**:同一人再报名拿同一 nodeId(token 重置)。
- 只在有渠道身份的私聊/群里有效;`RSCLAW_CALLER_ID` 空 = 拿不到可信身份,如实告诉用户"在微信里直接对我说"。

## 报名(用户说 报名/参赛/加入联赛)
```
leaguetool connect --name "<用户昵称>"
```
(从 `/live?ref=<暗号>` 来的带 `--ref "<暗号>"`,邀请人和他各得奖励。)输出 `{nodeId, token, new, invited}`。回复:
- new:true:报名成功!🎟️ 白拿初始积分,可以押注了{invited→",邀请奖励也到账 🎉"}。参赛号 `<nodeId>`。发「我的积分」看余额、「邀请」拉好友。
- new:false:你已报过名(参赛号 `<nodeId>`)。发「我的积分」看余额。

## 查积分(用户说 我的积分/余额/排名/第几/战绩/段位)
```
leaguetool mystats
```
输出 `{name, stats:{rank,...,grade,awards}, account:{balance,available,bets,wins}}`,回(余额是主指标):
> {name},你现在 **{balance} 积分**(可用 {available}),押注 {bets} 场赢 {wins} 场。{onBoard→"榜第 {rank}/{total}"}{awards→",现能拿:"+awards}
- `NOT_REGISTERED` → "你还没报名,发『报名』先加入,白拿初始积分。"

## 代真人下注(用户人话:我押墨西哥 2-1 押500)
① 认场:`web_fetch {LEAGUE_API_BASE}/v1/matches?status=scheduled`,按队名(`homeTeam.name.zh`/`awayTeam.name.zh`)匹配拿 matchId;对不上(已开球/找不到)如实说。
② 解析比分(主-客,按赛程 home/away 顺序)+ 押注额 stake;没说押多少就问"押多少分?"。
③ 提交:
```
leaguetool bet --match <matchId> --home <H> --away <A> --stake <S>
```
输出 `{accepted, available}`。回执:
> ✅ 已记下:<主队> H-A,押 S 分。可用余额还剩 **<available>**。开球后锁单,押中按赔率派彩 🎉

报错原样接受、别重试:`closed`→"这场已开球锁单了,下场再来"；`duplicate`→"这场你押过了,一场一注不可改"；`余额不足`→"余额不够,少押点或发『邀请』拉好友拿分"；`NOT_REGISTERED`→"先发『报名』"。

## 邀请(用户说 邀请/邀请链接/拉好友)
`leaguetool mystats` 拿自己 nodeId,回:
> 🎟️ 发给好友,他报名后**你俩各得邀请积分**:`https://api.duoduoyun.work/worldcup/live?ref=<你的nodeId>`
> 好友点开按提示对我说「报名 <你的nodeId>」。拉越多本金越多,不封顶。

## 用自己的 agent 参赛(用户说 我要用自己的 agent 下注/给我参赛 token)
跑 connect,把**原始 token 私聊**发他(机密,绝不进群):
```
leaguetool connect --name "<用户昵称>"
```
输出 `{nodeId, token}`,私聊回:
> 🎟️ 你的参赛 token(**机密别外泄**):`<token>`
> 在你 agent 的 rsclaw.json5 加:`"env":{"LEAGUE_API_BASE":"http://api.duoduoyun.work:8080","LEAGUE_TOKEN":"<token>"}`
> 装技能:`curl -sL https://api.duoduoyun.work/worldcup/football-agent/SKILL.md -o ~/.rsclaw/skills/football-agent/SKILL.md`
> 重启后你的 agent 就能自主查赛程/赔率、用自己 token 下注(见 football-agent 技能)。一人一号,再领会重置旧 token。

## 需要的环境(~/.rsclaw/.env 或配置 env 块)
```
LEAGUE_API_BASE=http://api.duoduoyun.work:8080
LEAGUE_ADMIN_KEY=<football 管理员 key,connect/bet 用>
FOOTBALL_API_BASE=http://api.duoduoyun.work:8080/v1   # 认场查赛程
```
(`RSCLAW_CALLER_ID`/`RSCLAW_CALLER_CHANNEL` 由 gateway 自动注入。)

> 结算/派彩由 football 服务自动跑,主办也不碰结算。坐庄设赔率/播报是 `football-hub` 技能的事。
