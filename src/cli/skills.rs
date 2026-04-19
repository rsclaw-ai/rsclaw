use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SkillsCommand {
    /// List installed skills
    List,

    /// Show detailed info about an installed skill
    ///
    /// Example: rsclaw skills info web-search
    Info {
        skill: String,
    },

    /// Check installed skill health (missing commands, broken manifests)
    ///
    /// Use --eligible to show only runnable skills.
    Check {
        /// Only show skills with all commands present
        #[arg(long)]
        eligible: bool,
    },

    /// Install a skill by name or registry slug
    ///
    /// Supported formats:
    ///   rsclaw skills install web-search
    ///   rsclaw skills install vercel-labs/agent-skills@web-design-guidelines
    ///   rsclaw skills install https://github.com/owner/repo
    ///
    /// Registries searched (based on gateway.language):
    ///   CN locale  — skills.sh · skillhub.cn
    ///   Other      — skills.sh · clawhub.ai
    Install {
        /// Skill slug, owner/repo@skill, or URL
        name: String,
    },

    /// Uninstall an installed skill
    ///
    /// Example: rsclaw skills uninstall web-search
    Uninstall {
        /// Skill slug (as shown by `rsclaw skills list`)
        name: String,
    },

    /// Update installed skill(s) to the latest version
    ///
    /// Example: rsclaw skills update            # update all
    ///          rsclaw skills update web-search  # update one
    Update {
        /// Skill slug (omit to update all installed skills)
        name: Option<String>,
    },

    /// Search for skills across registries
    ///
    /// Results are merged and ranked by installs + stars.
    ///
    /// Example: rsclaw skills search browser
    ///          rsclaw skills search "image generation"
    ///
    /// Registries searched (based on gateway.language):
    ///   CN locale  — skills.sh · skillhub.cn (parallel)
    ///   Other      — skills.sh · clawhub.ai  (parallel)
    Search {
        /// Search query (keyword or phrase)
        query: String,
    },
}
