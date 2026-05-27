# Contributing to RsClaw

Thanks for the interest. This file is the **human-oriented** quick-start. The full engineering bible — architecture, internal abstractions, what RsClaw is and isn't — lives in [AGENTS.md](AGENTS.md). Read that before any non-trivial work.

---

## Ground rules

- **Branch**: work on `dev`. `main` is for releases — never push to it directly. Tags `vX.Y.Z` (CLI) and `app-vX.Y.Z` (desktop) are cut from `main` once `dev` is stable.
- **License**: by submitting code you agree it's dual-licensed under MIT **OR** Apache-2.0 — same terms as the rest of the project.
- **Identity scope**: RsClaw is an agent product, not a multi-provider LLM gateway. Read the "Identity" section of [AGENTS.md](AGENTS.md) before adding features that "feel gateway-shaped" (vault, tiered pricing, cross-provider routing exposed to clients).
- **Don't introduce abstractions or scope on speculation.** A bug fix doesn't need a refactor. Three similar lines beats a premature trait. If the task is small, the PR is small.

---

## Setup

```bash
git clone https://github.com/rsclaw-ai/rsclaw.git
cd rsclaw
git checkout dev

# Rust 1.91+
rustup show          # confirm toolchain
```

System deps (only what you'll touch needs to be installed):

- `protoc` — Linux: `apt install protobuf-compiler` · macOS: `brew install protobuf` · Windows: `choco install protoc`
- `ffmpeg` — only for voice channels / TTS
- Chrome — only for browser-automation features (rsclaw will install one if absent)

---

## Build

**Always `cargo brd`** (alias for `cargo build --profile release-dev`):

```bash
cargo brd                                    # default
RUST_LOG=rsclaw=debug cargo run -- gateway run
```

Why not `cargo build` or `cargo build --release`?

- `cargo build` (debug) is too slow for the agent loop — tail latencies blow up and you'll waste hours chasing phantom perf regressions.
- `cargo build --release` (publishing profile) costs ~10 min cold. Reserve for actual release builds.
- `release-dev` is optimized but with debug-info — fast enough to behave like prod, fast enough to iterate.

Restart a running gateway after a rebuild:

```bash
./target/release-dev/rsclaw gateway restart      # uses your dev binary
# (do NOT `cargo run -- gateway restart` — that re-enters cargo, debug profile, slow)
```

---

## Test

**Never `cargo test --lib` without a name filter.** Some tests (e.g. `store::redb_store::tests::upgrades_v2_database_to_v3_*`) historically `current_exe()`-self-spawned and fork-bombed the host. This is fixed under `cfg(test)` since `0f54a02`, but the discipline stands — running the full suite blindly is rarely what you want, and a filter:

- catches the test you care about in seconds
- isolates expensive integration tests
- works on Windows / macOS / Linux uniformly

```bash
cargo test --lib store::redb_store::tests::upgrade
cargo test --lib agent::memory::tests::
cargo test --lib provider::rsclaw
```

For CI parity (full suite), use the release-cli.yml flow on a tag push — local `cargo test --all` on macOS can hit CJK-font panics absent on CI runners (and vice versa).

---

## Code style

- **Comments**: write the *why*, not the *what*. Don't reference the current task / PR / issue — that belongs in the commit message and rots in the code.
- **No emojis in source** unless the user asks for them on a UI string.
- **No multi-paragraph docstrings**. One line max.
- **Error handling**: only validate at system boundaries (user input, external APIs). Trust internal callers and framework guarantees.
- **No feature flags or back-compat shims for hypothetical futures**. If the API is changing, change it; clean up the call sites.
- **CJK safety**: never `&s[..n]` for truncation — panics mid-CJK-char. Use `crate::util::truncate_str`. (Background: [feedback_cjk_safe_truncate](docs/lang/) — swept 36 sites in `ae11ddc`.)
- **Tools field**: never move tool definitions from the LLM request's `tools:` field into messages-as-text. It tanks tool-calling quality.

---

## Commits

Conventional-commits-ish format, scope optional but encouraged:

```
fix(memory): extractor must preserve user's language (no auto-translate)
fix(cmd/gateway): missing CommandExt import broke Windows release build
ci: explicit rustup target add to work around dtolnay action regression
feat(provider/rsclaw): v1.9 user_tools wire — base/user tool segment split
chore(deps): bump tokio from 1.49 to 1.50
```

Body (optional) explains the *why* and the constraint, not the *what*. Example:

```
fix(store/redb): cfg(test) guard against current_exe() fork bomb

Under `cargo test`, current_exe() resolves to the test binary — which
does NOT contain main.rs (cargo generates its own harness entrypoint).
The spawned child has no dispatch for LEGACY_REDB_UPGRADE_HELPER_ENV
and runs the FULL test suite, which re-triggers the upgrade test, which
spawns ANOTHER child — fork bomb.

In test builds, just do the upgrade in-process. We forfeit the
library-isolation guarantee, but tests run in controlled environments
where mixing v2 + v3 redb symbols isn't a concern.
```

- Don't add `Co-Authored-By` lines.
- Don't `--amend` published commits — make a new commit.
- Don't `--no-verify` on hooks.
- One concern per commit. Easier to revert, easier to review.

---

## Pull requests

1. Branch from `dev`, push your branch.
2. Open a PR against `dev`. Title = a single conventional-commit line; body = problem + approach + verification.
3. CI must pass. If CI flakes (it does — see `.github/workflows/release-cli.yml` + the dtolnay action saga in commit history), document the flake in the PR thread and re-run.
4. One reviewer LGTM minimum; two for anything touching `src/a2a/`, `src/agent/runtime.rs`, `src/store/`, `src/server/mod.rs`, or `src/provider/`.
5. Squash-merge by default. Keep merge commits for cross-cutting refactors that genuinely benefit from preserved history.
6. After merge, delete the branch.

### What needs explicit pre-discussion (open an issue first)

- New protocol surfaces (A2A method extensions, custom WebSocket frames)
- New core dependencies (anything that pulls 50+ transitive crates)
- Behavior changes that break config-file compatibility
- Changes to `tests/fixtures/baseline-*.json` (these anchor the rsclaw-llm KV prefix cache — coordinate with the fleet team)
- Anything in `src/a2a/relay.rs` — hub-spoke topology has subtle invariants

---

## Reviewing

If you're reviewing someone else's PR:

- Verify the build (`cargo brd`) and the relevant test (with a filter) actually pass.
- Read the diff with the question: "what's the smallest change that would also pass the test?" — if the PR is larger, ask why.
- Don't sit on stale PRs. Decline + explain is better than ghosting.
- Suggest, don't dictate. Style preferences are not blockers; correctness, safety, and architectural fit are.

---

## Releasing (maintainers)

Releases are dual-tagged. For version `X.Y.Z`:

```bash
git checkout dev
# verify tests; bump Cargo.toml [package].version if needed
git push origin dev:main                # FF dev → main
git tag vX.Y.Z app-vX.Y.Z               # both tags at the same commit
git push origin vX.Y.Z app-vX.Y.Z
```

Pushing the tags triggers `release-cli.yml` (binary tarballs for mac/linux/windows × arm64/x86_64) and `release-desktop.yml` (DMG / DEB / EXE). After the workflows finish, the `update-tap` job auto-updates [`rsclaw-ai/homebrew-tap`](https://github.com/rsclaw-ai/homebrew-tap) Formula and Cask — `brew upgrade rsclaw` picks it up.

To publish to crates.io:

```bash
cargo publish --dry-run       # sanity-check packaging
cargo publish                 # irreversible
```

---

## Questions

- 🐛 Bug or unclear behavior — open an [Issue](https://github.com/rsclaw-ai/rsclaw/issues) with a minimal repro.
- 💡 Design idea or RFC — start a [Discussion](https://github.com/rsclaw-ai/rsclaw/discussions) or open an issue tagged `design`.
- 💬 Community channels — [WeChat / Feishu / QQ / Telegram](https://rsclaw.ai/en/community).

Thanks for showing up.
