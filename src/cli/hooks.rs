use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum HooksCommand {
    List,
    Info { id: String },
    Check,
    Enable { id: String },
    Disable { id: String },
    Install,
    Update,
}
