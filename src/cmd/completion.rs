use anyhow::Result;

use crate::cli::CompletionArgs;

pub async fn cmd_completion(args: CompletionArgs) -> Result<()> {
    let shell = args.shell.to_lowercase();

    let script = match shell.as_str() {
        "bash" => generate_bash(),
        "zsh" => generate_zsh(),
        "fish" => generate_fish(),
        other => anyhow::bail!("unsupported shell: {other} (use bash, zsh, or fish)"),
    };

    if args.install {
        install_completion(&shell, &script)?;
    } else {
        print!("{script}");
    }

    Ok(())
}

fn generate_bash() -> String {
    r#"# rsclaw bash completions
# Add to ~/.bashrc:  eval "$(rsclaw completion --shell bash)"
_rsclaw() {
    local cur prev commands
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    commands="setup onboard configure config doctor gateway channels agents models skills plugins memory sessions cron hooks system secrets security sandbox logs status health tui backup reset update pairing completion dashboard daemon docs qr uninstall webhooks"

    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=($(compgen -W "${commands}" -- "${cur}"))
    fi
}
complete -F _rsclaw rsclaw
"#
    .to_string()
}

fn generate_zsh() -> String {
    r#"# rsclaw zsh completions
# Add to ~/.zshrc:  eval "$(rsclaw completion --shell zsh)"
#compdef rsclaw

_rsclaw() {
    local -a commands
    commands=(
        'setup:Initialise config and workspace'
        'onboard:Interactive onboarding wizard'
        'configure:Interactive configuration wizard'
        'config:Config management'
        'doctor:Diagnose configuration issues'
        'gateway:Gateway lifecycle'
        'channels:Channel management'
        'agents:Agent management'
        'models:Model management'
        'skills:Skill management'
        'plugins:Plugin management'
        'memory:Memory management'
        'sessions:Session management'
        'cron:Cron job management'
        'hooks:Webhook hook management'
        'system:System utilities'
        'secrets:Secrets management'
        'security:Security audit'
        'sandbox:Sandbox management'
        'logs:Tail gateway logs'
        'status:Show overall status'
        'health:Health check'
        'tui:Terminal UI'
        'backup:Backup management'
        'reset:Reset state'
        'update:Update rsclaw binary'
        'pairing:DM pairing management'
        'completion:Generate shell completions'
        'dashboard:Open Control UI'
        'daemon:Legacy gateway alias'
        'docs:Search live documentation'
        'qr:Generate iOS pairing QR'
        'uninstall:Uninstall service and data'
        'webhooks:Webhook helpers'
    )
    _describe 'command' commands
}

_rsclaw "$@"
"#
    .to_string()
}

fn generate_fish() -> String {
    r#"# rsclaw fish completions
# Save to ~/.config/fish/completions/rsclaw.fish
complete -c rsclaw -n __fish_use_subcommand -a setup -d 'Initialise config and workspace'
complete -c rsclaw -n __fish_use_subcommand -a onboard -d 'Interactive onboarding wizard'
complete -c rsclaw -n __fish_use_subcommand -a configure -d 'Interactive configuration wizard'
complete -c rsclaw -n __fish_use_subcommand -a config -d 'Config management'
complete -c rsclaw -n __fish_use_subcommand -a doctor -d 'Diagnose configuration issues'
complete -c rsclaw -n __fish_use_subcommand -a gateway -d 'Gateway lifecycle'
complete -c rsclaw -n __fish_use_subcommand -a channels -d 'Channel management'
complete -c rsclaw -n __fish_use_subcommand -a agents -d 'Agent management'
complete -c rsclaw -n __fish_use_subcommand -a models -d 'Model management'
complete -c rsclaw -n __fish_use_subcommand -a skills -d 'Skill management'
complete -c rsclaw -n __fish_use_subcommand -a plugins -d 'Plugin management'
complete -c rsclaw -n __fish_use_subcommand -a memory -d 'Memory management'
complete -c rsclaw -n __fish_use_subcommand -a sessions -d 'Session management'
complete -c rsclaw -n __fish_use_subcommand -a cron -d 'Cron job management'
complete -c rsclaw -n __fish_use_subcommand -a hooks -d 'Webhook hook management'
complete -c rsclaw -n __fish_use_subcommand -a system -d 'System utilities'
complete -c rsclaw -n __fish_use_subcommand -a secrets -d 'Secrets management'
complete -c rsclaw -n __fish_use_subcommand -a security -d 'Security audit'
complete -c rsclaw -n __fish_use_subcommand -a sandbox -d 'Sandbox management'
complete -c rsclaw -n __fish_use_subcommand -a logs -d 'Tail gateway logs'
complete -c rsclaw -n __fish_use_subcommand -a status -d 'Show overall status'
complete -c rsclaw -n __fish_use_subcommand -a health -d 'Health check'
complete -c rsclaw -n __fish_use_subcommand -a tui -d 'Terminal UI'
complete -c rsclaw -n __fish_use_subcommand -a backup -d 'Backup management'
complete -c rsclaw -n __fish_use_subcommand -a reset -d 'Reset state'
complete -c rsclaw -n __fish_use_subcommand -a update -d 'Update rsclaw binary'
complete -c rsclaw -n __fish_use_subcommand -a pairing -d 'DM pairing management'
complete -c rsclaw -n __fish_use_subcommand -a completion -d 'Generate shell completions'
complete -c rsclaw -n __fish_use_subcommand -a dashboard -d 'Open Control UI'
complete -c rsclaw -n __fish_use_subcommand -a daemon -d 'Legacy gateway alias'
complete -c rsclaw -n __fish_use_subcommand -a docs -d 'Search live documentation'
complete -c rsclaw -n __fish_use_subcommand -a qr -d 'Generate iOS pairing QR'
complete -c rsclaw -n __fish_use_subcommand -a uninstall -d 'Uninstall service and data'
complete -c rsclaw -n __fish_use_subcommand -a webhooks -d 'Webhook helpers'
"#
    .to_string()
}

fn install_completion(shell: &str, script: &str) -> Result<()> {
    let home = dirs_next::home_dir().unwrap_or_default();
    let (path, msg) = match shell {
        "bash" => {
            let p = home.join(".bashrc");
            (p, "appended to ~/.bashrc")
        }
        "zsh" => {
            let p = home.join(".zshrc");
            (p, "appended to ~/.zshrc")
        }
        "fish" => {
            let dir = home.join(".config/fish/completions");
            std::fs::create_dir_all(&dir)?;
            let p = dir.join("rsclaw.fish");
            std::fs::write(&p, script)?;
            println!("installed completions to {}", p.display());
            return Ok(());
        }
        _ => anyhow::bail!("unsupported shell for install: {shell}"),
    };

    // For bash/zsh, append an eval line.
    let eval_line = format!("\neval \"$(rsclaw completion --shell {shell})\"\n");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains("rsclaw completion") {
        println!("completions already installed in {}", path.display());
    } else {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path)?;
        f.write_all(eval_line.as_bytes())?;
        println!("{msg}: {}", path.display());
    }
    Ok(())
}
