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
