// CLI output follows the build's brand. The `println!`/`eprintln!`/`print!`/
// `eprint!` below shadow the std macros for every `cmd` submodule (declared
// after them): when a line's format string contains default-brand words
// ("run `rsclaw doctor --fix`", `~/.rsclaw`, `rsclaw.json5`, `RsClaw`), the
// formatted line goes through `rsclaw_brand::rebrand`. Lines whose template
// carries no brand words (JSON dumps, `"{text}"` passthrough of replies, page
// text and file contents) print untouched. With the default brand both paths
// print the same bytes as the std macros.
macro_rules! println {
    () => { ::std::println!() };
    ($fmt:tt $($rest:tt)*) => {{
        let line = ::std::format!($fmt $($rest)*);
        ::std::println!("{}", ::rsclaw_brand::brand_line($fmt, &line))
    }};
}
macro_rules! eprintln {
    () => { ::std::eprintln!() };
    ($fmt:tt $($rest:tt)*) => {{
        let line = ::std::format!($fmt $($rest)*);
        ::std::eprintln!("{}", ::rsclaw_brand::brand_line($fmt, &line))
    }};
}
#[allow(unused_macros)]
macro_rules! print {
    ($fmt:tt $($rest:tt)*) => {{
        let line = ::std::format!($fmt $($rest)*);
        ::std::print!("{}", ::rsclaw_brand::brand_line($fmt, &line))
    }};
}
#[allow(unused_macros)]
macro_rules! eprint {
    ($fmt:tt $($rest:tt)*) => {{
        let line = ::std::format!($fmt $($rest)*);
        ::std::eprint!("{}", ::rsclaw_brand::brand_line($fmt, &line))
    }};
}

/// `anyhow::bail!` whose message follows the brand like printed output.
#[allow(unused_macros)]
macro_rules! brand_bail {
    ($($arg:tt)+) => {
        return ::std::result::Result::Err(::anyhow::anyhow!(
            "{}",
            ::rsclaw_brand::rebrand(&::std::format!($($arg)+))
        ))
    };
}

pub mod agent_turn;
pub mod agents;
pub mod anycli;
pub mod approvals;
pub mod backup;
pub mod browser;
pub mod channels;
pub mod completion;
pub mod config;
pub use rsclaw_config::config_json;
pub mod cron;
pub mod daemon;
pub mod dashboard;
pub mod debug;
pub mod devices;
pub mod directory;
pub mod dns;
pub mod doctor;
pub mod env;
pub mod gateway;
pub mod gateway_http;
pub mod hooks;
pub mod image;
pub mod kb;
pub mod memory;
pub mod message;
pub mod migrate;
pub mod models;
pub mod ops;
pub mod plugins;
pub mod qr;
pub mod reset;
pub mod sandbox;
pub mod secrets;
pub mod security;
pub mod sessions;
pub mod setup;
pub mod skills;
pub mod style;
pub mod system;
// tools/ extracted to the rsclaw-tools crate (crate-split). Re-export under the
// same path so `crate::cmd::tools::X` keeps resolving across the codebase.
pub use rsclaw_tools as tools;
pub mod tray;
pub mod uninstall;
pub mod update;
pub mod watch;
pub mod webhooks;

pub use agent_turn::cmd_agent_turn;
pub use agents::cmd_agents;
pub use anycli::cmd_anycli;
pub use approvals::cmd_approvals;
pub use backup::cmd_backup;
pub use browser::cmd_browser;
pub use channels::cmd_channels;
pub use completion::cmd_completion;
pub use config::cmd_config;
pub use cron::cmd_cron;
pub use daemon::cmd_daemon;
pub use dashboard::cmd_dashboard;
pub use debug::cmd_debug;
pub use devices::cmd_devices;
pub use directory::cmd_directory;
pub use dns::cmd_dns;
pub use doctor::cmd_doctor;
pub use env::cmd_env;
pub use gateway::{cmd_gateway, gateway_print_status, gateway_signal_stop};
pub use hooks::cmd_hooks;
pub use image::cmd_image;
pub use kb::cmd_kb;
pub use memory::cmd_memory;
pub use message::cmd_message;
pub use migrate::cmd_migrate;
pub use models::cmd_models;
pub use ops::{cmd_health, cmd_logs, cmd_status, cmd_tui};
pub use plugins::cmd_plugins;
pub use qr::cmd_qr;
pub use reset::cmd_reset;
pub use sandbox::cmd_sandbox;
pub use secrets::cmd_secrets;
pub use security::cmd_security;
pub use sessions::cmd_sessions;
pub use setup::{cmd_configure, cmd_onboard, cmd_setup};
pub use skills::cmd_skills;
pub use system::cmd_system;
pub use tools::cmd_tools;
pub use tray::cmd_tray;
pub use uninstall::cmd_uninstall;
pub use update::cmd_update;
pub use watch::cmd_watch;
pub use webhooks::cmd_webhooks;
