# Branding

RsClaw reads its identity from a **brand file** at compile time: product
names, the data directory, the config file name, service identifiers, the
User-Agent and the hosted endpoints. The `rsclaw-brand` crate parses the file
in its build script and generates constants that the rest of the workspace
uses. Nothing is read at run time, so a branded binary cannot be switched
back by an environment variable or a config edit.

The default brand file is `crates/rsclaw-brand/brand/rsclaw.toml`. With it,
every generated value is identical to the historical hard-coded one, byte for
byte. That includes the system-prompt text, so prompt-cache prefixes do not
change.

## Building with another brand

```sh
RSCLAW_BRAND_FILE=brands/acme.toml cargo brd
```

`RSCLAW_BRAND_FILE` is an absolute path, or a path relative to the workspace
root. Cargo rebuilds `rsclaw-brand` and everything that depends on it when the
variable or the file changes. An invalid file fails the build and names the
bad field.

## Fields

Every field is required. Unknown fields are rejected, so a typo cannot fall
back silently to another brand's value.

| Field | Default | Used for |
| --- | --- | --- |
| `id` | `rsclaw` | Machine id of the brand (slug) |
| `product_name` | `RsClaw` | Product name in CLI output, channel messages, the tray and prompts |
| `product_name_zh` | `螃蟹AI` | Chinese product name |
| `assistant_name` | `Crab AI Assistant` | Default assistant identity in the bootstrap workspace files |
| `assistant_name_zh` | `螃蟹AI助手` | Chinese assistant identity |
| `emoji` | `🦀` | Brand mark in channel messages (may be empty) |
| `website` | `rsclaw.ai` | Host shown in channel card titles (no scheme) |
| `cli_name` | `rsclaw` | Command name in help and messages, and the binary name inside update archives |
| `home_dir` | `.rsclaw` | Data directory under `$HOME`. `--dev` and `--profile` append `-dev` and `-<name>`. |
| `config_file` | `rsclaw.json5` | Config file name in the data directory. `.` + this name is the per-project config. |
| `service_label` | `ai.rsclaw.gateway` | macOS launchd label and plist name |
| `service_unit` | `rsclaw-gateway` | systemd unit name |
| `windows_service` | `rsclaw` | Windows service name (`sc`) |
| `service_display` | `RsClaw AI Gateway` | Windows service display name |
| `user_agent` | `rsclaw` | User-Agent product token (`<token>/<version>`) |
| `release_repo` | `rsclaw-ai/rsclaw` | GitHub `owner/repo` used as the release fallback for `update`. Empty disables the fallback. |
| `urls.api` | `https://api.rsclaw.ai` | Managed model, media and embedding API (`/v1` is appended) |
| `urls.hub` | `https://hub.rsclaw.ai` | Skill, plugin and tool hub (`/tools/manifest.json`, `/meta.json`, allowlists) |
| `urls.app` | `https://app.rsclaw.ai` | Release metadata (`/api/version`) and the remote defaults catalog (`/defaults.toml`). **Empty** turns both off: `update` reports that the distributor delivers updates, and the gateway never replaces the brand's catalog with a remote one. A brand without its own release host must leave it empty; pointing it at another brand's host would offer that brand's binaries and catalog. |

URLs must start with `http://` or `https://` and must not end with `/`; only `urls.app` may be empty. Slug
fields accept lowercase ASCII letters, digits, `-` and `_`.

## Using the brand in code

- Use the constants: `rsclaw_brand::HOME_DIR`, `CONFIG_FILE`, `API_V1_URL`, and
  so on.
- Every constant also has a literal macro, for `concat!` and attribute
  positions: `concat!(rsclaw_brand::brand_user_agent!(), "/dev")`.
- `rsclaw_brand::rebrand(text)` rewrites a user-facing template written with
  the default brand's words: `RsClaw`, `Crab AI Assistant`, `螃蟹AI助手`,
  `~/.rsclaw`, `rsclaw.json5`, `rsclaw.ai`, `🦀`, and `rsclaw <subcommand>`
  invocations. With the default brand it returns the input unchanged and
  borrowed. Use it for text blocks, such as translations and bootstrap
  prompts, where splicing in constants would be awkward.

## CLI output and help

CLI messages are written with the default brand's words and are rebranded
where they leave the process:

- **Printed lines.** In the CLI command modules (`rsclaw-runtime` `cmd/` and
  `rsclaw-tools`), `println!`/`eprintln!`/`print!`/`eprint!` are shadowed by
  macros that call `rsclaw_brand::brand_line(template, line)`. A line is
  rebranded only when its format template contains default-brand words, so
  data printed through `"{}"`-style templates (JSON, replies, file contents)
  is never rewritten. The styling helpers (`bold`, `dim`, `ok`, `banner`, …)
  rebrand their text, and non-default brands get a plain title instead of
  the RSCLAW word mark.
- **Errors.** CLI errors that name a command use `brand_bail!` (in `cmd/`),
  `rebrand(...)`, or `concat!(…, brand_cli_name!(), …)`.
- **`--help`.** The root `Cli` runs every argument and subcommand through
  `rebrand` after the command tree is built (`rsclaw-cli/src/brand_help.rs`),
  so doc comments keep the default wording.
- **Shell completions** are generated for `cli_name`.

## What does not follow the brand

Some identifiers are compatibility or protocol surface, not branding. They
keep their names in every build:

- **`RSCLAW_*` environment variables.** Deployment scripts and the desktop app
  set them. A brand can document them under its own name, but the variables
  keep these names.
- **The `rsclaw` provider id and `rsclaw-*` model names.** These are protocol
  identifiers of the managed model service. They appear in user configs and
  must stay stable.
- **Prompt-cache prefix ids, the plugin device-key salt, and other crypto or
  protocol constants.** Changing them breaks caches, signatures or stored
  data.
- **Crate names and the Cargo binary name.** The binary is always built as
  `rsclaw`. Rename it when you package it (installer, archive or app bundle),
  and set `cli_name` to the same name so help text, messages and `update`
  stay consistent.
- **On-disk marker files** inside the data directory, such as
  `.rsclaw-source.json` and `.rsclaw-managed`. They live under the branded
  `home_dir` and are never shown to users.
- **The desktop UI (`ui/`).** It has its own branding.

## Defaults catalog

The catalog compiled into the binary (`defaults.toml`: providers, channels,
search engines, exec-safety rules, ...) is produced by
`crates/rsclaw-config/build.rs`:

1. With the default brand and no override it is the workspace
   `defaults.toml`, byte for byte.
2. With another brand, the hosted URLs (`urls.api`, `urls.hub`, `urls.app`)
   and the data-dir paths in the exec-safety rules follow the brand file.
3. `RSCLAW_DEFAULTS_OVERRIDE=<file>` (absolute or workspace-relative) merges
   an override document on top, keeping the catalog's comments and layout:
   - tables merge key by key;
   - `[[providers]]`, `[[channels]]` and other arrays of tables match
     entries by `name`: a match is merged, `_remove = true` drops it, an
     unmatched entry is appended;
   - any other value replaces the catalog's value.

```toml
# defaults.override.toml
[meta]
defaults_version = "2026.10.7-acme.1"   # bump so existing installs refresh

[[providers]]
name  = "rsclaw"
label = "Acme Cloud (recommended)"

[[providers]]
name    = "openrouter"
_remove = true
```

Bump `meta.defaults_version` whenever an override changes shipped entries;
installed copies only refresh on a version change.

## Brand directory

The directory that holds the brand file may also ship optional assets. The
build picks them up automatically (adding or removing a file triggers a
rebuild) and validates them: JSON5 files must be objects, TOML must parse,
Markdown must be non-empty UTF-8. The default brand directory
(`crates/rsclaw-brand/brand/`) ships none, so stock builds are unchanged.

| File | Effect |
| --- | --- |
| `config.defaults.json5` | Default config layer, **under** the user's config file |
| `policy.locked.json5` | Locked config layer, **over** the user's config file |
| `defaults.override.toml` | Override for the defaults catalog (used when `RSCLAW_DEFAULTS_OVERRIDE` is unset; see "Defaults catalog") |
| `prompts/policy.md` | Distributor policy prompt |
| `prompts/policy.zh.md`, `prompts/policy.en.md` | Language-specific policy prompt (optional) |
| `prompts/<zh\|en>/<SOUL\|IDENTITY\|AGENTS\|USER\|HEARTBEAT>.md` | Bootstrap workspace templates |

### Config layers

The effective config is `config.defaults.json5 ⊕ user config ⊕
policy.locked.json5`, merged on the JSON value before schema
deserialization:

- objects merge key by key, recursively;
- arrays and scalars replace (so a default `agents.list` is replaced
  entirely by a user `agents.list`);
- `${VAR}` placeholders are not expanded inside the layers.

The layers never reach the user's file: config editors read and write only
what the user wrote. A user value under a locked path is kept in the file
but has no effect; `PUT /api/v1/config` and the WS `config.set` report such
paths in `lockedIgnored`. If the layers make a config fail schema
validation, the load error says so. A unit test
(`brand_layers::tests::compiled_layers_yield_a_valid_config`) checks that the
compiled layers validate on top of an empty user config.

### Prompts

Bootstrap templates replace the stock `SOUL.md`, `IDENTITY.md`, ... when a
new workspace is seeded (per language; the stock template is used for a file
the brand does not ship). They are written once and stay user-editable.

The policy prompt is not editable by users. It is injected into every
system prompt (all agents, channels and sub-agents, and the minimal prompt of
internal sessions) as a block titled `## Distributor policy`
(`## 发行方规则` when the gateway language is Chinese). It opens the per-client
suffix, right **after** the shared system prefix, so the prefix used for the
managed model's prompt cache stays byte-identical across brands and no new
prefix id is needed. Language choice: the language-specific file, then
`policy.md`, then the other language.

### Inspecting a build

`GET /api/v1/brand` returns the brand identity and its config layers:

```json
{
  "id": "acme", "productName": "Acme Agent", "productNameZh": "…",
  "assistantName": "Acme Assistant", "cliName": "acme-agent",
  "website": "acme.example",
  "lockedPaths": ["tools.exec.safety"],
  "hasPolicyPrompt": true, "hasDefaults": true
}
```

`doctor` prints a brand line (id, number of locked keys, policy prompt) for
non-default builds.
