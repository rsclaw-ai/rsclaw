// Default API types and their base URLs
// Mirrors defaults.toml / src/provider/defaults.rs
export type ApiType = "rsclaw" | "openai" | "openai-responses" | "anthropic" | "gemini" | "ollama";

export const API_TYPE_LABELS: Record<ApiType, string> = {
  rsclaw: "RsClaw (kvCache=2)",
  openai: "OpenAI Chat",
  "openai-responses": "OpenAI Responses",
  anthropic: "Anthropic",
  gemini: "Google Gemini",
  ollama: "Ollama",
};

export const API_TYPE_DEFAULT_URLS: Record<ApiType, string> = {
  rsclaw: "https://api.rsclaw.ai/v1/agent",
  openai: "https://api.openai.com/v1",
  "openai-responses": "https://api.openai.com/v1",
  anthropic: "https://api.anthropic.com/v1",
  gemini: "https://generativelanguage.googleapis.com/v1beta",
  ollama: "http://localhost:11434",
};

export const API_TYPE_AUTH_STYLES: Record<ApiType, string> = {
  rsclaw: "bearer",
  openai: "bearer",
  "openai-responses": "bearer",
  anthropic: "x-api-key",
  gemini: "bearer",
  ollama: "none",
};

// Whether the api type needs an API key
export const API_TYPE_NEEDS_KEY: Record<ApiType, boolean> = {
  rsclaw: true,
  openai: true,
  "openai-responses": true,
  anthropic: true,
  gemini: true,
  ollama: false,
};
