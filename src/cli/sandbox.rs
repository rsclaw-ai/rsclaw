use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SandboxCommand {
    List,
    Recreate { id: Option<String> },
    Explain,
}
