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
    #[command(subcommand, name = "image-fallbacks")]
    ImageFallbacks(ImageFallbacksCommand),
    /// Scan local Ollama/LMStudio models.
    Scan,
    #[command(subcommand)]
    Auth(ModelsAuthCommand),
    /// Download embedding models from HuggingFace.
    Download {
        /// Model to download (default: bge). Available: bge
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
pub enum ImageFallbacksCommand {
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
