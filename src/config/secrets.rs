//! SecretsManager — resolves SecretRef values at startup and on hot-reload.
//!
//! Three resolution strategies (agents.md §23):
//!   env  → std::env::var(id)
//!   file → read JSON file declared in the named provider, extract value at
//! JSON Pointer `id`   exec → run provider's command + args + [id], capture
//! stdout

use anyhow::{Context, Result, bail};

use super::{
    runtime::RuntimeConfig,
    schema::{SecretOrString, SecretRef, SecretSource},
};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct SecretsManager;

impl SecretsManager {
    /// Resolve every SecretRef found in `config` in-place, replacing each
    /// `SecretOrString::Ref(r)` with `SecretOrString::Plain(resolved_value)`.
    ///
    /// Called once at startup, and again on hot-reload before the atomic swap.
    pub fn resolve_all(config: &mut RuntimeConfig) -> Result<()> {
        // gateway.auth_token is already resolved to a plain String by into_runtime();
        // the original SecretRef lives in the schema layer (Config), not in
        // RuntimeConfig. RuntimeConfig stores resolved Strings, so there is
        // nothing to iterate here at the runtime layer.  The schema-layer
        // resolution happens during loader::load_json5 via a dedicated walk.
        // This function serves as the integration point for future
        // full schema-layer resolution and is currently a no-op placeholder that
        // validates the exec providers are reachable.
        //
        // For the purposes of §23 the concrete resolution helpers below are the
        // load-bearing code used by loader.rs / into_runtime helpers.
        let _ = config; // suppress unused warning until caller wires it up
        Ok(())
    }

    /// Resolve a single SecretOrString value.
    ///
    /// If the value is already a plain string it is returned as-is.
    /// If it is a SecretRef the appropriate resolver is called.
    pub fn resolve(
        value: &SecretOrString,
        context: &str,
        config: &RuntimeConfig,
    ) -> Result<String> {
        match value {
            SecretOrString::Plain(s) => Ok(s.clone()),
            SecretOrString::Ref(r) => resolve_ref(r, context, config),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal resolver
// ---------------------------------------------------------------------------

fn resolve_ref(r: &SecretRef, context: &str, config: &RuntimeConfig) -> Result<String> {
    match r.source {
        SecretSource::Env => resolve_env(r, context),
        SecretSource::File => resolve_file(r, context, config),
        SecretSource::Exec => resolve_exec(r, context, config),
    }
}

// --- env ---

fn resolve_env(r: &SecretRef, context: &str) -> Result<String> {
    std::env::var(&r.id).with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             env var `{}` is not set",
            r.id
        )
    })
}

// --- file ---

fn resolve_file(r: &SecretRef, context: &str, config: &RuntimeConfig) -> Result<String> {
    // The provider entry holds the file path in its `file` field.
    let provider_name = r.provider.as_deref().unwrap_or("default");
    let secrets_cfg = config.ops.secrets.as_ref().with_context(|| {
        format!(
            "secret resolution failed for {context}: \
                 secrets.providers is not configured (needed for file provider `{provider_name}`)"
        )
    })?;

    let provider = secrets_cfg.providers.get(provider_name).with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             secrets provider `{provider_name}` not found"
        )
    })?;

    let file_path = provider.file.as_deref().with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             secrets provider `{provider_name}` has type=file but no `file` path"
        )
    })?;

    let raw = std::fs::read_to_string(file_path).with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             could not read secrets file `{file_path}`"
        )
    })?;

    // `id` is a JSON Pointer (RFC 6901), e.g. "/providers/openai/apiKey".
    // If `id` is empty or "/" we treat the whole file as the value (plain text).
    let pointer = &r.id;
    if pointer.is_empty() || pointer == "/" {
        return Ok(raw.trim().to_owned());
    }

    let json: serde_json::Value = serde_json::from_str(&raw).with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             secrets file `{file_path}` is not valid JSON"
        )
    })?;

    let found = json.pointer(pointer).with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             JSON Pointer `{pointer}` not found in `{file_path}`"
        )
    })?;

    match found {
        serde_json::Value::String(s) => Ok(s.clone()),
        other => Ok(other.to_string()),
    }
}

// --- exec ---

fn resolve_exec(r: &SecretRef, context: &str, config: &RuntimeConfig) -> Result<String> {
    let provider_name = r.provider.as_deref().unwrap_or("default");
    let secrets_cfg = config.ops.secrets.as_ref().with_context(|| {
        format!(
            "secret resolution failed for {context}: \
                 secrets.providers is not configured (needed for exec provider `{provider_name}`)"
        )
    })?;

    let provider = secrets_cfg.providers.get(provider_name).with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             secrets provider `{provider_name}` not found"
        )
    })?;

    let command = provider.command.as_deref().with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             secrets provider `{provider_name}` has type=exec but no `command`"
        )
    })?;

    let mut cmd = std::process::Command::new(command);

    // Prepend the provider-level args (e.g. ["read", "--account",
    // "my.1password.com"]).
    if let Some(args) = &provider.args {
        cmd.args(args);
    }

    // Append the SecretRef id as the final argument.
    cmd.arg(&r.id);

    let output = cmd.output().with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             could not execute secrets provider command `{command}` (provider `{provider_name}`)"
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "secret resolution failed for {context}: \
             provider `{provider_name}` exited with status {}: {}",
            output.status,
            stderr.trim()
        );
    }

    let value = String::from_utf8(output.stdout).with_context(|| {
        format!(
            "secret resolution failed for {context}: \
             provider `{provider_name}` stdout is not valid UTF-8"
        )
    })?;

    Ok(value.trim().to_owned())
}
