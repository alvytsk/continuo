# Continuo

A keyboard-first terminal audio player for local audio, finite HTTP media, and (from M4) podcasts.

Milestones 0 through 3 are implemented: domain types and identities (M0), local playback over Symphonia and CPAL with position tracking (M1), durable checkpoint persistence (M2), and finite HTTP media with capability probing and range-based seek (M3). Feed-driven podcasts (M4) and a full terminal UI (M5) are not implemented yet; today's interface is a status line plus a handful of keys (space to pause, the arrow keys to seek, `s`/`p` to stop/play, `q` to quit).

## Development

Install Rust through rustup. The repository pins Rust 1.98.1 and the rustfmt and clippy components. Dependency versions are recorded in the committed Cargo.lock.

Run from the repository root:

    cargo run --locked -- play <path-or-url>
    RUST_LOG=continuo=debug cargo run --locked -- play <path-or-url>
    cargo fmt --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked

Logging goes to stderr. Runtime code forbids unsafe code and denies unwrap/expect; tests may use unwrap/expect for assertions and fixtures.

M0 has no audio system dependency; every later milestone requires libasound2-dev on Linux for CPAL — the runtime libasound.so.2 alone is insufficient.

## Usage

    continuo play ~/Music/episode.mp3
    continuo play https://example.com/podcast/episode-42.mp3
    continuo play https://example.com/podcast/episode-42.mp3 --probe-only

`--probe-only` opens the source, prints what was found, and exits without touching an audio device or the terminal.

HTTP playback's honest limits:

- A range-capable server can seek and resume — including MP3 files with no
  seek index, such as most podcasts.
- A range-less server plays through from the start but cannot seek or resume.
- A live stream, or a source whose continuity cannot be established, is refused rather than played.
- There is no automatic reconnection: a dropped connection fails rather than retrying on its own. Playing again makes one explicit attempt to reopen at the preserved position.

### Seeking accuracy

Seeking on MP3 works by computing a byte offset instead of asking the
decoder to scan forward, which is what makes seeking on a long podcast fast
and responsive instead of stalling. A file that carries a proper seek index
(a Xing, Info, or VBRI header, which most encoders write) lands exactly, or
within a fraction of a second for variable-bitrate audio.

A file with **no such header** is different: the landing is a rough
estimate, and "approximate" here can mean landing in a substantially
different part of the recording, not just a few seconds off. Measured on a
worst case — a 600-second file with no seek index and genuinely variable
bitrate — a seek requested a third of the way through the recording landed
five seconds from the end: 235 seconds away from where it was asked to go.
Constant-bitrate files without an index are unaffected by this — the
estimate happens to be exact for them — but nothing in the file tells the
player which kind it is before the seek runs, so every landing on an
index-less MP3 is reported and should be read as an estimate, never as a
confirmed position.

## Design and roadmap

Read the [architecture](docs/architecture.md), the [M3 acceptance coverage map](docs/m3-acceptance.md), and the [approved foundation spec](docs/superpowers/specs/2026-09-07-continuo-foundation-design.md).

M1 added local playback, M2 durable resume, and M3 finite HTTP playback. M4 will add feeds and subscriptions, and M5 the TUI.

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
