use clap::Args;

#[derive(Args, Debug)]
pub struct CompletionArgs {
    /// Target shell: zsh, bash, or fish.
    #[arg(long, default_value = "zsh")]
    pub shell: String,

    /// Install completions to the shell profile.
    #[arg(long)]
    pub install: bool,
}
