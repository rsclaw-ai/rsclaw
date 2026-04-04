use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SkillsCommand {
    /// List installed skills
    List,
    /// Show detailed info about a skill
    Info { skill: String },
    /// Check skill health
    Check {
        #[arg(long)]
        eligible: bool,
    },
    /// Install a skill from clawhub.ai
    Install {
        /// Skill slug (e.g. "web-search", "self-improving-agent")
        name: String,
    },
    /// Uninstall a skill
    Uninstall {
        /// Skill slug
        name: String,
    },
    /// Update installed skill(s)
    Update {
        /// Skill slug (omit to update all)
        name: Option<String>,
    },
    /// Search for skills on clawhub.ai
    Search {
        /// Search query
        query: String,
    },
}
