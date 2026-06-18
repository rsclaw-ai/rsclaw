use clap::Args;

#[derive(Args, Debug)]
pub struct CompletionArgs {
    /// Target shell: zsh, bash, fish, or powershell.
    #[arg(default_value = "zsh")]
    pub shell: String,

    /// Install completions to the shell profile.
    #[arg(long)]
    pub install: bool,
}
