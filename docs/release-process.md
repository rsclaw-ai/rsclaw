# Release Process

How to cut a RsClaw release, and the traps that have actually bitten us.

Two artifacts ship per release and **both tags must be pushed together**:

| Tag | Workflow | Artifact |
|---|---|---|
| `vX.Y.Z` | `.github/workflows/release-cli.yml` | CLI binaries, 6 targets |
| `app-vX.Y.Z` | `.github/workflows/release-desktop.yml` | Tauri desktop installers |

Both trigger on tag push (`v*` / `app-v*`) and also expose
`workflow_dispatch` with a `tag` input for re-runs.

Targets (both workflows):

```
x86_64-unknown-linux-gnu    aarch64-unknown-linux-gnu
x86_64-apple-darwin         aarch64-apple-darwin
x86_64-pc-windows-msvc      aarch64-pc-windows-msvc
```

---

## Why tags must be cut from `main`

**`ci.yml` runs on `main` only — by design:**

```yaml
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
```

`main` is the release branch, and CI is the gate in front of it. `dev` is
the working branch and is deliberately not gated — that is what keeps
day-to-day iteration fast.

The contract that makes this work: **release tags point at commits that
have passed the `main` gate.** Tagging anywhere else skips the gate.

`v2026.8.6` is what happens when the contract is broken. The tag was cut on
`dev`, which was **103 commits ahead of `main`** at the time, so nothing in
it had been through CI. All 6 non-macOS jobs failed on one line:

```rust
// crates/rsclaw-desktop/src/native.rs — inside #[cfg(not(target_os = "macos"))]
enigo::Axis::Vertical   // `Axis` was never imported
```

It compiles locally on macOS because the branch is `cfg`-ed out, and it had
been sitting on `dev` since 2026-06-14 (`3486bee1`) — 7 weeks. Alongside it,
7 weeks of test-side drift had piled up (missing dev-deps, `AppState` gaining
11 fields, `include_str!` paths moved by the crate split); `cargo test --all`
did not even compile.

None of that is a CI defect. CI never claimed to cover `dev`; the release
simply bypassed it.

> **Rule: merge to `main`, let CI go green there, then tag that commit.**
> A green badge describes `main` — so make sure `main` is what you are
> shipping.

Belt and braces before tagging, since a local run is faster than a failed
release:

```bash
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test --all
cd ui && yarn tsc --noEmit
```

Cross-compiling at least one non-macOS target locally is cheap insurance
against exactly the `Axis`-class bug:

```bash
cargo check -p rsclaw-desktop --target x86_64-pc-windows-msvc
```

---

## 1. Bump the version

Nine files carry the version string. Miss one and the built binary reports a
version that does not match the tag:

```
Cargo.toml
crates/rsclaw-runtime/Cargo.toml
ui/package.json
ui/src-tauri/Cargo.toml
ui/src-tauri/tauri.conf.json
defaults.toml
ui/app/components/rsclaw-panel.tsx
ui/app/components/onboarding.tsx
ui/app/lib/catalog.ts          # includes defaultUserAgent
```

Find any stragglers:

```bash
grep -rl "OLD\.VERSION" --include="*.toml" --include="*.json" \
  --include="*.json5" --include="*.ts" --include="*.tsx" . \
  | grep -v node_modules | grep -v "^./target"
```

Then sync both lockfiles (`Cargo.lock`, `ui/src-tauri/Cargo.lock`) — a build
that rewrites a lockfile mid-release is a dirty tree waiting to happen.

## 2. Verify, then land on `main`

Run the full check from the section above. Only then:

```bash
git checkout main
git merge dev --ff-only
```

**Tags must point at a commit on `main`.** Cutting from a `dev`-only commit
is what let `v2026.8.6` ship untested code.

## 3. Tag and push

```bash
git tag vX.Y.Z
git tag app-vX.Y.Z
git push origin main --tags
```

Re-cutting a tag that already exists remotely (delete both, then re-create):

```bash
git tag -d vX.Y.Z app-vX.Y.Z
git push origin :refs/tags/vX.Y.Z :refs/tags/app-vX.Y.Z
git tag vX.Y.Z && git tag app-vX.Y.Z
git push origin main --tags
```

## 4. Watch the runs

```bash
gh run list --limit 5
gh run view <run-id> --json status,jobs \
  -q '.jobs[] | "\(.status)\t\(.conclusion // "-")\t\(.name)"'
```

---

## Failure triage

### Is it our code or GitHub?

Check **which step** failed before reading any compiler output:

```bash
gh api repos/:owner/:repo/actions/jobs/<job-id> \
  -q '.steps[] | "\(.status)\t\(.conclusion // "-")\t\(.name)"'
```

A failure in `Set up job` is **never** our code. It looks like this:

```
Failed to resolve action download info. Error: Service Unavailable
##[error]Service Unavailable
```

That is a GitHub infrastructure outage — the runner never got as far as
checking out the repo. During the `v2026.8.6` release this took out jobs
across several attempts. Wait for recovery (`curl -s -o /dev/null -w "%{http_code}"
https://api.github.com`), then re-run.

A real code failure shows a `Build`/`Test`/`Clippy` step failing with
compiler output.

### Confirm a regression against a known-good tag

Re-run an older tag's workflow. If it passes on the same runner images, the
environment is fine and the regression is genuinely ours. This is how the
`Axis` bug was isolated: old tag `v2026.6.13` went 6/6 green while
`v2026.8.6` failed 4/6.

### Re-running

```bash
gh run rerun <run-id> --failed   # only the failed jobs
gh run rerun <run-id>            # everything, including cancelled
```

Two gotchas:

- **A run with any job still in progress cannot be re-run** — you get
  `cannot be rerun; This workflow is already running`. Wait for it to finish.
- `--failed` skips `cancelled` jobs. After multiple overlapping re-runs the
  state gets tangled; it is cleaner to re-push the tag for a fresh run than
  to keep patching a half-cancelled one.

---

## Notes

- **Never push without explicit approval** (AGENTS.md). `main` is
  release-only; `dev` is the default working branch.
- Both `vX.Y.Z` and `app-vX.Y.Z` must ship together, or the desktop app and
  CLI drift apart.
- Use debug/`release-dev` builds while iterating. `cargo brd` →
  `target/release-dev/rsclaw`.
- Verify a produced binary before trusting it:
  ```bash
  ./rsclaw --version        # must match the tag
  shasum -a 256 rsclaw      # compare across build host and dist target
  ```

### Building on a bare Windows host

If CI is down or you need an out-of-band build, a clean Windows box needs
all of these before `cargo build --release --target x86_64-pc-windows-msvc`
will succeed:

| Dependency | Why |
|---|---|
| VS Build Tools (MSVC) | linker |
| Rust toolchain | — |
| Node | Tauri frontend |
| **protoc** | `lark-websocket-protobuf` |
| **LLVM / libclang** | `silk-rs` bindgen |

The last two are easy to miss — they are not Rust-level dependencies, and
their absence surfaces as opaque build-script errors.
