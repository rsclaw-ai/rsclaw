use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum StockCommand {
    /// One real-time A-share quote (`/v1/quote/:code`).
    Quote(StockQuoteArgs),
    /// Batch real-time quotes (`/v1/quote/batch`).
    QuoteBatch(StockQuoteBatchArgs),
    /// K-line history for a code (`/v1/kline/:code`).
    Kline(StockKlineArgs),
    /// Full-market snapshot, optionally filtered by market or codes
    /// (`/v1/snapshot`). Pass `--ts` for historical snapshots.
    Snapshot(StockSnapshotArgs),
    /// Natural-language query routed through iwencai
    /// (`/v1/ask`). Best for "今天哪个板块涨停最多" / "北向资金
    /// 净流入前十" / "最近一周连板高度榜" style asks — iwencai
    /// handles the parsing.
    Ask(StockAskArgs),
    /// Read-only SQL against astock's DuckDB (`/v1/query`).
    /// astock validates server-side; pass a single statement.
    Query(StockQueryArgs),
}

#[derive(Args, Debug)]
pub struct StockQuoteArgs {
    /// Stock code. Tolerates `600519`, `SH600519`, `600519.SH`,
    /// `sh:600519` — normalized to 6-digit form before send.
    pub code: String,
    /// Emit raw JSON (default: pretty-printed).
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StockQuoteBatchArgs {
    /// Repeatable `--code <code>` or one comma-separated list.
    #[arg(long = "code", value_delimiter = ',')]
    pub codes: Vec<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StockKlineArgs {
    pub code: String,
    /// astock accepts `1m`, `5m`, `15m`, `30m`, `1h`, `1d`, `1w`,
    /// `1mon` (validated server-side).
    #[arg(long, short = 'p', default_value = "1d")]
    pub period: String,
    /// Number of bars (1..=800; astock clamps).
    #[arg(long, short = 'c', default_value = "60")]
    pub count: u32,
    #[arg(long, default_value = "0")]
    pub offset: u32,
    /// `none`, `qfq` (前复权), or `hfq` (后复权).
    #[arg(long, default_value = "none")]
    pub adjust: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StockSnapshotArgs {
    /// Historical timestamp, e.g. `2024-12-31` or
    /// `2024-12-31T15:00:00+08:00`. Omit for the live snapshot.
    #[arg(long)]
    pub ts: Option<String>,
    /// Filter by market: `SH`, `SZ`, or `BJ`.
    #[arg(long)]
    pub market: Option<String>,
    /// Filter by explicit codes (comma-separated or repeated).
    #[arg(long = "code", value_delimiter = ',')]
    pub codes: Vec<String>,
    /// `none`, `qfq`, `hfq` (only valid with --ts).
    #[arg(long, default_value = "none")]
    pub adjust: String,
    /// Cap rows printed in non-JSON form (default 20 to keep the
    /// terminal sane). `0` removes the cap.
    #[arg(long, default_value = "20")]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StockAskArgs {
    /// Free-form question, e.g. "今天涨停的科技股有哪些".
    pub query: String,
    #[arg(long)]
    pub page: Option<u32>,
    #[arg(long)]
    pub limit: Option<u32>,
    /// astock-internal `call_type` hint (advanced; usually omit).
    #[arg(long)]
    pub call_type: Option<String>,
    /// Default emits raw JSON because the iwencai response shape
    /// varies a lot; pretty-printing without a stable schema would
    /// be misleading.
    #[arg(long, default_value_t = true)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StockQueryArgs {
    /// SQL statement (astock validates server-side).
    pub sql: String,
    #[arg(long)]
    pub json: bool,
}
