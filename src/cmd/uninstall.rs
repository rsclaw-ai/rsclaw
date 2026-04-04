use anyhow::Result;

use crate::{cli::UninstallArgs, config};

pub async fn cmd_uninstall(args: UninstallArgs) -> Result<()> {
    let base_dir = config::loader::base_dir();
    let do_all = args.all;
    let dry = args.dry_run;

    let remove_service = do_all || args.service;
    let remove_state = do_all || args.state;
    let remove_workspace = do_all || args.workspace;
    let remove_app = do_all || args.app;

    if !remove_service && !remove_state && !remove_workspace && !remove_app {
        println!("nothing selected -- use --service, --state, --workspace, --app, or --all");
        println!();
        println!("  --service    remove launchd/systemd service");
        println!("  --state      remove ~/.rsclaw/ state directory");
        println!("  --workspace  remove agent workspace directories");
        println!("  --app        remove the rsclaw binary");
        println!("  --all        all of the above");
        println!("  --dry-run    show what would be removed");
        return Ok(());
    }

    if !dry && !args.yes && !args.non_interactive {
        println!("this will permanently remove the selected components.");
        println!("re-run with --yes or --non-interactive to confirm, or --dry-run to preview.");
        return Ok(());
    }

    if remove_service {
        if dry {
            println!("[dry-run] would uninstall gateway service");
        } else {
            println!("uninstalling gateway service...");
            // Delegate to gateway uninstall logic.
            let gw_result =
                crate::cmd::gateway::cmd_gateway(crate::cli::GatewayCommand::Uninstall).await;
            match gw_result {
                Ok(()) => println!("  service removed"),
                Err(e) => println!("  service removal: {e}"),
            }
        }
    }

    if remove_state {
        if dry {
            println!("[dry-run] would remove {}", base_dir.display());
        } else if base_dir.exists() {
            std::fs::remove_dir_all(&base_dir)?;
            println!("removed state dir: {}", base_dir.display());
        } else {
            println!("state dir not found: {}", base_dir.display());
        }
    }

    if remove_workspace {
        // Try to find workspace from config, fallback to base_dir/workspace.
        let ws = base_dir.join("workspace");
        if dry {
            println!("[dry-run] would remove {}", ws.display());
        } else if ws.exists() {
            std::fs::remove_dir_all(&ws)?;
            println!("removed workspace: {}", ws.display());
        } else {
            println!("workspace not found: {}", ws.display());
        }
    }

    if remove_app {
        let exe = std::env::current_exe().unwrap_or_default();
        if dry {
            println!("[dry-run] would remove {}", exe.display());
        } else {
            println!("to remove the binary, run:");
            println!("  rm {}", exe.display());
            println!("(not auto-deleting the running binary)");
        }
    }

    if dry {
        println!();
        println!("(dry run -- no changes made)");
    }

    Ok(())
}
