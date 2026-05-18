//! `rsclaw watch …` — terminal-delivery variant of `/watch`.
//!
//! Reuses the gateway watch pipeline (parser + source impl + filter) but
//! replaces the chat-channel sink with stdout. Runs until Ctrl-C or the
//! source emits a fatal lifecycle event.

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot};

use crate::cli::WatchArgs;
use crate::gateway::watch::filter::Filter;
use crate::gateway::watch::parser::{self, ParsedCommand, SourceKind};
use crate::gateway::watch::source::EventRecord;

pub async fn cmd_watch(args: WatchArgs) -> Result<()> {
    // The chat-side parser expects the body as a single string (e.g.
    // "sse ${ASTOCK}"). Clap gave us the tokens already split, so re-join
    // them with single spaces — quoting is not preserved across argv
    // anyway, so this matches what the user typed at the shell prompt
    // after the shell's own word-splitting.
    let body = args.body.join(" ");

    let spec = match parser::parse(&body)? {
        ParsedCommand::Start(spec) => spec,
        ParsedCommand::List | ParsedCommand::Stop(_) => {
            return Err(anyhow!(
                "`watch list` / `watch stop` are only available via the chat slash command"
            ));
        }
    };

    let kind_label = match spec.kind {
        SourceKind::File => "file",
        SourceKind::Shell => "shell",
        SourceKind::Sse => "sse",
    };
    eprintln!("watch: starting {} source: {}", kind_label, spec.raw_source);
    if let Some(g) = &spec.grep {
        eprintln!("watch: grep filter: {g}");
    }
    eprintln!("watch: press Ctrl-C to stop");

    // Resolve --template defaults the same way the chat-side
    // processor does. Keeps the CLI behavior aligned with /watch.
    let (grep_eff, jq_eff, event_eff) =
        crate::gateway::watch::resolve_template_defaults_for_cli(&spec);
    if let Some(name) = &spec.template {
        eprintln!("watch: template: {name}");
    }
    let filter = Filter::from_spec(grep_eff.as_deref(), jq_eff.as_deref(), event_eff)
        .map_err(|e| anyhow!("invalid filter: {e}"))?;
    let source_impl = crate::gateway::watch::build_source_impl(&spec)
        .map_err(|e| anyhow!("{e}"))?;

    let (src_tx, mut src_rx) = mpsc::channel::<EventRecord>(256);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let stop_tx = std::sync::Mutex::new(Some(stop_tx));

    let source_handle = tokio::spawn(async move {
        source_impl.run(src_tx, stop_rx).await;
    });

    let mut signal_received = false;
    let mut exit_code = 0i32;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                if !signal_received {
                    signal_received = true;
                    eprintln!("\nwatch: Ctrl-C received, stopping…");
                    if let Some(tx) = stop_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                } else {
                    // Second Ctrl-C — give up waiting on the source task.
                    break;
                }
            }
            maybe_ev = src_rx.recv() => match maybe_ev {
                Some(ev) => {
                    if ev.event.starts_with('_') {
                        // Lifecycle events go to stderr so users piping
                        // stdout to a file don't get them mixed in with
                        // real data lines.
                        eprintln!("[{}] {}", ev.event, ev.data);
                        // Mirror the gateway chat processor's policy: a
                        // non-fatal `_disconnect` / `_timeout` means the
                        // source will reconnect — keep listening. Only
                        // bail when the source explicitly marks the event
                        // fatal (SSE 4xx, non-SSE content-type, etc).
                        // Channel closure (recv → None) is the canonical
                        // "no more events ever" signal; we let that path
                        // do the actual loop exit.
                        let fatal = ev.data.get("fatal").and_then(|v| v.as_bool()).unwrap_or(false);
                        if fatal {
                            exit_code = 1;
                            break;
                        }
                    } else {
                        // jq with array expansion (e.g. `.codes[]`)
                        // can produce multiple lines from one event;
                        // emit each on its own stdout line.
                        for line in filter.apply(&ev) {
                            println!("{line}");
                        }
                    }
                }
                None => break, // source closed (no more reconnects coming)
            }
        }
    }

    let _ = source_handle.await;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
