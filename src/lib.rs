//! rsclaw library crate — exposes all modules for integration tests and
//! future embedding use-cases.  The binary entry-point is in `main.rs`.
#![recursion_limit = "256"]
// Pre-existing style lints — fix incrementally, not with a blanket allow(clippy::all).
#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::manual_strip,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::single_match,
    clippy::manual_contains,
    clippy::if_same_then_else,
    clippy::redundant_closure,
    clippy::useless_format,
    clippy::unnecessary_to_owned,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::print_literal,
    clippy::manual_pattern_char_comparison,
    clippy::doc_lazy_continuation,
    clippy::regex_creation_in_loops,
    clippy::while_let_loop,
    clippy::unnecessary_map_or,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_cast,
    clippy::ptr_arg,
    clippy::nonminimal_bool,
    clippy::new_without_default,
    clippy::manual_repeat_n,
    clippy::manual_is_multiple_of,
    clippy::derivable_impls,
    clippy::while_let_on_iterator,
    clippy::unnecessary_unwrap,
    clippy::unnecessary_sort_by,
    clippy::len_zero,
    clippy::map_clone,
    clippy::match_like_matches_macro,
    clippy::needless_return,
    clippy::redundant_field_names,
    clippy::redundant_pattern_matching,
    clippy::single_char_pattern,
    clippy::clone_on_copy,
    clippy::manual_map,
    clippy::unnecessary_filter_map,
    clippy::trim_split_whitespace,
    clippy::suspicious_to_owned,
    clippy::single_element_loop,
    clippy::result_large_err,
    clippy::redundant_locals,
    clippy::question_mark,
    clippy::needless_range_loop,
    clippy::match_single_binding,
    clippy::map_flatten,
    clippy::manual_unwrap_or_default,
    clippy::manual_split_once,
    clippy::manual_range_contains,
    clippy::manual_flatten,
    clippy::manual_div_ceil,
    clippy::manual_clamp,
    clippy::implicit_saturating_sub,
    clippy::field_reassign_with_default,
)]

pub mod a2a;
pub mod acp;
pub mod agent;
pub mod browser;
pub mod channel;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod cron;
pub mod events;
pub mod gateway;
pub mod heartbeat;
pub mod hooks;
pub mod i18n;
pub mod mcp;
pub mod migrate;
pub mod plugin;
pub mod provider;
pub mod server;
pub mod skill;
pub mod store;
pub mod sys;
pub mod ws;

pub use sys::MemoryTier;

/// Ensure the rustls TLS crypto provider is installed for all lib tests.
/// This runs once before any test in the crate, preventing "No provider set"
/// panics when tests that construct `reqwest::Client` run in parallel.
#[cfg(test)]
#[ctor::ctor]
fn init_test_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
