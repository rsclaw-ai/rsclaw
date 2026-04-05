use anyhow::Result;

use super::style::*;
use crate::{agent, cli::MemoryCommand, config, sys::detect_memory_tier};

pub async fn cmd_memory(sub: MemoryCommand) -> Result<()> {
    let tier = detect_memory_tier();
    let base = config::loader::base_dir();
    let data_dir = base.join("var/data");
    let model_dir = base.join("models/bge-small-en");
    let cfg = config::load().ok();
    let search_cfg = cfg.as_ref().and_then(|c| c.raw.memory_search.as_ref());
    match sub {
        MemoryCommand::Status(args) => {
            let mem =
                agent::memory::MemoryStore::open(&data_dir, Some(&model_dir), tier, search_cfg)
                    .await?;
            let count = mem.count().await?;
            if args.json {
                println!("{}", serde_json::json!({"documents": count}));
            } else {
                banner(&format!("rsclaw memory v{}", env!("RSCLAW_BUILD_VERSION")));
                kv("documents", &bold(&count.to_string()));
            }
        }
        MemoryCommand::Search(args) => {
            banner(&format!("rsclaw memory search v{}", env!("RSCLAW_BUILD_VERSION")));
            let mut mem =
                agent::memory::MemoryStore::open(&data_dir, Some(&model_dir), tier, search_cfg)
                    .await?;
            let results = mem.search(&args.query, None, args.max_results).await?;
            if results.is_empty() {
                warn_msg("no results");
            } else {
                kv("query", &cyan(&args.query));
                kv("results", &bold(&results.len().to_string()));
                println!();
                for doc in &results {
                    println!(
                        "  {} {} {}",
                        dim(&format!("[{}]", doc.id)),
                        dim(&format!("({})", doc.kind)),
                        doc.text
                    );
                }
            }
        }
        MemoryCommand::Index(_args) => {
            let mut mem =
                agent::memory::MemoryStore::open(&data_dir, Some(&model_dir), tier, search_cfg)
                    .await?;
            let count = mem.reindex().await?;
            ok(&format!("re-indexed {} document(s)", bold(&count.to_string())));
        }
    }
    Ok(())
}
