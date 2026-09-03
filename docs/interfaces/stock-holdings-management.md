# Stock holdings management interface

## Goal

Allow the RsClaw agent to manage the existing holdings/watchlist file through natural-language requests. Scheduled quote delivery is out of scope and remains unchanged.

## Tool

`stock_holdings`

Actions:

- `list`: return all entries.
- `add`: add one entry; requires `code` and `name`; `cost` defaults to `0`, `shares` defaults to `0` (watchlist-only).
- `update`: update an existing entry selected by `code`; accepts `name`, `cost`, and/or `shares`.
- `remove`: remove an existing entry selected by `code`.

Stock codes accept six digits or a `.SH`/`.SZ` suffix and are persisted as six digits to remain compatible with the existing file.

## Storage

The existing JSON array shape is preserved:

```json
[{"code":"600308","name":"华泰股份","cost":3.325,"shares":1000}]
```

Path resolution order:

1. `RSCLAW_HOLDINGS_PATH` environment variable.
2. `astock.holdingsPath` or legacy `astock.holdings_path` in `rsclaw.json5`.
3. On Windows, existing `K:\openclaw\workspace-multi-agent\holdings_config.json`.
4. `~/.rsclaw/holdings_config.json` fallback.

Mutations take an exclusive sidecar-file lock, re-read after acquiring the lock, and replace the JSON file atomically from a temporary file in the same directory.

## Error behavior

Malformed JSON is never overwritten. Duplicate adds, missing update/remove targets, invalid codes, empty names, negative costs, and invalid shares return errors without changing the file.
