# Continuo: release pipeline and crates.io publishing

Status: approved in conversation on 2026-09-17; ready for an implementation plan. The behavior below is the proposed contract, not an assertion that it already exists.

Branch: `feat/release-pipeline`.

## 1. Product decision

Continuo builds from source and has never been released. This work makes it installable with one command — `cargo install continuo-player` — and gives the repository the checks that a published crate needs.

The deliverable is a binary. The library target stays an implementation detail of that binary: no stable public API is promised, no semver discipline is claimed over the 90 modules `src/lib.rs` currently exports, and no crate is split out. If a reusable library is ever wanted, that is a separate decision taken later.

Three constraints shape everything below.

- **The name `continuo` is taken on crates.io.** It belongs to `continuo` v0.8.0, "Runtime service composition for multi-service Rust applications", last published 2026-09-12. It is maintained and unrelated, so the name will not be released.
- **The package exceeds the crates.io size cap.** `tests/fixtures` is 17 MB of audio and `docs` is 1.7 MB; the limit is 10 MB.
- **Only Linux is verified.** CI runs on `ubuntu-latest` alone, yet a crates.io listing offers the crate to every platform.

## 2. Existing foundations

- `Cargo.toml` declares `name`, `version`, `edition` and `rust-version` and nothing else. Every field crates.io requires — `description`, `license`, `repository` — is absent.
- `.github/workflows/ci.yml` holds one job, `check`, that installs ALSA headers and the pinned toolchain and then runs `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings` and `cargo test --locked`. There is no caching, no doc build, and no packaging check.
- `rust-toolchain.toml` pins 1.98.1 with rustfmt and clippy. `clippy.toml` relaxes the unwrap and expect denials inside tests. `[lints]` in `Cargo.toml` forbids `unsafe_code` and denies `unwrap_used` and `expect_used`. These stay as they are.
- `Cargo.lock` is committed, which is correct for a binary and is what lets every command below use `--locked`.
- There is no `LICENSE`, no `CHANGELOG.md`, and no git tag.
- `src/cli.rs` defines `Cli` with a required `command: CliCommand`. `src/app.rs:105` dispatches `CliCommand::Tui { mouse, artwork }` to `crate::tui::run`. That is the only call into `src/tui/`.
- `src/main.rs` already prints clap's own help and usage errors and maps clap's exit code, so a change to whether a subcommand is required needs no new error handling.

## 3. Package identity

`Cargo.toml` gains the publishing metadata and separates the package name from the binary and library names.

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

The package is `continuo-player` on crates.io. The installed binary and the library are both `continuo`, so `continuo tui` still runs and the `use continuo::...` imports in the integration tests are untouched. This is the only place the new name appears in code.

`exclude` takes the package from roughly 20 MB to under 2 MB.

- `/tests` goes out whole rather than `/tests/fixtures` alone: the test targets are useless without their fixtures, and `cargo package` verifies by building the library and binary, not the test targets.
- `/rust-toolchain.toml` goes out so an installing user is never asked to fetch the pinned 1.98.1 toolchain. It stays in the repository, where it governs development.

Excluding `/docs` breaks the README's relative links once the README is rendered on crates.io. Section 8 covers the fix.

## 4. Bare `continuo` opens the player

`continuo` with no arguments is currently a clap usage error on exit code 2. It becomes the full-screen player, so the first thing a new installer types does something useful.

- `src/cli.rs`: `Cli::command` becomes `Option<CliCommand>`.
- `src/app.rs`: a `None` arm calls `crate::tui::run` with `TuiOptions` built from `MouseMode::default()` and `ArtworkMode::default()` — the same values `continuo tui` uses when its flags are omitted.

Every existing subcommand keeps its spelling and behavior, `continuo tui` included. `continuo --help` and `continuo --version` are clap's and are unaffected, because clap handles them before the subcommand is resolved.

The test asserts that no arguments parse to the TUI path. It does not launch a terminal: `tests/m5_tui_process.rs` already covers the process-level behavior, and repeating it here would buy nothing.

## 5. Continuous integration

`.github/workflows/ci.yml` is replaced by five jobs, following the shape of the sibling r3sizer repository.

| Job | Runner | Command |
| --- | --- | --- |
| `fmt` | ubuntu-latest | `cargo fmt --check` |
| `clippy` | ubuntu-latest | `cargo clippy --locked --all-targets --all-features -- -D warnings` |
| `test` | ubuntu-latest, macos-latest | `cargo test --locked` |
| `doc` | ubuntu-latest | `cargo doc --locked --no-deps` with `RUSTDOCFLAGS: -D warnings` |
| `package` | ubuntu-latest | `cargo publish --dry-run --locked` |

Every job installs the pinned toolchain the way the current workflow does, keeps `CARGO_TERM_COLOR: always`, and adds `Swatinem/rust-cache@v2`, which the repository does not use today.

The ALSA headers step is conditional on `runner.os == 'Linux'`. macOS needs no package: `cpal` uses CoreAudio there, and the `signal-hook` and `gag` dependencies are already gated on `cfg(unix)`, which macOS satisfies.

Two jobs do work the current pipeline does not.

- **`test` on macOS** is what backs the platform claim the crates.io listing makes. It is also the job most likely to fail, since no macOS build has ever been attempted. A failure here is a finding, not a setback: it must be resolved, or the listing must say so, before the first publish.
- **`package`** runs the real packaging path on every pull request. Packaging fails for reasons ordinary builds never reveal — an `exclude` that drops a needed file, a size cap, a metadata field crates.io rejects. Finding that at release time costs a version number, because a crates.io version can be yanked but never reused.

## 6. Security audit

`.github/workflows/audit.yml` runs `cargo audit` on the same triggers r3sizer uses: a weekly cron on Monday at 08:00 UTC, and any push or pull request that touches `Cargo.lock`. Permissions are `contents: read`.

It uses the `rustsec/audit-check` action rather than r3sizer's `cargo install cargo-audit --locked`, which spends about two minutes compiling the tool on every run.

## 7. Release and publishing

`.github/workflows/release.yml` triggers on a pushed tag matching `v*`, and on `workflow_dispatch`, and runs one job:

1. Check out the tag.
2. Install the pinned toolchain and the ALSA headers.
3. Verify the tag matches the manifest — the version from `cargo metadata` must equal the tag with its leading `v` removed. A mismatch fails the job before anything is published. Skipped on a manual run, which has no tag.
4. `cargo test --locked`.
5. `cargo publish --locked`, or `cargo publish --dry-run --locked` when the run was started by `workflow_dispatch`.

The `workflow_dispatch` path exists so the whole workflow, token minting included, can be exercised without publishing anything. Section 7's ordering depends on it.

Permissions are `contents: read` and `id-token: write`.

**Authentication.** The workflow authenticates through crates.io Trusted Publishing, using `rust-lang/crates-io-auth-action` to mint a short-lived token from the job's OIDC identity. No `CARGO_REGISTRY_TOKEN` secret is stored in the repository.

**The first publish is manual and is not performed by this workflow.** It runs from the maintainer's machine with a personal crates.io token. Two reasons: a trusted publisher is configured against a crate that already exists on crates.io, and the first publish is irreversible, so it deserves a deliberate human act. The order is therefore:

1. Land everything in this spec; confirm `cargo publish --dry-run --locked` passes in CI.
2. `cargo publish --locked` locally, publishing `continuo-player` 0.1.0.
3. Configure the trusted publisher on crates.io for `alvytsk/continuo`, workflow `release.yml`.
4. Run `release.yml` once through `workflow_dispatch` to prove the trusted-publishing path end to end against a dry run.

**No `v0.1.0` tag is pushed.** The tag trigger fires however a tag is created, so a `v0.1.0` tag would start a run whose publish step must fail — 0.1.0 is already on crates.io by then, and a version can never be uploaded twice. The commit that publishes 0.1.0 is the record of it. The first tag ever pushed is `v0.1.1`.

From `v0.1.1` onward, releasing is `git tag v0.1.1 && git push --tags`, and the workflow publishes.

Version numbers are bumped by hand in `Cargo.toml` with a matching `CHANGELOG.md` entry. No release automation tool is introduced.

## 8. Supporting files and documentation

- `LICENSE` — MIT, copyright Alexey Vymyatnin, matching the `license` field and the sibling r3sizer repository.
- `CHANGELOG.md` — Keep a Changelog format, with a `0.1.0` entry describing the first release.
- `README.md` — the install section leads with `cargo install continuo-player` and keeps the build-from-source instructions below it, including the ALSA note. The section that says which milestones are implemented stays accurate.
- **Absolute links in `README.md`.** Because `/docs` is excluded from the package, the screenshot and the acceptance-document links must become absolute `https://github.com/alvytsk/continuo/...` URLs, or they render broken on crates.io. They keep working on GitHub.
- `docs/architecture.md` — the deployment section gains the crates.io path alongside building from source.

No `CONTRIBUTING.md` and no `SECURITY.md`. This is a single-maintainer project and neither file would say anything the README does not.

## 9. Out of scope

- Cargo feature flags of any kind. The earlier `tui` feature idea is dropped: it would gate `src/tui/` cheaply but could not shed `rustfft` or `image` without cfg-gating the playback engine's own fields and its realtime audio callback, and no headless consumer exists to justify either.
- Splitting the crate into a workspace.
- Prebuilt release binaries, cross-compilation, and any signing or notarization.
- Windows support. It is neither built nor claimed.
- Release automation such as release-please, and any change to how versions are chosen.
- A stable, curated public library API.

## 10. Validation

Automated, and all of it in CI:

- `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, and `cargo doc` with `-D warnings` pass.
- `cargo test --locked` passes on both ubuntu-latest and macos-latest.
- `cargo publish --dry-run --locked` passes, and the resulting `.crate` file is under 10 MB.
- The new test asserts that an empty argument list resolves to the TUI path, and that each existing subcommand still parses as it did.

By hand, before the first publish:

- `cargo install --path .` then `continuo` with no arguments opens the player; `continuo play`, `continuo feeds` and `continuo tui` are unchanged.
- The README renders correctly on crates.io, screenshot included, after the dry run's packaged README is inspected.

## 11. Risks

- **macOS may not build.** Nothing in the dependency set is known to be Linux-only, but nothing has been tried. If it fails and cannot be fixed cheaply, the fallback is to drop macOS from the matrix and say so in the README rather than ship an untested claim.
- **`continuo-player` could be taken between now and the first publish.** It was free on 2026-09-17. The manual first publish should follow shortly after this work lands.
