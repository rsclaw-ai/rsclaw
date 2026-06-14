use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ModelsCommand {
    List,
    Status,
    Set {
        model: String,
    },
    SetImage {
        model: String,
    },
    #[command(subcommand)]
    Aliases(AliasesCommand),
    #[command(subcommand)]
    Fallbacks(FallbacksCommand),
    /// Scan local Ollama/LMStudio models.
    Scan,
    #[command(subcommand)]
    Auth(ModelsAuthCommand),
    /// Inspect or reset per-model chain failover health
    /// (Healthy / Cooling / Disabled).
    #[command(subcommand)]
    Health(HealthCommand),
    /// Download ML models from gitfast.org.
    Download {
        /// Model to download (default: bge). Available: bge, bge-base-zh,
        /// bge-small-en, whisper, whisper-turbo, vits
        model: Option<String>,
    },
    /// List installed embedding models.
    Installed,
}

#[derive(Subcommand, Debug)]
pub enum AliasesCommand {
    List,
    Add { alias: String, model: String },
    Remove { alias: String },
}

#[derive(Subcommand, Debug)]
pub enum FallbacksCommand {
    List,
    Add { model: String },
    Remove { model: String },
    Clear,
}

#[derive(Subcommand, Debug)]
pub enum ModelsAuthCommand {
    Add,
    SetupToken,
    PasteToken,
    #[command(subcommand)]
    Order(AuthOrderCommand),
}

#[derive(Subcommand, Debug)]
pub enum HealthCommand {
    /// List every model the gateway's failover loops have observed,
    /// with its current status (Healthy / Cooling / Disabled) + last
    /// error snippet. Equivalent to `GET /api/v1/models/health`.
    List,
    /// Clear a Disabled/Cooling state for a single model so the next
    /// chain iteration retries it. Use after recharging the provider
    /// balance, rotating an API key, or correcting a misspelt model id.
    Reset { model: String },
}

#[derive(Subcommand, Debug)]
pub enum AuthOrderCommand {
    Get {
        provider: String,
    },
    Set {
        provider: String,
        order: Vec<String>,
    },
    Clear {
        provider: String,
    },
}
