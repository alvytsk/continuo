# Continuo M5: compact Ratatui player

Status: layout A selected by the user; this written specification is ready for review. The detailed behavior below is the proposed implementation contract, not an assertion that it already exists.

Branch: `feat/ratatui-player`.

## 1. Product decisions

Build a local-first terminal player using the selected compact layout A. The player, small cover, frequency spectrum, progress, and controls sit above the current playback queue. Browsing opens on demand. The queue is restored between launches; named playlists are outside this version.

The primary environment is Ghostty with Zellij or Herdr. The development machine reports Ghostty 1.3.1, Zellij 0.45.1, and Herdr 0.9.0. Cover graphics must be detected through the active terminal connection and must have a character-block fallback. Installed versions alone do not prove successful rendering.

A future Tauri 2 application should reuse the application behavior and audio engine. Server mode, remote clients, streaming to another machine, and cross-device session ownership are deferred. Do not introduce IPC, a network protocol, or a speculative remote implementation now.

The selected browser reference is [compact player](assets/continuo-compact-reference.html). It contains sample content and simulated playback, not production behavior. Its outer browser page, design-study headings, comparison controls, and window decoration are not part of the terminal application.

## 2. Existing foundations

The repository has local and finite HTTP playback, podcast subscriptions, stable media identities, cancellation, seek provenance, and durable checkpoints. `src/playback/engine.rs` owns the transport worker; `src/session.rs` owns checkpoint policy. These contracts remain in force.

`src/app.rs` currently mixes terminal handling, application coordination, lifecycle events, persistence setup, and status rendering. Extract the coordination needed by both the existing CLI player and the new TUI; avoid rewriting the transport engine.

The current `PersistedState` schema is 2 and contains no queue. `MediaMetadata` contains title and duration information, but no artist, album, or artwork. The feed parser and cache also have no artwork fields. Queue restoration, richer metadata, artwork, and spectrum analysis therefore require real application work beyond drawing widgets.

## 3. Ownership and module boundaries

Keep the existing crate initially. Use focused modules instead of splitting into a workspace merely to anticipate Tauri.

| Area | Responsibility | Boundary |
| --- | --- | --- |
| Application runtime | Own engine handle, session policy, HTTP service, persistence writer, queue, and current application state | Accept semantic commands; expose state and operation outcomes without terminal types |
| Queue | Ordered entries, stable entry IDs, active entry, mutations, successor decisions | Pure data and policy; no rendering, decoding, or filesystem I/O |
| Existing playback engine | Decode, seek, buffer, output, report actual playback and capabilities | Continue using the existing admission, revision, cancellation, and event contracts |
| Existing media/library services | Resolve paths, URLs, and cached podcast identities; read subscriptions and episodes | Reuse `library` operations without routing through printable CLI output |
| Artwork worker | Read and decode supported artwork with bounded resource use | Return image data independently of terminal graphics encoding |
| Spectrum worker | Analyze an optional bounded copy of output PCM | Publish the latest frequency-band frame independently of lifecycle events |
| TUI | Layout, key/mouse mapping, focus, selection, scrolling, terminal graphics, restoration | Translate input to application commands and render application state |

Candidate source locations are `src/application/`, `src/queue.rs`, `src/artwork/`, `src/playback/spectrum.rs`, and `src/tui/`. The implementation plan must pin the exact extraction boundaries after inspecting callers and tests. Keep compatibility re-exports where existing tests consume public helpers from `app`.

Application commands describe intent such as enqueue, play entry, pause, seek, reorder, remove, stop, and shutdown. No command accepts a Ratatui widget, terminal key event, Tauri handle, or serialized network message. UI selection and an optimistic seek target are presentation state; they must not become authoritative checkpoints.

## 4. Entry points and lifecycle

Add an explicit `continuo tui` entry point. Keep existing `continuo play`, feed commands, and `--probe-only` behavior compatible. A bare `continuo` can retain its current CLI help behavior for this milestone.

Opening the TUI restores the queue, active entry, volume, and checkpoints without automatically playing. An empty queue opens an idle interface. The audio device can be created lazily on the first play action; the interface must be usable when no output device is available.

`q` or Ctrl-C exits the TUI, gracefully stops the local engine, captures and flushes state, and restores the terminal. A suspended or detached multiplexer session can keep the process alive, but Continuo itself does not become a daemon.

## 5. Queue behavior

Each queue entry stores a stable `QueueEntryId`, the existing `MediaId`, a resolvable source reference, and display metadata. IDs identify occurrences: duplicate entries for the same media are allowed. Checkpoints remain keyed by `MediaId`, so duplicates share listening history while retaining independent queue positions.

For a local file, preserve the normalized absolute path; for a direct URL, preserve the normalized source URL. A podcast entry retains its episode identity and the resolved enclosure source, rather than a mutable display index. Refreshing a feed must not silently substitute another episode for a queued entry.

Proposed rules:

- Enqueue appends without changing playback. Explicitly playing an entry uses existing resume/replay policy.
- Selection can move independently of the playing entry. Reordering preserves both IDs and does not reload audio.
- A revision-matched, verified `EndOfTrack` advances once to the next queue entry. A stale completion event cannot advance the new track.
- The last entry ends playback; there is no wrapping, shuffle, or repeat in this milestone.
- Playback failure leaves the queue and checkpoint intact, reports the error, and waits for an explicit retry or another selection. It does not silently skip entries.
- Removing a nonplaying entry leaves playback alone. Removing the active entry captures its checkpoint, stops it, removes it, clears the active-entry reference, and selects its successor (or predecessor at the end), without starting that selection automatically. Selection alone never establishes a new active entry.
- Clearing the queue stops playback and removes queue entries while retaining listening history. Require confirmation in the UI because this discards the queue.
- Explicit previous/next actions move to adjacent entries without wrapping and use the same playback preparation and checkpoint path as playing a selected entry.

The browser sketch is illustrative: its wrapping skip buttons and automatic replacement after active-entry removal do not define production semantics.

## 6. Durable state

Extend the atomic playback snapshot to schema 3 with queue entries and the active queue-entry reference. Keep volume, current media, and checkpoints in the same snapshot. UI scroll position and selection need not be persisted.

Migrate schemas 1 and 2 by retaining their existing state and introducing an empty queue with no active queue entry. Preserve an existing `current_media` checkpoint reference even when it has no queue entry. For a populated queue with an active entry, the active entry and current media must agree. Validate entry IDs for uniqueness, source/identity consistency, and active-entry references before accepting a snapshot.

The legacy `continuo play` command plays outside the queue: retain queue entries, clear the active queue-entry reference when adopting that explicit source, and persist its current media and progress normally. Restoring the TUI must not infer a queue occurrence from `MediaId`, because duplicate occurrences are valid.

Persist queue mutations through the existing single writer. Preserve checkpoint capture order, established-versus-estimated provenance, protected resume points, malformed/unsupported file handling, and graceful flush reporting. A queue mutation must not bypass the writer with an independent file write.

Prevent concurrent playback applications from writing the same state profile by acquiring an exclusive process-lifetime lock before opening the writer. Read-only feed listing and probe operations do not acquire that playback-writer lock. Release it automatically when the owning process exits. This is local persistence protection, not a server/session feature.

## 7. Terminal layout and controls

Use Ratatui with the existing Crossterm input stack. Use nested layouts, bordered blocks, a stateful queue table, and a character-based progress bar. Preserve the reference palette: dark background, muted green highlights, cream foreground, and subtle borders. The user's terminal controls the font.

Normal layout, at least 80 columns by 28 rows:

1. One application/status row.
2. A compact player region with a small square-in-pixels cover on the left and track information, spectrum, transport, and progress on the right.
3. The queue fills the remaining space, with a separate playing marker and selected-row highlight.
4. A short keyboard/status footer.

At 50–79 columns or 18–27 rows, reduce artwork size and spectrum height, remove secondary metadata, and use single-line queue entries when necessary. Below 50 columns or 18 rows, hide artwork and spectrum; retain title, playback state, usable queue rows, and essential controls. Below 30 columns or 8 rows, show a resize message while preserving quit and playback controls. Drawing must remain valid even at zero-sized terminal rectangles.

| Input | Action |
| --- | --- |
| Space | Pause/resume |
| Enter | Play selected queue entry |
| Up/Down or j/k | Move selection |
| J/K | Move selected entry down/up |
| Left/Right | Seek backward/forward 10 seconds using existing burst handling |
| Home | Explicit restart from beginning |
| -/+ | Adjust volume |
| s / p | Stop / play |
| [ / ] | Previous / next queue entry |
| d | Remove selected entry |
| b | Open/close browser |
| a | Open path/URL input |
| c | Request queue clear with confirmation |
| ? | Show help |
| Esc | Close the active overlay or cancel input |
| q / Ctrl-C | Graceful quit |

Text entry consumes printable keys so typing a URL cannot trigger player shortcuts. Mouse support covers queue selection, activation, scrolling, and transport hit regions. Seeking with the mouse follows the same capability checks as keyboard seeking. No operation depends on mouse availability.

Duration, seekability, estimated position, buffering, and playback failure must remain honestly represented. Use an unknown-duration display instead of an invented denominator; do not present a feed's declared duration as decoder-confirmed.

## 8. Browsing and metadata

The on-demand browser offers local directories and cached subscriptions/episodes. Directory reads and media metadata extraction occur off the render loop; do not introduce recursive whole-library indexing. A user can enqueue selected files or episodes, or enter a path/HTTP URL directly. The browser operates independently of the playing item.

Preserve manual feed refresh: opening the browser or listing episodes never refreshes a feed. Existing subscribe, unsubscribe, and refresh commands remain available through the CLI; full subscription management screens are outside this milestone.

Use available title, artist, and album tags for local media. Use a filename or existing sanitized display-name fallback when metadata is absent. Queued entries may initially lack duration; background metadata can fill it without opening an audio device. Display strings must not emit raw terminal control characters or expose URL credentials.

## 9. Artwork

For this first implementation, resolve local embedded front-cover artwork, then sibling `cover.jpg`, `cover.png`, `folder.jpg`, or `folder.png` in that order. Support JPEG and PNG inputs. Limit encoded input to 10 MiB and decoded dimensions to 16 million pixels; failure produces a placeholder and never prevents playback.

Podcast feed-art parsing and remote artwork downloads are outside the first artwork increment; those entries can show a stable placeholder. This honors the requested best-effort small cover without expanding feed schemas in the TUI milestone.

Read and decode on a worker. Resize and encode the terminal representation outside rendering. Cache the prepared representation until the media, artwork mode, or layout size changes. Use `ratatui-image` for the active connection's supported protocol, fall back to colored half-blocks, and provide `auto`, `blocks`, and `off` settings. A detection timeout must fall back promptly. Clean up image placements on replacement, resize, and terminal exit.

## 10. Frequency spectrum

The real visualizer uses audio samples, not the browser's simulated animation. Analyze PCM associated with output consumption rather than decoding far ahead of playback.

An optional tap copies complete sample frames into a bounded, preallocated ring. The output callback remains allocation-free, lock-free, and I/O-free; it does not compute FFTs, wait for analysis, or publish terminal updates. If the tap has no room, drop visualization data and reset the worker's analysis window across the discontinuity. Playback and existing position-span publication take precedence.

On a worker, use a 2048-sample Hann window and 24 logarithmic frequency bands between 40 Hz and the lesser of 16 kHz or Nyquist. Combine stereo channels by spectral power so opposite-phase material does not vanish through a mono sum. Publish at most 20 frames per second, with bounded smoothing and a latest-value result. Associate frames with the playback revision and output timing; do not show another track's spectrum after a seek or load.

The display represents signal level after player gain. Paused, stopped, or starved playback decays to silence. Hidden or disabled visualization suspends analysis. Tauri can later render the same band data without depending on the terminal widget.

## 11. Failure and shutdown behavior

Application state changes stay ordered around the existing engine events. Drain lifecycle events before sampling progress, and reject stale revisions. Track changes capture outgoing checkpoints before adopting incoming identity. UI predictions never overwrite authoritative position.

Media opening, browsing, artwork, or device failures keep the TUI usable and expose a concise status message. Artwork failure does not become playback failure. Persistence failure remains visible; the interface must not claim state was saved when flushing failed. Send diagnostic logs to a file or another sink that cannot corrupt the alternate screen.

Terminal setup uses a guard that restores raw mode, mouse capture, cursor visibility, image placement, and the alternate screen on normal exit and unwind. Restore the terminal before printing a final fatal error. No terminal cleanup path skips the engine/writer shutdown sequence.

## 12. Validation and delivery order

Implement in reviewable increments: extract the shared runtime and add queue/state semantics; build the compact Ratatui interface and browser; add best-effort artwork; then add the actual spectrum. Each increment must leave existing CLI functionality working. This is one player milestone; a later server or GUI gets its own design.

Required evidence:

- Existing playback, cancellation, provenance, HTTP, feed, and persistence suites remain green.
- Queue tests cover duplicate media entries, reorder without reload, active removal, end-of-queue, failure behavior, and stale/double completion events.
- State tests cover v1/v2 migration, queue restoration, malformed active references, duplicate entry IDs, single-writer ownership, and persistence failure.
- Ratatui buffer tests cover normal, compact, tiny, empty, loading, failed, unknown-duration, and estimated-position screens; selection remains distinct from playback.
- Artwork fixtures cover supported images, fallback ordering, missing/corrupt/oversized input, and terminal resize cleanup.
- Spectrum tests use known tones, silence, opposite-phase stereo, dropped samples, and stale revisions; a saturated visualization channel cannot block playback.
- Manually exercise the result in Ghostty directly, Ghostty with Zellij, and Ghostty with Herdr: images, fallback, resize, pane switching, keyboard, mouse, and exit restoration. Browser checks do not satisfy these terminal checks.
- Run `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, and `cargo test --locked` for implementation acceptance.

## 13. Changes to earlier scope

The foundation scope excluded visualizers. This conversation explicitly adds a frequency spectrum to M5. Equalizers, audio effects, themes/skins, named playlists, third-party media services, daemon mode, remote streaming, and Tauri implementation remain outside this version.

## References

- [Ratatui layouts](https://ratatui.rs/concepts/layout/)
- [Ratatui widgets](https://docs.rs/ratatui/latest/ratatui/widgets/index.html)
- [ratatui-image](https://github.com/ratatui/ratatui-image)
- [Ghostty terminal features](https://ghostty.org/docs/features)
- [Zellij 0.45 image support](https://zellij.dev/news/nested-sessions-kitty-graphics-new-ui/)
- [Herdr 0.9 release](https://github.com/herdrdev/herdr/releases/tag/v0.9.0)
