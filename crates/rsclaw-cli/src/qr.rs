use clap::Args;

#[derive(Args, Debug)]
pub struct QrArgs {
    /// Output as JSON instead of QR image.
    #[arg(long)]
    pub json: bool,

    /// Suppress ASCII QR rendering.
    #[arg(long)]
    pub no_ascii: bool,

    /// Print only the setup code (no QR).
    #[arg(long)]
    pub setup_code_only: bool,

    /// Override gateway URL in QR payload.
    #[arg(long)]
    pub url: Option<String>,

    /// Override auth token in QR payload.
    #[arg(long)]
    pub token: Option<String>,

    /// Use password instead of token.
    #[arg(long)]
    pub password: Option<String>,

    /// Public URL for remote access.
    #[arg(long)]
    pub public_url: Option<String>,

    /// Generate QR for remote (public) access.
    #[arg(long)]
    pub remote: bool,
}
