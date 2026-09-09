# Continuo

A keyboard-first terminal audio player being built for local audio, finite HTTP media, and podcasts.

Milestone 0 provides domain types, validated media identities, checkpoint values, tracing, and CI. The binary initializes tracing and exits; audio playback and a terminal UI are not implemented yet.

## Development

Install Rust through rustup. The repository pins Rust 1.98.1 and the rustfmt and clippy components. Dependency versions are recorded in the committed Cargo.lock.

Run from the repository root:

    cargo run --locked
    RUST_LOG=continuo=debug cargo run --locked
    cargo fmt --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked

Logging goes to stderr. Runtime code forbids unsafe code and denies unwrap/expect; tests may use unwrap/expect for assertions and fixtures.

M0 has no audio system dependency. M1 will require libasound2-dev on Linux when CPAL is introduced; the runtime libasound.so.2 alone is insufficient.

## Design and roadmap

Read the [architecture](docs/architecture.md) and [approved foundation spec](docs/superpowers/specs/2026-09-07-continuo-foundation-design.md).

M1 adds local playback; M2 adds durable resume; M3 adds finite HTTP playback; M4 adds feeds and subscriptions; M5 adds the TUI.

Non-UTF-8 local paths are unsupported. Position will be an estimate when device latency is unavailable, and seek support may remain unknown until probed. HTTP transport never implies live radio.

## Playback state

Position, completion and volume are written to a small JSON file so that
quitting and relaunching the same file resumes where you left off.

- **Linux:** `$XDG_STATE_HOME/continuo/state.json`, falling back to
  `~/.local/state/continuo/state.json`
- **macOS and Windows:** the platform's local data directory

The file holds one checkpoint per media identity, capped at 512 entries, and is
replaced atomically — a crash mid-write cannot leave a truncated file. A file
this build cannot read is preserved rather than overwritten: garbage is moved
aside as `state.json.rejected-<timestamp>`, and a file from a newer build is
left exactly where it is with writing disabled for that session.

Reaching the end of a track marks it complete and keeps the position it ended
at; reopening a completed track starts from the beginning.

Deleting `state.json` forgets every remembered position, which is also the way
out if a stored position ever stops a file from opening.
