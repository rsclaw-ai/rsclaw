use anyhow::Result;

use super::style::*;
use crate::{cli::SessionsCommand, store, sys::detect_memory_tier};

pub async fn cmd_sessions(sub: SessionsCommand) -> Result<()> {
    let tier = detect_memory_tier();
    let data_dir = crate::config::loader::base_dir().join("var/data");
    let store = store::Store::open(&data_dir, tier)?;
    match sub {
        SessionsCommand::List(args) => {
            let sessions = store.db.list_sessions()?;
            if sessions.is_empty() {
                if args.json {
                    println!("[]");
                } else {
                    banner(&format!(
                        "rsclaw sessions v{}",
                        env!("RSCLAW_BUILD_VERSION")
                    ));
                    warn_msg("no sessions");
                }
            } else if args.json {
                let arr: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|s| serde_json::json!({"id": s}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                banner(&format!(
                    "rsclaw sessions v{}",
                    env!("RSCLAW_BUILD_VERSION")
                ));
                kv("total", &bold(&sessions.len().to_string()));
                println!();
                for s in &sessions {
                    item("-", &cyan(s));
                }
            }
        }
        SessionsCommand::Cleanup(args) => {
            let sessions = store.db.list_sessions()?;
            let to_delete: Vec<String> = sessions
                .into_iter()
                .filter(|k| args.active_key.as_deref() != Some(k.as_str()))
                .collect();

            if to_delete.is_empty() {
                ok("nothing to clean up");
                return Ok(());
            }

            if args.active_key.is_none() && !args.enforce {
                err_msg(&format!(
                    "refusing to delete all {} session(s) without --enforce or --active-key",
                    to_delete.len()
                ));
                return Ok(());
            }

            if args.dry_run {
                warn_msg(&format!("would delete {} session(s):", to_delete.len()));
                for k in &to_delete {
                    item("-", &dim(k));
                }
            } else {
                let mut deleted = 0usize;
                for k in &to_delete {
                    store.db.delete_session(k)?;
                    deleted += 1;
                }
                ok(&format!(
                    "deleted {} session(s)",
                    bold(&deleted.to_string())
                ));
            }
        }
    }
    Ok(())
}
