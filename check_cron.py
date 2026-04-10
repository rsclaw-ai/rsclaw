import json, re

with open('C:/Users/Administrator/.rsclaw/rsclaw.json5', 'r', encoding='utf-8') as f:
    c = re.sub(r',(\s*[}\]])', r'\1', f.read())
cfg = json.loads(c)

jobs = cfg.get('cron', {}).get('jobs', [])
for j in jobs:
    sched = j.get('schedule', '')
    if isinstance(sched, dict):
        sched = sched.get('expr', '')
    print(f'{j["id"][:8]} | {j.get("name","")[:20]:20} | {sched}')
