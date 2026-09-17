# Continuo Release Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make continuo installable with `cargo install continuo-player`, backed by a CI pipeline that verifies formatting, lints, tests on Linux and macOS, docs, and packaging, plus a tag-triggered release workflow.

**Architecture:** One crate, no workspace split and no cargo features. The package is renamed to `continuo-player` on crates.io while `[lib]` and `[[bin]]` keep the name `continuo`, so no import and no command spelling changes. `exclude` drops the 17 MB of test fixtures and the docs directory to clear the 10 MB package cap. The single CI job becomes five, one of which runs the real packaging path on every pull request. Releases publish from a `v*` tag through crates.io Trusted Publishing.

**Tech Stack:** Rust 1.98.1 (pinned in `rust-toolchain.toml`), edition 2024, clap 4, GitHub Actions, crates.io Trusted Publishing.

**Spec:** `docs/superpowers/specs/2026-09-17-continuo-release-pipeline-design.md`

## Global Constraints

- Rust is pinned to **1.98.1** by `rust-toolchain.toml`; every workflow installs that exact toolchain, never `stable`.
- Package name on crates.io is **`continuo-player`**. The binary and the library are both **`continuo`** and must stay that way.
- `Cargo.lock` is committed. Every cargo command in CI uses `--locked`.
- License is **MIT**, copyright **Alexey Vymyatnin**.
- `[lints]` in `Cargo.toml` forbids `unsafe_code` and denies `unwrap_used` and `expect_used`. Do not relax them. `clippy.toml` already exempts tests.
- The packaged `.crate` must be **under 10 MB**.
- Commit messages carry **no `Co-authored-by` and no Claude attribution trailers**. A local pre-commit hook rejects them.
- Windows is neither built nor claimed anywhere.

---

### Task 1: Package identity and metadata

Renames the package for crates.io, adds every field publishing requires, and makes `cargo publish --dry-run` pass. Nothing else in the plan can be verified until packaging works.

**Files:**
- Modify: `Cargo.toml:1-5` (the `[package]` block)
- Create: `LICENSE`
- Modify: `Cargo.lock` (regenerated, not hand-edited)

**Interfaces:**
- Consumes: nothing.
- Produces: a packageable crate named `continuo-player` whose lib and bin targets are both `continuo`. Task 3's `package` job and Task 5's release job both run `cargo publish` against this.

- [ ] **Step 1: Run the packaging check to see the package blow the size cap**

Run: `cargo publish --dry-run --locked && ls -l target/package/*.crate`

Expected: the command **exits 0** — missing metadata is only a warning in this cargo version, not an error — but two things are wrong, and the size is the one that fails a check:

```
warning: crate continuo@0.1.0 already exists on crates.io index
warning: manifest has no description, license, license-file, documentation, homepage or repository
    Packaged 257 files, 20.2MiB (13.9MiB compressed)
```

The `.crate` file is about 13.9 MiB against the crates.io cap of 10 MB. Confirm the failing assertion directly:

Run: `test "$(stat -c%s target/package/continuo-0.1.0.crate)" -lt 10485760; echo "under cap: $?"`

Expected: prints `under cap: 1` — the check fails. That is this task's red state. Step 6 turns it green.

Do not expect an error about the missing fields; you will not get one. The metadata still has to be added, because crates.io rejects an upload without `description` and `license` even though the local dry run tolerates their absence.

- [ ] **Step 2: Rewrite the `[package]` block and add the target names**

Replace lines 1-5 of `Cargo.toml` (everything from `[package]` down to the `rust-version` line, leaving the blank line and `[dependencies]` that follow) with:

```toml
[package]
name         = "continuo-player"
version      = "0.1.0"
edition      = "2024"
rust-version = "1.98.1"
description  = "A keyboard-first terminal audio player for local files, HTTP media and podcasts"
license      = "MIT"
repository   = "https://github.com/alvytsk/continuo"
readme       = "README.md"
keywords     = ["audio", "player", "podcast", "tui", "terminal"]
categories   = ["multimedia::audio", "command-line-utilities"]
authors      = ["Alexey Vymyatnin <alvy.tsk@gmail.com>"]
exclude      = ["/tests", "/docs", "/.github", "/.claude", "/rust-toolchain.toml"]

[lib]
name = "continuo"

[[bin]]
name = "continuo"
path = "src/main.rs"
```

The package is `continuo-player`; the lib and bin stay `continuo`, which is why no `use continuo::...` import in `tests/` and no command spelling changes.

- [ ] **Step 3: Create the LICENSE file**

Create `LICENSE` with the MIT text:

```
MIT License

Copyright (c) 2026 Alexey Vymyatnin

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

- [ ] **Step 4: Regenerate Cargo.lock**

The package rename changes the root entry in `Cargo.lock`, so `--locked` commands fail until it is refreshed.

Run: `cargo check`

Expected: succeeds, and `git diff --stat Cargo.lock` shows the `continuo` root package entry replaced by `continuo-player`. Do not edit `Cargo.lock` by hand.

- [ ] **Step 5: Run the packaging check to verify it passes**

Run: `cargo publish --dry-run --locked --allow-dirty`

`--allow-dirty` is required: `cargo publish` refuses a working tree with uncommitted changes, and yours has them until Step 9. The CI job added in Task 3 runs on a clean checkout and deliberately does **not** pass this flag.

Expected: PASS, ending with `Packaged N files, X MiB`, and the metadata warning from Step 1 is gone.

- [ ] **Step 6: Verify the package now clears the 10 MB cap**

Run: `test "$(stat -c%s target/package/continuo-player-0.1.0.crate)" -lt 10485760; echo "under cap: $?"`

Expected: prints `under cap: 0` — the assertion that failed in Step 1 now passes. Run `ls -lh target/package/continuo-player-0.1.0.crate` to see the figure; it should be roughly 1-2 MB, down from 13.9 MiB.

If it is still over, `exclude` is wrong — confirm `/tests` and `/docs` are both listed.

- [ ] **Step 7: Verify the binary is still named `continuo`**

Run: `cargo build --locked && ls target/debug/continuo`

Expected: the file exists. A binary named `continuo-player` means `[[bin]] name` was not applied.

- [ ] **Step 8: Run the full suite to confirm the rename broke no imports**

Run: `cargo test --locked`

Expected: PASS. The `use continuo::...` imports across `tests/` resolve through `[lib] name`.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock LICENSE
git commit -m "feat: package as continuo-player for crates.io

The name continuo is taken on crates.io by an unrelated maintained
crate, so the package is continuo-player while [lib] and [[bin]] keep
the name continuo: imports and command spellings are unchanged. exclude
drops the 17 MB of test fixtures and the docs directory to clear the
10 MB package cap. Adds the MIT license the manifest now declares."
```

---

### Task 2: Bare `continuo` opens the player

Running the binary with no arguments currently exits 2 with a clap usage error. It becomes the full-screen player, so the first command a new installer types does something useful.

**Files:**
- Modify: `src/cli.rs:9-12` (the `Cli` struct)
- Modify: `src/app.rs:59-60` (the `run` signature and match head)
- Modify: `tests/cli.rs` (two tests assert the old usage-error behavior)
- Modify: `tests/m5_tui_process.rs` (add the PTY test for the new default)

**Interfaces:**
- Consumes: `crate::tui::run(TuiOptions) -> Result<RunOutcome, AppError>` and `TuiOptions { mouse: MouseMode, artwork: ArtworkMode }`, both unchanged.
- Produces: `continuo::cli::Cli.command` becomes `Option<CliCommand>`. Any later code matching on it must handle `None`.

- [ ] **Step 1: Write the failing parse test**

In `tests/cli.rs`, add these imports below the existing `mod process;` declaration:

```rust
use clap::Parser;
use continuo::cli::{Cli, CliCommand};
```

Then add this test:

```rust
/// A bare `continuo` is no longer a usage error: it resolves to no
/// subcommand, which `app::run` dispatches to the player.
#[test]
fn a_bare_invocation_parses_to_no_subcommand() -> Result<(), Box<dyn std::error::Error>> {
    let parsed = Cli::try_parse_from(["continuo"])?;
    assert!(parsed.command.is_none(), "bare invocation carries no subcommand");

    // Every existing subcommand still parses as it did.
    let tui = Cli::try_parse_from(["continuo", "tui"])?;
    assert!(matches!(tui.command, Some(CliCommand::Tui { .. })));
    let feeds = Cli::try_parse_from(["continuo", "feeds"])?;
    assert!(matches!(feeds.command, Some(CliCommand::Feeds)));
    Ok(())
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test cli a_bare_invocation_parses_to_no_subcommand`

Expected: FAIL to compile, with `no method named is_none found for enum CliCommand` — `command` is not yet an `Option`.

- [ ] **Step 3: Make the subcommand optional**

In `src/cli.rs`, replace the `Cli` struct (lines 9-12) with:

```rust
pub struct Cli {
    /// The subcommand to run. Absent means a bare `continuo`, which opens
    /// the full-screen player on the saved queue with the same defaults
    /// `continuo tui` uses when its flags are omitted.
    #[command(subcommand)]
    pub command: Option<CliCommand>,
}
```

- [ ] **Step 4: Dispatch the empty case to the player**

In `src/app.rs`, replace lines 59-60:

```rust
pub fn run(cli: cli::Cli) -> Result<RunOutcome, crate::error::AppError> {
    match cli.command {
```

with:

```rust
pub fn run(cli: cli::Cli) -> Result<RunOutcome, crate::error::AppError> {
    // A bare `continuo` opens the player. The defaults are the ones
    // `continuo tui` applies when neither flag is given.
    let Some(command) = cli.command else {
        return crate::tui::run(crate::tui::TuiOptions {
            mouse: cli::MouseMode::default(),
            artwork: cli::ArtworkMode::default(),
        });
    };
    match command {
```

The rest of the match body is untouched; its arms already bind `CliCommand` variants by value.

- [ ] **Step 5: Run the parse test to verify it passes**

Run: `cargo test --test cli a_bare_invocation_parses_to_no_subcommand`

Expected: PASS.

- [ ] **Step 6: Replace the two tests that assert the old behavior**

`tests/cli.rs` has two tests built on the old contract. Both must change.

Replace `bare_invocation_prints_help_and_reports_filter_errors` entirely with the half that is still true — the `RUST_LOG` path, which fires in `telemetry::init` before argument parsing and is unaffected by this change:

```rust
/// The `RUST_LOG` filter-error path predates the CLI and still applies: it
/// fires during `telemetry::init`, before argument parsing runs, so it is
/// independent of whether a subcommand was given.
#[test]
fn an_invalid_rust_log_filter_fails_before_argument_parsing() {
    let profile = process::Profile::new().unwrap();
    let failure = profile
        .command()
        .env("RUST_LOG", "continuo=not-a-level")
        .output()
        .unwrap();
    assert!(!failure.status.success());
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(stderr.contains("continuo: invalid tracing filter"));
    assert!(stderr.contains("application startup failed"));
    assert!(stderr.contains("error parsing level filter"));
}
```

Replace `a_bare_invocation_is_a_usage_error_with_status_two` with a test that keeps the §4 exit-code contract it was really guarding, using a genuine usage error instead of a bare invocation:

```rust
/// §4: usage errors exit 2. `play` with no source is still one; a bare
/// invocation is not, because it now opens the player.
#[test]
fn a_usage_error_still_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    let profile = process::Profile::new()?;
    let output = profile.command().arg("play").output()?;
    assert_eq!(output.status.code(), Some(2), "§4: usage errors exit 2");
    assert!(output.stdout.is_empty());
    Ok(())
}
```

Leave `help_is_a_successful_entry_point_on_stdout` exactly as it is — `--help` is handled by clap before the subcommand is resolved, so it is unaffected.

- [ ] **Step 7: Run the whole CLI test file**

Run: `cargo test --test cli`

Expected: PASS, four tests.

- [ ] **Step 8: Add the PTY test for the real behavior**

The parse test proves dispatch; this proves a bare invocation actually opens the player. Add to `tests/m5_tui_process.rs`, which already has the PTY harness and the `LEAVE_ALT` constant:

```rust
#[test]
fn a_bare_invocation_opens_the_player_and_q_restores_the_terminal() {
    let profile = process::Profile::new().expect("profile");
    // No arguments at all, where `["tui"]` would normally go.
    let mut child = PtyChild::spawn(profile.root(), &[], &[], 100, 30).expect("spawn");
    assert!(
        child.wait_for("Queue is empty", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"q");
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(0));
    assert!(child.output().contains(LEAVE_ALT));
}
```

- [ ] **Step 9: Run the PTY test**

Run: `cargo test --test m5_tui_process a_bare_invocation_opens_the_player`

Expected: PASS. If it hangs to the 10-second timeout, the `None` arm in `app.rs` is not reached — recheck Step 4.

- [ ] **Step 10: Run the full suite**

Run: `cargo test --locked`

Expected: PASS. Any other test spawning the binary with no arguments and expecting failure surfaces here; there should be none beyond the two replaced in Step 6.

- [ ] **Step 11: Commit**

```bash
git add src/cli.rs src/app.rs tests/cli.rs tests/m5_tui_process.rs
git commit -m "feat: a bare continuo opens the player

Running the binary with no arguments was a clap usage error on exit
code 2. It now opens the full-screen player on the saved queue with the
defaults continuo tui applies when its flags are omitted, so the first
command after cargo install does something useful. Every subcommand
keeps its spelling, and --help is unaffected. The two tests asserting
the old usage error are replaced: one keeps the RUST_LOG path it also
covered, the other keeps the exit-2 contract using play with no source."
```

---

### Task 3: Split CI into five jobs and add the macOS leg

Replaces the single `check` job. The macOS leg is what backs the platform claim the crates.io listing makes; the `package` job catches packaging breakage on pull requests instead of at release time, when a bad manifest costs a version number that can never be reused.

**Files:**
- Modify: `.github/workflows/ci.yml` (replaced wholesale)

**Interfaces:**
- Consumes: the packageable manifest from Task 1 — the `package` job runs `cargo publish --dry-run --locked`.
- Produces: nothing other tasks consume.

- [ ] **Step 1: Replace the workflow**

Replace the entire contents of `.github/workflows/ci.yml` with:

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

env:
  CARGO_TERM_COLOR: always

jobs:
  fmt:
    name: Format
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install pinned Rust toolchain
        run: rustup toolchain install 1.98.1 --profile minimal --component rustfmt
      - run: cargo fmt --check

  clippy:
    name: Clippy
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install ALSA development headers
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
      - name: Install pinned Rust toolchain
        run: rustup toolchain install 1.98.1 --profile minimal --component clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo clippy --locked --all-targets --all-features -- -D warnings

  test:
    name: Test (${{ matrix.os }})
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    steps:
      - uses: actions/checkout@v4
      - name: Install ALSA development headers
        if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
      - name: Install pinned Rust toolchain
        run: rustup toolchain install 1.98.1 --profile minimal
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --locked

  doc:
    name: Doc
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install ALSA development headers
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
      - name: Install pinned Rust toolchain
        run: rustup toolchain install 1.98.1 --profile minimal
      - uses: Swatinem/rust-cache@v2
      - run: cargo doc --locked --no-deps
        env:
          RUSTDOCFLAGS: "-D warnings"

  package:
    name: Package
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install ALSA development headers
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
      - name: Install pinned Rust toolchain
        run: rustup toolchain install 1.98.1 --profile minimal
      - uses: Swatinem/rust-cache@v2
      - run: cargo publish --dry-run --locked
      - name: Check the package clears the 10 MB crates.io cap
        run: |
          crate=$(ls target/package/*.crate)
          size=$(stat -c%s "$crate")
          echo "$crate is $size bytes"
          test "$size" -lt 10485760
```

`fail-fast: false` matters: if macOS fails, the Linux result is still wanted.

- [ ] **Step 2: Verify each job's command locally before trusting CI**

Run each in turn:

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo publish --dry-run --locked --allow-dirty
```

The last one needs `--allow-dirty` only because `ci.yml` is uncommitted while you run it locally. The job in the workflow runs on a clean checkout and must not carry the flag.

Expected: all five PASS. `cargo doc` with `-D warnings` is the one most likely to fail first, on a broken intra-doc link; fix any it reports.

- [ ] **Step 3: Verify the workflow is valid YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('ok')"`

Expected: prints `ok`.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: split into fmt, clippy, test, doc and package jobs

The single check job becomes five, and the test job gains a macOS leg so
the platform claim the crates.io listing makes is backed by a build
rather than assumed. The package job runs the real packaging path on
every pull request and asserts the 10 MB cap: packaging fails for
reasons ordinary builds never reveal, and finding that at release time
costs a version number that can never be reused. Adds rust-cache."
```

- [ ] **Step 5: Push and read the macOS result**

```bash
git push -u origin feat/release-pipeline-impl
```

Then watch the run: `gh run watch` (or `gh run list --branch feat/release-pipeline-impl`).

If the push is refused by a permission gate, stop and report it — do not try to work around it. Pushing is the controller's call, not yours.

Expected: all jobs pass. **If the macOS leg fails, stop and report it rather than working around it.** Nothing in the dependency set is known to be Linux-only, but no macOS build has ever been attempted, so a failure here is new information. Note that `tests/cli.rs`, `tests/m4_cli.rs` and `tests/m5_tui_process.rs` all carry `#![cfg(target_os = "linux")]`, so a macOS failure is a compile or a runtime failure elsewhere, most likely in the CPAL or CoreAudio path. Per the spec's §11, the fallback if it cannot be fixed cheaply is to drop macOS from the matrix and say so in the README — that is a decision to bring back, not to take alone.

---

### Task 4: Security audit workflow

**Files:**
- Create: `.github/workflows/audit.yml`

**Interfaces:**
- Consumes: nothing.
- Produces: nothing other tasks consume.

- [ ] **Step 1: Create the workflow**

Create `.github/workflows/audit.yml`:

```yaml
name: Security audit

on:
  schedule:
    - cron: "0 8 * * 1" # every Monday at 08:00 UTC
  push:
    paths:
      - "Cargo.lock"
  pull_request:
    paths:
      - "Cargo.lock"

permissions:
  contents: read

jobs:
  audit:
    name: cargo audit
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: rustsec/audit-check@v2
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
```

This uses the `rustsec/audit-check` action rather than `cargo install cargo-audit --locked`, which spends about two minutes compiling the tool on every run.

- [ ] **Step 2: Verify the workflow is valid YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/audit.yml')); print('ok')"`

Expected: prints `ok`.

- [ ] **Step 3: Check the current dependency set locally**

Run: `cargo install cargo-audit --locked && cargo audit`

Expected: either a clean report, or advisories listed. **If advisories are reported, record them and bring them back** — deciding whether to upgrade, patch, or ignore a specific advisory is not part of this plan.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/audit.yml
git commit -m "ci: audit the dependency set weekly and on lockfile changes

Runs cargo audit on a Monday cron and on any change to Cargo.lock,
through the rustsec action rather than compiling cargo-audit per run."
```

---

### Task 5: Release workflow

Publishes on a `v*` tag through crates.io Trusted Publishing, so no long-lived registry token is stored in the repository.

**Files:**
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: the packageable manifest from Task 1. The version check reads `version` from `cargo metadata` for the package `continuo-player`.
- Produces: nothing other tasks consume.

- [ ] **Step 1: Create the workflow**

Create `.github/workflows/release.yml`:

```yaml
name: Release

on:
  push:
    tags: ["v*"]
  workflow_dispatch:

permissions:
  contents: read
  id-token: write

env:
  CARGO_TERM_COLOR: always

jobs:
  publish:
    name: Publish to crates.io
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install ALSA development headers
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev

      - name: Install pinned Rust toolchain
        run: rustup toolchain install 1.98.1 --profile minimal

      - uses: Swatinem/rust-cache@v2

      - name: Verify the tag matches the manifest version
        if: github.event_name == 'push'
        run: |
          manifest=$(cargo metadata --no-deps --format-version 1 \
            | python3 -c "import json,sys; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name'] == 'continuo-player'))")
          tag="${GITHUB_REF_NAME#v}"
          echo "manifest=$manifest tag=$tag"
          test "$manifest" = "$tag"

      - run: cargo test --locked

      - name: Authenticate to crates.io
        uses: rust-lang/crates-io-auth-action@v1
        id: auth

      - name: Publish
        env:
          CARGO_REGISTRY_TOKEN: ${{ steps.auth.outputs.token }}
        run: |
          if [ "${{ github.event_name }}" = "workflow_dispatch" ]; then
            cargo publish --dry-run --locked
          else
            cargo publish --locked
          fi
```

The version check is skipped on a manual run, which has no tag. The `workflow_dispatch` path exists so the whole workflow, token minting included, can be exercised without publishing anything.

- [ ] **Step 2: Verify the workflow is valid YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('ok')"`

Expected: prints `ok`.

- [ ] **Step 3: Verify the version-extraction command works locally**

Run:

```bash
cargo metadata --no-deps --format-version 1 \
  | python3 -c "import json,sys; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name'] == 'continuo-player'))"
```

Expected: prints `0.1.0`. If it raises `StopIteration`, the package name in Task 1 does not match the one queried here.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: publish to crates.io from a v* tag

Verifies the tag matches the manifest version, runs the suite, then
publishes through crates.io Trusted Publishing so no registry token is
stored in the repository. A workflow_dispatch run does everything but
publish, so the path can be exercised before a real release."
```

---

### Task 6: Documentation

Brings the README, changelog and architecture document in line with a published crate. The absolute-link change is not cosmetic: `/docs` is excluded from the package, so relative links render broken on crates.io.

**Files:**
- Create: `CHANGELOG.md`
- Modify: `README.md` (the `## Install` section, and every relative link into `docs/`)
- Modify: `docs/architecture.md` (the deployment section)

**Interfaces:**
- Consumes: the package name `continuo-player` from Task 1.
- Produces: nothing other tasks consume.

- [ ] **Step 1: Create the changelog**

Create `CHANGELOG.md`:

```markdown
# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-09-17

First release. Published to crates.io as `continuo-player`; the installed
binary is `continuo`.

### Added

- Plays MP3, FLAC, WAV and M4A files, direct `http(s)://` URLs, and episodes
  of subscribed RSS or Atom feeds.
- Remembers the position in every track, URL and episode, and resumes there.
- Seeks over HTTP with range requests, including MP3 podcasts with no seek
  index.
- A full-screen terminal player with a persistent queue, a file and podcast
  browser, cover art and a spectrum display.
- Feed management from the player: subscribe, refresh and unsubscribe.
- A bare `continuo` opens the player.

[Unreleased]: https://github.com/alvytsk/continuo/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/alvytsk/continuo/releases/tag/v0.1.0
```

- [ ] **Step 2: Rewrite the README install section**

In `README.md`, replace the whole `## Install` section (from the `## Install` heading down to, but not including, `## Quick start`) with:

````markdown
## Install

```sh
cargo install continuo-player
```

The crate is published as `continuo-player` because the name `continuo` was
already taken on crates.io. The installed binary is `continuo`.

On Linux, install `libasound2-dev` first. CPAL needs the ALSA headers, and
the runtime `libasound.so.2` alone is not enough.

### From source

1. Install Rust through rustup. The repository pins Rust 1.98.1 with the
   rustfmt and clippy components.
2. On Linux, install `libasound2-dev`.
3. Build from the repository root:

```sh
cargo build --release --locked
```

The binary is `target/release/continuo`. The examples below assume it is on
your `PATH`.
````

- [ ] **Step 3: Make every link into `docs/` absolute**

Because `exclude` drops `/docs`, relative links break when crates.io renders the README.

Run this to find them: `grep -n 'docs/' README.md`

Rewrite each hit to an absolute URL. The screenshot becomes a raw URL so the image itself loads:

```markdown
![The terminal player: cover art, track information, spectrum, transport and the queue](https://raw.githubusercontent.com/alvytsk/continuo/main/docs/images/tui.webp)
```

and each document link becomes a blob URL, for example:

```markdown
[docs/m5-acceptance.md](https://github.com/alvytsk/continuo/blob/main/docs/m5-acceptance.md)
```

- [ ] **Step 4: Document the bare invocation in the commands table**

`README.md` has a commands table and a quick-start block. Add this row to the top of the table, above the `play` row:

```markdown
| _(no arguments)_ | Open the full-screen player on the saved queue |
```

And add this as the last line of the quick-start code block, after `continuo tui`:

```sh
continuo            # same as `continuo tui`
```

- [ ] **Step 5: Note the install path in the architecture document**

`docs/architecture.md` has the deployment section at `## 11. Deployment` (line 409). Append this paragraph to the end of that section, before whatever heading follows it:

```markdown
The crate ships to crates.io as `continuo-player`, because `continuo` was
already taken there by an unrelated crate. The published binary and the
library target are both named `continuo`, so neither the command nor the
`use continuo::...` imports are affected by the package name. A release is
cut by pushing a `v*` tag, which triggers `.github/workflows/release.yml`:
it verifies the tag matches the manifest version, runs the suite, and
publishes through crates.io Trusted Publishing. The package excludes
`/tests` and `/docs`, so the 17 MB of audio fixtures stay out of it.
```

- [ ] **Step 6: Verify no relative docs links remain in the README**

Run: `grep -n '](docs/\|](\./docs/' README.md || echo "none remain"`

Expected: prints `none remain`.

- [ ] **Step 7: Verify the packaged README is the one crates.io will render**

Run:

```bash
cargo publish --dry-run --locked --allow-dirty
tar -xzOf target/package/continuo-player-0.1.0.crate continuo-player-0.1.0/README.md | head -20
```

`--allow-dirty` is needed because the README edits are not committed until Step 8.

Expected: the extracted README shows the new install section and the absolute image URL.

- [ ] **Step 8: Commit**

```bash
git add CHANGELOG.md README.md docs/architecture.md
git commit -m "docs: document installing from crates.io

The README leads with cargo install continuo-player and keeps building
from source below it. Every link into docs/ becomes absolute, because
exclude drops that directory from the package and relative links render
broken on crates.io. Adds a changelog with the 0.1.0 entry."
```

---

## After the plan: the first publish

These steps are the maintainer's, not an implementer's, and are deliberately manual. They follow the spec's §7.

1. Merge `feat/release-pipeline-impl` (it contains the spec and plan commits too); confirm the `package` job is green on `main`.
2. `cargo publish --locked` from a local checkout of `main`, using a personal crates.io token. This is the first publish and is irreversible — a version can be yanked but never reused, and never re-uploaded.
3. Configure the trusted publisher on crates.io for `continuo-player`: repository `alvytsk/continuo`, workflow `release.yml`.
4. Run `release.yml` once through `workflow_dispatch` to prove the token-minting path against a dry run.

**No `v0.1.0` tag is pushed.** The tag trigger fires however a tag is created, so a `v0.1.0` tag would start a run whose publish step must fail — 0.1.0 is already on crates.io by then. The first tag ever pushed is `v0.1.1`.

From then on, a release is: bump `version` in `Cargo.toml`, move the changelog's `Unreleased` items under the new heading, commit, then `git tag v0.1.1 && git push --tags`.
