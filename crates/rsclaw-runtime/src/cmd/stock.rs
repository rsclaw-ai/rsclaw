//! `rsclaw stock <subcmd>` CLI handler.
//!
//! Stock data is now provided by the commercial astock WASM plugin. The
//! one-shot CLI no longer links the private astock client into the open core.

use anyhow::{Result, bail};

use rsclaw_cli::StockCommand;

/// Handle `rsclaw stock ...`.
pub async fn cmd_stock(_sub: StockCommand) -> Result<()> {
    bail!(
        "`rsclaw stock` now requires the commercial astock WASM plugin path. \
         Use `/astock ...` in a running gateway session or the stock_* agent tools \
         after loading `~/dev/rsclaw-plugins/astock` with ASTOCK_API_KEY."
    )
}
