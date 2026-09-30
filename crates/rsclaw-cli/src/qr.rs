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
    /// Also read from `RSCLAW_AUTH_TOKEN` so the secret need not appear in
    /// argv (visible in `ps` / shell history).
    #[arg(long, env = "RSCLAW_AUTH_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    /// Use password instead of token.
    /// Also read from `RSCLAW_GATEWAY_PASSWORD` so the secret need not appear in
    /// argv (visible in `ps` / shell history).
    #[arg(long, env = "RSCLAW_GATEWAY_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,

    /// Public URL for remote access.
    #[arg(long)]
    pub public_url: Option<String>,

    /// Generate QR for remote (public) access.
    #[arg(long)]
    pub remote: bool,
}
