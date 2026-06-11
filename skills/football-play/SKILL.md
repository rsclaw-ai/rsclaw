---
name: football-play
description: 世界杯竞猜下注 押注 投注 我押 我买 下注 押多少 押巴西赢 押2比1 betting place bet stake;查我的注 改注 赔率多少 odds。用户在微信里用人话下注时,解析→认场→替他提交。
version: 1.0.0
icon: "🎰"
author: "@rsclaw"
---

# 世界杯竞猜 · 人工下注

用户在微信里**用人话下注**(如"押墨西哥 2-1,押 500"),你解析、认出是哪场、**用他的渠道身份替他提交**。规则由服务端兜底,你只管把话翻成一次下注 + 回执。

IMPORTANT:
- **身份自动从渠道取**(微信 openid,来自 `RSCLAW_CALLER_ID`)。**绝不问、不传用户 id;真人不碰 token。**
- 你只下**一注**;改注/撤注做不到(服务端一场一注不可改),如实告知。

## 流程
**① 认场**:从用户话里提取队名,查当日未开球的比赛找 matchId(带 `Authorization: Bearer ${FOOTBALL_API_KEY}`):
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/matches?status=scheduled", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```
匹配 `homeTeam.name.zh`/`awayTeam.name.zh` 含用户说的队;对不上(已开球/找不到)→ 告诉用户"这场已开球或没找到,只能押未开球的"。顺带看该场 `odds`(赔率)告诉用户押中能翻多少。

**② 解析**:比分(主-客,注意主客顺序按赛程的 home/away)+ 押注额 stake。用户没说押多少 → 问"押多少分?"(或默认提示可用余额)。

**③ 提交**(exec,身份自动取):
```
leaguetool bet --match <matchId> --home <H> --away <A> --stake <S>
```
输出 `{accepted, nodeId, seq, available}`。回执:
> ✅ 已记下:<主队> <H>-<A>,押 <S> 分。可用余额还剩 **<available>**。开球后锁单,押中按赔率派彩 🎉

## 报错处理(原样接受,别重试同样的)
- `SUBMIT_REJECTED` 含 "closed" → "这场已开球,锁单了,下场再来。"
- `SUBMIT_REJECTED` 含 "duplicate" → "你这场已经押过了,一场只能押一注、不能改。"
- `SUBMIT_REJECTED` 含 "余额不足" → "余额不够,先少押点,或邀请好友拿积分(说『邀请』)。"
- `NOT_REGISTERED` → "你还没报名,先发『报名』白拿初始积分。"

## 配套口令
- **"赔率多少"**:`GET ${FOOTBALL_API_BASE}/matches?status=scheduled` 看各场 `odds`,告诉用户胜/平/负赔率。
- **"我的积分/余额/排名"**:交给 `football-register` 技能(`leaguetool mystats`)。
- **赛程/比分/出线**:交给 `football` 技能。

## 需要的环境(~/.rsclaw/.env)
```
LEAGUE_API_BASE=http://<host>:8080
LEAGUE_ADMIN_KEY=<football 管理员 key,代下注用>
FOOTBALL_API_BASE=http://<host>:8080/v1   # 认场查赛程
FOOTBALL_API_KEY=<数据 key,留空则数据公开>
```
(`RSCLAW_CALLER_ID`/`RSCLAW_CALLER_CHANNEL` 由 gateway 自动注入。)

## 边界
只在有渠道身份的私聊里能下注(`RSCLAW_CALLER_ID` 空 → 拒,告诉用户"在微信里直接对我说下注")。结算、派彩、防作弊全在服务端,你改不了。
