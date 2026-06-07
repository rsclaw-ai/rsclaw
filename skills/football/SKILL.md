---
name: football
description: 世界杯足球数据查询 赛程 比分 实时比分 出线形势 积分榜 球队战力 球员资料 交锋史 历史战绩 伤停 阵容 资讯 集锦 赔率 赛果结算 World Cup fixtures live-scores standings team power-rating head-to-head news lineup match-result。竞猜联赛中枢/参赛 agent 取数用。
version: 1.0.0
icon: "⚽"
author: "@rsclaw"
---

# Football 世界杯数据

通过 `web_fetch` 调用 2026 世界杯数据 API 取数(赛程/比分/战力/历史/资讯/赛果)。
本技能只负责**取数**;预测、计分、互动由 agent 自身逻辑完成。

IMPORTANT:
- 用 `web_fetch` 工具真正发请求,不要输出 JSON 文本当结果。
- 所有请求带鉴权头 `Authorization: Bearer ${FOOTBALL_API_KEY}`。
- ID(matchId/teamId/playerId)整个赛事不变,可直接缓存复用。

## 配置
- Base URL:`${FOOTBALL_API_BASE}`(如 `http://api-host:8080/v1`)
- 鉴权:`Authorization: Bearer ${FOOTBALL_API_KEY}`(或 `X-API-Key`)
- 完整契约见 football 项目 `docs/worldcup-football-api.openapi.yaml`。

## 取数(查询类,参赛/中枢都可用)

**赛程 / 某日比赛**(stage/team/status/date 可选):
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/matches?date=2026-06-11", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```

**比赛详情**:
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/matches/wc2026-grp-001", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```

**出线形势 / 积分榜**(group 可选):
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/standings?group=A", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```

**球队详情(含战力 powerRating)**:
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/teams/arg", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```

**两队交锋史**:
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/head2head?teamA=arg&teamB=bra", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```

**球队世界杯历史战绩**:
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/history/worldcup?team=arg", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```

**资讯 / 伤停 / 阵容**(team/since/importance 可选):
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/news?team=fra&importance=breaking", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```

## 赛果结算(仅中枢 agent 用)

`/matches/{id}/result` 是结算唯一可信源。**只在 `final=true` 时结算**;
`postponed/cancelled/abandoned` 且 `final=false` → 作废该场预测。
```json
{"tool": "web_fetch", "url": "${FOOTBALL_API_BASE}/matches/wc2026-grp-001/result", "headers": {"Authorization": "Bearer ${FOOTBALL_API_KEY}"}}
```
取到终态赛果后,中枢用 `leaguetool score`(确定性计分)算分、`leaguetool append` 记账本。
参赛 agent **没有也不应有**写赛果的能力。

## 用法提示
- 参赛 agent 形成预测前:查 `matches`(对阵)+ `teams`(战力)+ `head2head`/`history`(历史)。
- 提交预测走 `agent_hub` 工具(非本技能),格式 `{matchId, homeScore, awayScore, confidence, reasoning}`。
- 球迷助手回答"赛程/比分/出线/战力/历史/资讯"直接用上面对应端点。
- 列表端点支持 `?updatedSince=` 增量、`?lang=zh|en`;响应带 `Cache-Control`,可信任短时缓存。
