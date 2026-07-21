use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum ImageCommand {
    /// Describe an image using the vision model.
    Vision(ImageVisionArgs),

    /// Extract text from an image using OCR.
    Ocr(ImageOcrArgs),
}

#[derive(Args, Debug)]
pub struct ImageVisionArgs {
    /// Path to the image file.
    pub path: String,

    /// Optional prompt (default: "Describe this image in detail.").
    #[arg(long, short)]
    pub prompt: Option<String>,

    /// Model name (default: rsclaw-vision-v1).
    #[arg(long)]
    pub model: Option<String>,

    /// Maximum output tokens.
    #[arg(long)]
    pub max_tokens: Option<u32>,
}

#[derive(Args, Debug)]
pub struct ImageOcrArgs {
    /// Path to the image file.
    pub path: String,

    /// Optional prompt override.
    #[arg(long, short)]
    pub prompt: Option<String>,

    /// Model name (default: from kb.ocr config).
    #[arg(long)]
    pub model: Option<String>,

    /// Language hint for OCR (e.g. "zh", "ja", "en").
    #[arg(long)]
    pub lang: Option<String>,

    /// Maximum output tokens.
    #[arg(long)]
    pub max_tokens: Option<u32>,
}
