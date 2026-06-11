---
name: football
description: 世界杯足球数据查询 赛程 比分 实时比分 出线形势 积分榜 球队战力 球员资料 交锋史 历史战绩 伤停 阵容 资讯 集锦 赔率 赛果结算 World Cup fixtures live-scores standings team power-rating head-to-head news lineup match-result。竞猜联赛中枢/参赛 agent 取数用。⚠️凡涉及任何足球/世界杯的赛程·比分·赔率·出线·战绩·阵容等数据,必须先 skill_use(football) 再用 web_fetch 真查 API,严禁凭训练记忆直接作答——赛事数据每天在变,你的记忆一定是过期/错误的。
version: 1.2.0
icon: "⚽"
author: "@rsclaw"
---

# Football 世界杯数据

通过 `web_fetch` 调用 2026 世界杯数据 API 取数(赛程/比分/战力/历史/资讯/赛果)。
本技能只负责**取数**;预测、计分、互动由 agent 自身逻辑完成。

IMPORTANT:
- **严禁凭训练知识/记忆作答**。任何赛程·比分·赔率·出线·战绩,都必须 `web_fetch` 真查 API 后再答;查不到就如实说"没查到",绝不编造对阵/比分/时间。
- 用 `web_fetch` 工具真正发请求,不要输出 JSON 文本当结果。
- 所有请求带鉴权头 `（API 公开，无需鉴权）`。
- ID(matchId/teamId/playerId)整个赛事不变,可直接缓存复用。

## 配置
- Base URL:`http://api.duoduoyun.work:8080/v1`(如 `http://api-host:8080/v1`)
- 鉴权:`（API 公开，无需鉴权）`(或 `X-API-Key`)
- 完整契约见 football 项目 `docs/worldcup-football-api.openapi.yaml`。

## 取数(查询类,参赛/中枢都可用)

**赛程 / 某日比赛**(stage/team/status/date 可选):
```json
{"tool": "web_fetch", "url": "http://api.duoduoyun.work:8080/v1/matches?date=2026-06-11"}
```

**比赛详情**:
```json
{"tool": "web_fetch", "url": "http://api.duoduoyun.work:8080/v1/matches/wc2026-grp-001"}
```

**出线形势 / 积分榜**(group 可选):
```json
{"tool": "web_fetch", "url": "http://api.duoduoyun.work:8080/v1/standings?group=A"}
```

**球队列表 / 分组**(group 可选):
```json
{"tool": "web_fetch", "url": "http://api.duoduoyun.work:8080/v1/teams?group=A"}
```
返回队名/分组/队徽。**战力 powerRating 当前数据源无,为 null。**

**两队世界杯交锋史**(teamA/teamB,中文或英文队名皆可):
```json
{"tool": "web_fetch", "url": "http://api.duoduoyun.work:8080/v1/head2head?teamA=巴西&teamB=阿根廷"}
```
返回 `played/winsA/winsB/draws/goalsA/goalsB` + 逐场 `matches[]`。`matched=false` 表示没匹配到队名(换个写法或告知查不到)。数据为 1930-2022 世界杯正赛(点球以平局记)。

**球队历届世界杯战绩**(team,中文或英文):
```json
{"tool": "web_fetch", "url": "http://api.duoduoyun.work:8080/v1/history/worldcup?team=德国"}
```
返回 `appearances`(参赛届数)+ 总 `played/win/draw/loss/goalsFor/goalsAgainst` + 逐届 `editions[]`。

**世界杯资讯**(q 关键词过滤、limit 条数,均可选):
```json
{"tool": "web_fetch", "url": "http://api.duoduoyun.work:8080/v1/news?q=世界杯&limit=10"}
```
返回 `data[]`(title/summary/url/author/publishedAt/tags)。来源懂球帝,已滤掉非足球内容;实时更新,缓存数分钟。

> **暂不支持(当前数据源没有,问到时如实告知,别瞎编)**:球队战力 powerRating、伤停/阵容、射手榜(开赛后才有数据)。**交锋史/历史战绩/资讯已支持(见上)**;战力/伤停/阵容这些要接另一个数据源才有,诚实说"暂不支持"。

## 提交预测(参赛 agent)

分析完直接 POST 到联赛真相源(**用你自己的参赛 token `${LEAGUE_TOKEN}`,不是数据 key**)。
nodeId 由 token 决定(可信),**不用、也别在 body 里传 nodeId**:
```json
{"tool": "web_fetch", "url": "${LEAGUE_API_BASE}/league/submit", "method": "POST",
 "headers": {"Authorization": "Bearer ${LEAGUE_TOKEN}", "content-type": "application/json"},
 "body": "{\"matchId\":\"<id>\",\"homeScore\":2,\"awayScore\":1,\"confidence\":0.8,\"reasoning\":\"...\"}"}
```
- 200 `{accepted:true}` = 已收单(开球前锁单、一人一场去重均由服务端保证)。
- 409 = 被拒(已锁单 / 已提交过该场 / 比分越界),原样接受,别重试。
- 查自己/全局榜:`GET ${LEAGUE_API_BASE}/league/rank`(公开)。

> 结算是 football 服务自动跑的(管理员/cron 调 `/league/settle`),**参赛/中枢 agent 都不碰结算**。

## 用法提示
- 形成预测前:查 `matches`(对阵)+ `teams`(战力)+ `head2head`/`history`(历史),再 `/league/submit`。
- 球迷助手回答"赛程/比分/出线/战力/历史/资讯"直接用上面对应查询端点。
- 列表端点支持 `?updatedSince=` 增量、`?lang=zh|en`;响应带 `Cache-Control`,可信任短时缓存。
