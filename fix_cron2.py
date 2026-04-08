import json

ORIGINAL_JOBS = [
    {"id": "f1395011-b276-43fa-9e38-5dc55ef8041e", "name": "Trading A 股早盘分析", "schedule": {"kind": "cron", "expr": "0 9 * * 1-5", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "分析 A 股早盘情况，输出策略建议（只读，写入 shared-context）"}},
    {"id": "bd1a052b-e701-4543-9ca8-02b7bd0e65dd", "name": "Trading A 股收盘分析", "schedule": {"kind": "cron", "expr": "30 16 * * 1-5", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "分析 A 股全天行情，输出收盘速报和策略（只读，写入 shared-context）"}},
    {"id": "a4668f67-23dd-43bf-afe6-d0baf660c60c", "name": "Trading 龙虎榜分析", "schedule": {"kind": "cron", "expr": "0 20 * * 1-5", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "调用 longhu-analyzer 技能，分析龙虎榜数据，判断游资和机构动向（写入 shared-context/intel/longhu-analysis.md）"}},
    {"id": "004936af-69ad-4893-bd05-ec6592989734", "name": "Trading 美股盘前分析", "schedule": {"kind": "cron", "expr": "0 22 * * *", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "分析美股盘前情况，评估对 A 股影响（只读，写入 shared-context）"}},
    {"id": "f304b725-d12f-4663-a48f-8dd8ebac5a23", "name": "Trading Token 日报", "schedule": {"kind": "cron", "expr": "30 22 * * *", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "生成今日 Token 消耗日报"}},
    {"id": "877ec314-1eeb-4c49-bcb4-c821231c5931", "name": "Trading 选股策略", "schedule": {"kind": "cron", "expr": "30 14 * * 1-5", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "执行选股策略：使用新浪财经接口获取数据，筛选条件（涨幅 3-5%、换手 5-10%、市值 50-200 亿、非 ST/非科创），按成交额排序选前 10 只，保存到 shared-context/intel/stock-pool-daily.json"}},
    {"id": "71162221-c175-4d0d-87f4-b374c086c019", "name": "Trading 选股策略跟踪", "schedule": {"kind": "cron", "expr": "30 15 * * 1-5", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "统计昨日选股表现（涨跌分布、胜率、平均收益），分析最涨最差表现，输出策略优化建议"}},
    {"id": "e30f1414-c665-4856-9d12-e6e9f0b4a4e0", "name": "持仓盯盘 - 开盘监控", "schedule": {"kind": "cron", "expr": "30 9 * * 1-5", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "执行持仓开盘监控（吉鑫科技 603938、天富能源 600509、易事特 300376、联发股份 002394），检查竞价情况、开盘涨跌幅、量比、换手率，输出预警和走势预判。止损统一设为 -3%，接近止损（-2.5%）时提前预警"}},
    {"id": "5a3085c6-5f08-453c-bdac-ff239ef708ef", "name": "持仓盯盘 - 收盘总结", "schedule": {"kind": "cron", "expr": "5 15 * * 1-5", "tz": "Asia/Shanghai"}, "agentId": "main", "enabled": True, "payload": {"kind": "agentTurn", "message": "执行持仓收盘总结（吉鑫科技 603938、天富能源 600509、易事特 300376、联发股份 002394），统计全天涨跌幅、持仓盈亏、主力资金流向，输出收盘报告和明日策略建议。止损统一设为 -3%，检查是否有股票触及止损"}},
    {"id": "b3b9dc9e-cdd6-4301-9627-e2a611e7a706", "name": "交易盯盘", "schedule": {"kind": "cron", "expr": "*/5 9-11,13-15 * * 1-5"}, "agentId": "main", "enabled": True, "payload": {"kind": "systemEvent", "text": "cd K:\\openclaw\\workspace-multi-agent && python position_monitor.py"}},
    {"id": "ea0d0ae1-1b04-44a4-8a68-023bf8680e5b", "name": "duckdb当时交易数据更新", "schedule": {"kind": "cron", "expr": "30 20 * * *"}, "agentId": "main", "enabled": True, "payload": {"kind": "systemEvent", "text": "cd K:\\openclaw\\workspace && python fetch_daily_data.py --date today"}},
]

import re

with open('C:/Users/Administrator/.rsclaw/rsclaw.json5', 'r', encoding='utf-8') as f:
    c = f.read()

c = re.sub(r',(\s*[}\]])', r'\1', c)
cfg = json.loads(c)

for job_data in ORIGINAL_JOBS:
    job = {
        "id": job_data["id"],
        "name": job_data["name"],
        "schedule": job_data["schedule"]["expr"],
        "agentId": job_data["agentId"],
        "enabled": job_data["enabled"],
    }
    if job_data["schedule"].get("tz"):
        job["tz"] = job_data["schedule"]["tz"]
    
    payload = job_data["payload"]
    if payload["kind"] == "agentTurn":
        job["payload"] = {"kind": "agentTurn", "message": payload["message"]}
    else:
        job["payload"] = {"kind": "systemEvent", "text": payload["text"]}
    
    cfg["cron"]["jobs"].append(job)

with open('C:/Users/Administrator/.rsclaw/rsclaw.json5', 'w', encoding='utf-8') as f:
    json.dump(cfg, f, ensure_ascii=False, indent=2)

print("Fixed!")
