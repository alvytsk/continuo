# Continuo M5: compact Ratatui player

Status: layout A selected by the user; revised after the second technical review and ready for review. The detailed behavior below is the proposed implementation contract, not an assertion that it already exists.

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
| Application runtime | Own engine handle, `Session`, HTTP service, persistence writer, and orchestration | Route mutations through `Session`; expose state and operation outcomes without terminal types |
| Session | Sole owner of mutable `PersistedState`, including queue, active reference, volume, and checkpoints; sole builder of persistence snapshots | Serialize queue commands, metadata/source updates, playback events, and checkpoint ticks into one authoritative state |
| Queue | Ordered entries, stable entry IDs, active entry, mutations, successor decisions | Pure data and policy stored inside Session's state; no separate mutable runtime copy, rendering, decoding, or filesystem I/O |
| Existing playback engine | Decode, seek, buffer, output, report actual playback and capabilities | Preserve admission, revision, and cancellation semantics; add caller-supplied load correlation and protected load outcomes as specified in §6 |
| Existing media/library services | Resolve paths, URLs, and cached podcast identities; read subscriptions and episodes | Reuse `library` operations without routing through printable CLI output |
| Artwork worker | Read and decode supported artwork with bounded resource use | Return image data independently of terminal graphics encoding |
| Spectrum worker | Analyze an optional bounded copy of output PCM | Publish the latest frequency-band frame independently of lifecycle events |
| TUI | Layout, key/mouse mapping, focus, selection, scrolling, terminal graphics, restoration | Translate input to application commands and render application state |

Candidate source locations are `src/application/`, `src/queue.rs`, `src/artwork/`, `src/playback/spectrum.rs`, and `src/tui/`. The implementation plan must pin the exact extraction boundaries after inspecting callers and tests. Keep compatibility re-exports where existing tests consume public helpers from `app`.

Application commands describe intent such as enqueue, play entry, pause, seek, reorder, remove, stop, and shutdown. No command accepts a Ratatui widget, terminal key event, Tauri handle, or serialized network message. UI selection and an optimistic seek target are presentation state; they must not become authoritative checkpoints.

The application may keep immutable view snapshots. Session also owns a bounded, transient table of pending load tokens and their exact queue-occurrence targets (§6); this table is not persisted. There is no second writable queue. All persistent changes go through `Session` on the application thread; only `Session` produces `Action::Submit` snapshots. Thus a periodic checkpoint includes the latest queue, and a queue mutation includes the latest accepted checkpoint. Background workers send results to the application rather than constructing snapshots themselves.

## 4. Entry points and lifecycle

Add an explicit `continuo tui` entry point. Keep existing `continuo play`, feed commands, and `--probe-only` behavior compatible except for the explicitly shared playback locking and signal changes below. A bare `continuo` retains the required-subcommand usage error and exit code 2; `continuo --help` remains the successful help entry point.

Opening the TUI restores the queue, active entry, volume, and checkpoints without automatically playing. An empty queue opens an idle interface. The audio device can be created lazily on the first play action; the interface must be usable when no output device is available.

Before loading, show the restored active entry's title and saved resume candidate labeled `saved`, including `~` for an estimated candidate, `played` for completion, and `position unknown` when appropriate. This is saved history, not live engine progress or proof that the source is still available. With no active entry, show an idle player; a selected queue row can show its own saved history without becoming active.

Pre-load and end-of-queue input has explicit semantics:

| Situation | Space or p | Enter | Home and Left/Right |
| --- | --- | --- | --- |
| Restored active entry, no loaded media | Load the active entry with resume/replay policy, regardless of selection | Load the selected entry | No-op with `Play a track before seeking`; do not open media or change a checkpoint |
| No active entry, nonempty queue | Load the selected entry; default selection is the first row | Load the selected entry | Same unloaded no-op |
| Empty queue | No-op with `Queue is empty` | Same | Same; no seek command sent |
| Last entry ended | Replay the ended active entry from the beginning under completed-entry policy | Play selected entry with resume/replay policy | Home explicitly restarts the active entry; Left/Right no-op with `Track ended; press play to replay` |

While loading, Home and Left/Right do not accumulate a future seek; report `Still loading`. After a loaded track is stopped or paused, retain existing engine restart/seek semantics. While playing, Space toggles pause; `p` is idempotent play/resume. A failed load is retried with Space/p using the last requested entry when still queued; Enter always chooses the selected row. Previous/next before loading use the restored active entry, or selection if there is none, as their navigation anchor; a missing neighbor is a no-op.

`q`, the Ctrl-C key, and OS shutdown requests converge on one idempotent application shutdown path shared by **both `tui` and legacy `play`, including `play` without a terminal**. On Unix, register SIGINT, SIGHUP, and SIGTERM through the safe `signal-hook` iterator API before state loading, terminal setup, or spawning playback workers. A dedicated listener records a shutdown request and wakes the application; it never renders, loads state, or flushes inside a raw signal handler. Installation failure aborts startup before entering the alternate screen. This does not depend on Tokio's currently disabled signal feature. Gate Unix signal code by platform; feed commands and `--probe-only` do not acquire this playback lifecycle.

On a request, cancel source preparation/reads through the existing engine interrupt path, drain terminal lifecycle outcomes, capture the final available checkpoint, flush and join the writer, and attempt terminal restoration even if the pane's PTY has gone away. Keep the profile lock through the final writer shutdown. Close and join the signal listener during teardown; repeated signals do not initiate concurrent flushes. Exit normally for `q`/key Ctrl-C; both playback commands exit with `128 + signal_number` after OS-signal shutdown, using the first recorded shutdown signal. SIGKILL, process abort, and power loss cannot run this path and retain only the last durable write. A suspended or detached multiplexer session can keep the process alive, but Continuo itself does not become a daemon.

## 5. Queue behavior

Each queue entry stores a stable `QueueEntryId`, the existing `MediaId`, a resolvable source reference, and display metadata. IDs identify occurrences: duplicate entries for the same media are allowed. Checkpoints remain keyed by `MediaId`, so duplicates share listening history while retaining independent queue positions.

For a local file, preserve the normalized absolute path; for a direct URL, preserve the normalized source URL. A podcast entry retains its `(FeedId, EpisodeKey)` identity and the last resolved enclosure as a fallback, rather than a mutable display index.

Before each podcast load, read the current local cache for that feed and match the exact episode identity. If present with a usable enclosure, use that URL and update the fallback through `Session`; rotated or signed URLs do not change `MediaId` or checkpoints. If the episode or subscription is absent, or the cache file is missing, use the saved URL and show `Using saved episode source`. If the episode is present but has no usable enclosure, report `NotPlayable` instead of reviving a removed enclosure. Corrupt, unreadable, or unsupported cache data is an explicit resolution error, not evidence that the episode is absent. Never match by title, list index, or publication date; never fetch or refresh a feed implicitly. An expired fallback fails through normal playback error handling.

Proposed rules:

- Enqueue appends without changing playback. Every entry load uses existing resume/replay policy, including explicit play, previous/next, and automatic advancement: resume half-listened media; replay completed media from zero; preserve the established/estimated and unavailable-resume protection rules.
- Selection can move independently of the playing entry. Reordering preserves both IDs and does not reload audio.
- An engine `EndOfTrack` for the current playback revision advances once to the next queue entry, after `Session` observes completion and captures the outgoing state. The engine emits it after output drains; the application does not infer completion from a duration, byte count, or timer. Both `Established` and `Estimated` provenance advance. Estimated completion still cannot overwrite an established checkpoint position. Deduplicate completion by the adopted playback revision; a stale completion event cannot advance the new track.
- The last entry ends playback; there is no wrapping, shuffle, or repeat in this milestone.
- Playback failure leaves the queue and checkpoint intact, reports the error, and waits for an explicit retry or another selection. It does not silently skip entries.
- Removing a nonplaying entry leaves playback alone. Removing the active entry captures its checkpoint, stops it, removes it, clears the active-entry reference, and selects its successor (or predecessor at the end), without starting that selection automatically. Selection alone never establishes a new active entry.
- Clearing the queue stops playback and removes queue entries while retaining listening history. Require confirmation in the UI because this discards the queue.
- Explicit previous/next actions move to adjacent entries without wrapping and use the same playback preparation and checkpoint path as playing a selected entry.

The browser sketch is illustrative: its wrapping skip buttons and automatic replacement after active-entry removal do not define production semantics.

This build supports 256 queue occurrences, including duplicates. Reject an enqueue operation that would exceed the cap in full, with a visible capacity message; do not silently truncate a batch. The cap is an operational limit, **not a schema-validity rule**. A larger on-disk queue, including one written by a later schema-3 build with a higher cap, uses the queue-only recovery policy in §6; it never causes whole-file quarantine. Raising this operational limit alone does not require a schema bump. The checkpoint cap stays at 512 media identities with the existing eviction policy: queue membership does not pin a checkpoint. A queued track's history can therefore be evicted after enough other media are played, including through legacy `play`. The entry remains queued and a later load with no checkpoint starts from zero. Queue capacity bounds snapshot work; it does not promise indefinite history retention.

## 6. Durable state

Extend the atomic playback snapshot to schema 3 with queue entries and the active queue-entry reference. Keep volume, current media, and checkpoints in the same snapshot. UI scroll position and selection need not be persisted.

Migrate schemas 1 and 2 by retaining their existing state and introducing an empty queue with no active queue entry. Preserve an existing `current_media` checkpoint reference even when it has no queue entry. For a populated queue with an active entry, the active entry and current media must agree.

Decode the supported version envelope and existing listening-history fields independently of the queue extension. Queue/active fields must be decoded separately from their raw JSON values so even a wrong queue field type or malformed entry cannot fail deserialization of otherwise valid checkpoints, volume, `current_media`, or checkpoint bookkeeping. Apply these recovery rules before constructing Session:

| Problem confined to the queue extension | Recovered in-memory state |
| --- | --- |
| Malformed, dangling, or media-mismatched active reference with otherwise usable entries | Keep all entries; clear only the active reference |
| Malformed queue/entry, duplicate entry IDs, or source/identity mismatch | Clear the whole queue and active reference; do not guess which occurrences are valid |
| Well-formed queue exceeds this build's operational capacity | Clear the whole queue and active reference; report the unsupported queue size |

In every row, retain **all valid checkpoints, volume, `current_media`, and existing checkpoint bookkeeping**. Never infer an active occurrence from a matching `MediaId`. A missing/null active reference is normal when no entry is active and needs no recovery warning; an absent schema-3 queue is recovered as an empty queue. Before enabling any writer after a queue repair, copy the exact original file bytes to a unique sibling recovery file using create-new semantics, sync it with the existing durability conventions, and show a sanitized warning with the recovery path and the fields reset. Do not rename the original out of place or overwrite an earlier recovery copy. If copying/syncing fails, keep the recovered state usable in memory but disable persistence for this session and warn; leave the original untouched. After a successful backup, the normal single writer may persist the repaired state. Runtime mutations reject invariant violations before changing Session; recovery is for loading existing files.

Whole-file malformed-state handling remains for invalid JSON or invalid listening-history/envelope fields that cannot be decoded independently. Unsupported schema versions and unreadable files retain the existing preservation/disabled-writer behavior. Read-only snapshot readers may decode the usable listening-history portion without accepting the queue; they do not repair, copy, quarantine, or write files. Queue repair and its backup occur only under the playback profile lock.

The legacy `continuo play` command plays outside the queue: retain queue entries, clear the active queue-entry reference when adopting that explicit source, and persist its current media and progress normally. Restoring the TUI must not infer a queue occurrence from `MediaId`, because duplicate occurrences are valid.

Persist queue mutations through Session's authoritative state and the existing single writer. The writer accepts immutable `Action::Submit` snapshots in application order and may coalesce them; it never merges independently built queue and checkpoint copies. Preserve checkpoint capture order, established-versus-estimated provenance, protected resume points, file handling with the queue-only exception above, and graceful flush reporting. A queue mutation must not bypass Session or the writer with an independent file write.

**Correlate loads with caller-supplied request tokens.** Add a required `LoadRequestId` to `PlaybackCommand::Load`, echoed unchanged in `Loaded`, load-related `Failed`, and `StateChanged { state: Loading, .. }`. Use an optional request field on general state/failure events, since some are not associated with a load. Session allocates process-lifetime unique tokens and registers `(token, QueueEntryId or legacy-play target, MediaId)` before submission; remove the registration if admission is `Busy` or `Gone`. Allow at most 16 pending loads in the application and report busy when full, independently of the engine's admission limit. Every load outcome removes its pending registration; a successful adoption retains its token separately as the adopted playback identity. Never derive tokens from `MediaId`, `session_rev`, or queue index, and never reuse a token within an engine lifetime. `Admission` continues to describe command admission only.

Session adopts a queue occurrence **only when it observes `Loaded` for that registered token**, checking that the entry still exists and matches the event's media. Capture outgoing state first, then adopt active ID and `current_media` atomically, with the event's revision and resume disposition. Multiple accepted requests can be pending: Enter on rows 3 and 7 containing the same media creates distinct tokens; their `Loaded` events adopt row 3 then row 7 in engine event order, even if both were submitted before the first event arrived. Submitting the newer request does not itself adopt row 7 or invalidate row 3's real load outcome. Reordering while loading cannot change the target. Failure/cancellation before `Loaded` retires that pending request without adopting its media or overwriting the previous checkpoint; progress for an unadopted load cannot update that previous checkpoint. A removed pending target is marked invalid; its late outcome cannot resurrect the entry, and any resulting playback is stopped. Legacy play uses the same correlation but adopts no queue occurrence. All application retries that need identity adoption issue a fresh tokenized `Load`.

`Loaded` is now a **protected lifecycle outcome**: it cannot be dropped or displaced as an ordinary event. Keep it ordered before subsequent state/progress adoption for that load, retain it in the shutdown report if not yet delivered, and account for it in event reserve/backlog arithmetic and admission backpressure. Add a tokenized protected `LoadCancelled` outcome for an accepted load interrupted before `Loaded`/`Failed`, including accepted commands discarded during shutdown, so pending registrations can always retire. Each accepted load has exactly one load outcome (`Loaded`, load failure, or `LoadCancelled`); a later playback failure is distinct from its already successful load. This is a narrow engine-contract extension, with saturation tests required rather than assuming the current reserve is sufficient. Ordinary `Loading` notifications may still coalesce/drop; adoption never depends on receiving one or on matching events by order alone.

`session_rev` remains worker-owned. Device recovery changes the revision of the adopted playback without selecting another queue occurrence or consuming a pending token. Maintain the adopted load identity across recovery; lifecycle events are drained before progress, and neither a revision increment nor a newer progress snapshot may substitute for `Loaded` adoption. Unknown, duplicate, invalidated, or stale token outcomes cannot select an entry or mutate its history.

Prevent concurrent playback applications from using stale state by acquiring an exclusive process-lifetime lock **before any state read, load, migration, quarantine, or writer creation**. Resolve the profile path, open a stable sibling `state.lock`, acquire its nonblocking advisory lock, and only then call `StateStore::load`. Do not lock the atomically replaced `state.json` inode, and do not unlink/recreate the lock file on unlock. Retain the same guard through initialization, final snapshot capture, writer flush/join, and teardown. A later invocation must load afresh after obtaining the lock; it cannot reuse a snapshot from before acquisition.

For legacy `play`, resolve the positional path/URL or cached podcast source **before acquiring the playback state lock**, as the current CLI does. This is source validation/identity resolution, not decoder/device opening or reading the playback snapshot. Missing-file, directory, and invalid-source errors therefore take precedence over profile contention. After successful resolution, install the shared lifecycle handling and acquire the lock before loading resume state. `tui` has no initial source to resolve and acquires the lock before restoring its queue; later user-selected source resolution runs within that owned session. The complete TUI setup order is specified in §11.

If the lock is held, both a second `tui` and a second `play` refuse to start, exit nonzero, and report `Another Continuo player is using this state profile` before opening an audio device or entering raw mode. Do not use the `writable = false` path for contention. Failure to resolve/create/lock the state profile is also a startup error for these playback commands. This intentionally replaces the existing no-state-directory unsaved fallback. After successful lock acquisition, the existing read-only/disabled-writer handling for unsupported or unreadable state remains, with a visible unsaved-session warning. Read-only feed listing and probe operations do not acquire this lock; their snapshots are never promoted into a playback writer. These locks coordinate updated Continuo processes; older binaries that ignore the lock remain outside the guarantee.

## 7. Terminal layout and controls

Use Ratatui with the existing Crossterm input stack. Use nested layouts, bordered blocks, a stateful queue table, and a character-based progress bar. Preserve the reference palette: dark background, muted green highlights, cream foreground, and subtle borders. The user's terminal controls the font.

Normal layout, at least 80 columns by 28 rows:

1. One application/status row.
2. A compact player region with a small square-in-pixels cover on the left and track information, spectrum, transport, and progress on the right.
3. The queue fills the remaining space, with a separate playing marker and selected-row highlight.
4. A short keyboard/status footer.

The smallest tier matched by either dimension wins. Evaluate in this order: below 30 columns **or** 8 rows shows the resize message; otherwise below 50 columns **or** 18 rows uses the minimal tier; otherwise below 80 columns **or** 28 rows uses the compact tier; otherwise use normal. Thus 100×20 is compact. Compact reduces artwork and spectrum height, removes secondary metadata, and uses single-line queue entries when needed. Minimal hides artwork and spectrum while retaining title, playback state, queue rows, and essential controls. The resize-message tier preserves quit and playback controls. Drawing remains valid even at zero-sized terminal rectangles.

| Input | Action |
| --- | --- |
| Space | Pause/resume; unloaded/ended behavior follows §4 |
| Enter | Play selected queue entry |
| Up/Down or j/k | Move selection |
| J/K | Move selected entry down/up |
| Left/Right | Seek backward/forward 10 seconds using existing burst handling |
| Home | Explicit restart from beginning |
| - or _ / + or = | Decrease / increase volume; keep the existing aliases |
| s / p | Stop / play |
| [ / ] | Previous / next queue entry |
| d | Remove selected entry |
| b | Open/close browser |
| a | Open path/URL input |
| c | Request queue clear with confirmation |
| ? | Show help |
| m | Toggle mouse capture; status/footer shows on or off |
| Ctrl-L | Clear Ratatui's screen buffer, invalidate image placements, and redraw the whole view |
| Esc | Close the active overlay or cancel input |
| q / Ctrl-C | Graceful quit |

Text entry consumes printable keys so typing a URL cannot trigger player shortcuts. Mouse support covers queue selection, activation, scrolling, and transport hit regions. Seeking with the mouse follows the same capability checks as keyboard seeking. Mouse capture defaults on, can be toggled with `m` outside text entry, and can be disabled at launch with `--mouse off`. Turning it off sends the protocol's disable sequences so terminal/multiplexer text selection works normally; it does not interrupt playback. No operation depends on mouse availability.

Duration, seekability, estimated position, buffering, and playback failure must remain honestly represented. Use an unknown-duration display instead of an invented denominator; do not present a feed's declared duration as decoder-confirmed.

## 8. Browsing and metadata

The on-demand browser offers local directories and cached subscriptions/episodes. Directory reads and media metadata extraction occur off the render loop; do not introduce recursive whole-library indexing. A user can enqueue selected files or episodes, or enter a path/HTTP URL directly. The browser operates independently of the playing item.

Preserve manual feed refresh: opening the browser or listing episodes never refreshes a feed. Existing subscribe, unsubscribe, and refresh commands remain available through the CLI; full subscription management screens are outside this milestone.

*Superseded for subscribe, refresh and unsubscribe by the M6 design (`2026-09-16-continuo-m6-feed-management-design.md`).*

Use available title, artist, and album tags for local media. Use a filename or existing sanitized display-name fallback when metadata is absent. **Background metadata enrichment is local-file-only**, with at most two cancellable workers and no audio device. Enqueueing, restoring, or browsing a URL/podcast entry performs no metadata network requests and no remote duration probes. Use already-cached feed metadata with declared-duration provenance preserved; otherwise show unknown fields. An explicit playback load may fill remote metadata as a byproduct of the existing bounded HTTP preparation/decode path. Persist enrichment results only through `Session`. Display strings must not emit raw terminal control characters or expose URL credentials.

## 9. Artwork

For this first implementation, resolve local embedded front-cover artwork, then sibling `cover.jpg`, `cover.png`, `folder.jpg`, or `folder.png` in that order. Support JPEG and PNG inputs. Limit encoded input to 10 MiB and decoded dimensions to 16 million pixels; failure produces a placeholder and never prevents playback.

Podcast feed-art parsing and remote artwork downloads are outside the first artwork increment; those entries can show a stable placeholder. This honors the requested best-effort small cover without expanding feed schemas in the TUI milestone.

Read and decode on a worker. Resize and encode the terminal representation outside rendering. Cache the prepared representation until the media, artwork mode, or layout size changes. Use `ratatui-image` for the active connection's supported protocol, fall back to colored half-blocks, and provide `auto`, `blocks`, and `off` settings. A detection timeout must fall back promptly. Clean up image placements on replacement, resize, and terminal exit.

Artwork decoding/encoding and background metadata probing are isolated jobs with the contained-panic boundary in §11. An unwinding decoder panic becomes a failed job and placeholder/unknown metadata; discard that job's decoder state and keep playback and the TUI running. Do not apply this containment flag to the audio engine or to workers' shared-state orchestration.

## 10. Frequency spectrum

The real visualizer uses audio samples, not the browser's simulated animation. Analyze PCM associated with output consumption rather than decoding far ahead of playback.

An optional tap copies post-gain output PCM into a bounded, preallocated ring. The output callback remains allocation-free, lock-free, and I/O-free; it does not compute FFTs, wait for analysis, or publish terminal updates. One audio frame contains every output channel. Reserve a chunk with `rtrb::Producer::write_chunk` for an integral number of complete frames, fill it, and commit only a multiple of the channel count. A short reservation is never completed by individual `push` calls. The implementation must pair sample chunks and their timing/generation descriptors atomically from the consumer's perspective: reserve capacity for both first, commit PCM first, then publish its descriptor, and consume only described PCM. If either capacity is unavailable, drop the complete tap block. Attach a discontinuity sequence to the next accepted descriptor and reset the worker's FFT window when it changes. No orphan PCM, partial channel frame, or unmatched descriptor may be exposed. Playback and existing position-span publication take precedence.

The callback labels tap descriptors with its output generation, the control epoch, an immutable transport-instance ID assigned when that tap is constructed, channel count, sample rate, discontinuity sequence, and predicted output timestamp. It does **not** know or fabricate `session_rev`. Before enabling a transport, the playback worker publishes the mapping from `(transport-instance ID, generation, control epoch)` to `session_rev` and its output clock domain. The spectrum worker uses that mapping to label results; unknown or retired mappings are discarded. A unique transport-instance ID prevents a wrapped generation or recreated device from matching old samples. Reset accumulation on mapping/format changes and discontinuities; invalidate old results on load, seek, and transport retirement, even when a seek preserves the logical media identity.

Keep the **2048-sample Hann window and merge narrow bands**. Start with 24 nominal logarithmic intervals from 40 Hz to the lesser of 16 kHz or Nyquist. Assign positive-frequency FFT bin centers to half-open intervals, with the last upper edge inclusive. Walk intervals from low to high, merging adjacent intervals until a band contains at least two bin centers. Merge any remaining underfilled tail into the preceding band; if the entire range contains fewer than two centers, publish unavailable/silence rather than an empty-band display. The result has at most 24 bands, with explicit lower/upper edges in each frame. Average bin power within each merged band and average spectral power across **all output channels**, so opposite-phase channels do not cancel. Do not stretch the result into 24 duplicate low-frequency bars. Fewer, wider low bands are intentional; a 2048-sample transform cannot claim finer frequency resolution.

At 44.1/48 kHz the FFT spacing is approximately 21.5/23.4 Hz; the nominal first interval is only about 11.3 Hz wide. Band merging is therefore required even at these ordinary sample rates, and must be computed from the actual device sample rate. Publish at most 20 frames per second, with bounded smoothing and a latest-value result. Schedule frames against their mapped output clock/timestamps; never display another transport's spectrum under the current track.

A numerical check of this interval-merging rule gives the following expected geometry. These are specification checks, not a benchmark of an implemented visualizer.

| Output sample rate | Emitted bands | First merged interval |
| --- | --- | --- |
| 44.1 kHz | 21 | 40–65.9 Hz |
| 48 kHz | 21 | 40–84.6 Hz |
| 96 kHz | 19 | 40–108.6 Hz |
| 192 kHz | 16 | 40–229.6 Hz |

Each band contains at least two bin centers; all eligible centers are assigned exactly once. The much wider low band at 192 kHz explicitly reflects the short window's limited resolution.

The display represents signal level after player gain. Paused, stopped, or starved playback decays to silence. Hidden or disabled visualization suspends analysis. Tauri can later render the same band data without depending on the terminal widget.

## 11. Failure and shutdown behavior

Application state changes stay ordered around the existing engine events. Drain lifecycle events before sampling progress, and reject stale revisions. Track changes capture outgoing checkpoints before adopting incoming identity. UI predictions never overwrite authoritative position.

After successful startup, media opening, browsing, artwork, or device failures keep the TUI usable and expose a concise status message. Artwork failure does not become playback failure. Persistence failure remains visible; the interface must not claim state was saved when flushing failed.

**Redirect process fd 2 for the TUI lifetime**, including messages emitted directly by ALSA or other C libraries. Use the safe `gag::Redirect::stderr` API with a per-run append log under the profile's `logs/` directory; keep `unsafe_code = "forbid"` in Continuo. Install the panic hook and its initially empty restoration slot **before redirecting fd 2**. Set up the log and redirection before entering raw/alternate-screen mode or constructing the audio device; failure aborts startup visibly. All worker logs, including `tracing`, must remain off the live terminal. Plain CLI commands, including legacy `play`, retain their current stderr output. Hold the redirect through engine/device destruction and writer shutdown; then restore the terminal and original fd 2 before printing any final error. This is fd redirection, not merely changing the Rust tracing subscriber. At startup under the profile lock, retain the five most recent prior logs and create a new uniquely named log. The active file has no size cap in this milestone; do not claim that ordinary tracing rotation also bounds direct C-library writes.

TUI startup order is: create cleanup state and install the panic hook; install the signal listener; resolve/create the profile and acquire its lock; load/recover state; open the log and install fd-2 redirection; start the persistence writer and application workers; enter terminal mode and render. Audio creation remains lazy. Until terminal setup succeeds, cleanup uses only resources already initialized. Check recorded shutdown requests between stages and take the same teardown path rather than completing startup after a signal. Legacy `play` resolves its source first (§6), then installs signal handling, locks and loads state, starts its writer/engine, and enters its existing terminal loop when available. It receives the same signal flush and exit-code contract without the TUI log redirection.

Terminal setup uses idempotent cleanup for raw mode, mouse capture, cursor visibility, image placement, and the alternate screen. A pane closing may make these writes fail; continue engine shutdown and persistence flush anyway. On normal exit and SIGHUP/SIGTERM/SIGINT, use the §4 shutdown path before releasing persistence ownership. Ctrl-L and resize clear Ratatui's cached buffer and reprepare image placements; this is recovery support, not the chosen solution for C-library stderr.

The TUI panic hook retains the previous hook and first checks a thread-local contained-job flag (default false, accessed without panicking). Artwork and metadata workers set that flag with a scoped guard only inside a `catch_unwind` boundary around a disposable decode/probe job; the guard restores its prior value on success or unwind. For a contained panic, the hook records a best-effort sanitized diagnostic to the log and returns **without restoring the terminal/fd 2, disabling rendering, invoking the default hook, or requesting shutdown**. The catch boundary reports a normal job failure, drops the affected decoder, and lets the worker accept later jobs. Do not hold shared application locks or mutate Session inside that boundary. Merely adding `catch_unwind` without the flag is insufficient because the hook runs first. Logging in the hook must not block on the worker's own locks or panic; it may omit a diagnostic if the logging path is unavailable. Containment covers Rust unwinding panics, not aborts or foreign-library crashes.

For an uncontained panic on the application thread or any worker, the hook first marks rendering disabled, performs best-effort terminal restoration and fd-2 restoration, and only then invokes the previous/default hook so the panic diagnostic can appear on the primary screen. It never waits for application state or the writer and never panics itself. Keep terminal cleanup idempotent and independently accessible. Store the redirect guard in `Arc<Mutex<Option<gag::Redirect<File>>>>`: restoration uses `try_lock`, recovers the guard from an acquired poisoned lock, takes the redirect with `Option::take`, releases the mutex, then drops the redirect to restore fd 2 **before** calling the previous hook. Normal teardown uses the same take-once slot. Never hold this mutex during rendering, logging, worker joins, or other application work. If the slot is busy, skip it rather than deadlocking; fd restoration and diagnostic visibility are best effort in that case. Publish the newly constructed redirect directly into the slot, with no intervening fallible work, before starting workers or terminal setup; test panic cleanup immediately after installation as well as after terminal entry.

After an uncontained unwind on the application thread, an outer unwind boundary attempts engine/writer teardown and resumes unwinding without reporting success. An uncontained worker panic wakes/fails the application so it tears down. Contained job failures never take this fatal path. Ordinary rendering stops once fatal panic cleanup begins. Do not rely on a guard dropping after the default hook prints, and do not promise a fresh checkpoint from corrupted state or an aborting process.

## 12. Validation and delivery order

Implement in reviewable increments: isolate playback subprocess tests, establish queue-only recovery and the tokenized engine contract, and extract the shared runtime with queue/state semantics; build the compact Ratatui interface and browser; add best-effort artwork; then add the actual spectrum. The implementation plan must start from the revised state and load-adoption contracts above. Each increment must leave existing CLI functionality working. This is one player milestone; a later server or GUI gets its own design.

Every test launching a playback binary must give the child an isolated temporary state profile, including error paths, probe cases, PTY/signal helpers, and `a_stalled_remote_open_is_quittable_before_anything_loads` in `tests/cli_playback.rs`. Use a shared subprocess helper that sets `XDG_STATE_HOME` on Linux and the corresponding test profile override on other supported platforms; never mutate the test runner's process-global environment. Keep the temporary directory alive until the child has exited. Contention tests deliberately share one temporary profile among their own children; no test may read, lock, or write the developer's real playback profile. Audit all binary launch sites before enabling the lock.

Required evidence:

- Existing playback, cancellation, provenance, HTTP, feed, and persistence suites remain green.
- Queue tests cover duplicate media entries, reorder without reload, active removal, end-of-queue, failure behavior, stale/double completion events, both completion provenances, auto-advance into partial/completed episodes, the 256-entry cap, atomic rejection of oversized batches, and checkpoint eviction of queued media.
- State tests cover v1/v2 migration, queue restoration, single-writer ownership, and persistence failure. For malformed active references, media mismatch, duplicate IDs, source/identity mismatch, malformed queue field types/entries, and over-capacity schema-3 queues, assert that all checkpoints, volume, `current_media`, and bookkeeping survive; assert only the specified queue fields reset, the original bytes are copied, and backup failure disables writing without changing the original. Include a larger queue from a same-schema build, read-only history access despite invalid queue data, and unchanged handling of invalid base state/unsupported versions. A process test holds A's lock through a final newer checkpoint, proves B cannot load while it is held, and verifies a fresh B invocation reads that checkpoint after A exits. Test contention for both `tui` and `play`, lock errors, initialization failure release, and queue/checkpoint interleaving through Session with deliberately delayed writes. Under a held temporary profile lock, absent-file and directory invocations must still report their source errors before attempting the lock.
- Engine/Session tests submit two loads for distinct queue IDs sharing one `MediaId` before draining events, and assert exact token-based adoption. Cover reordered/removed pending entries, rejected admission, failure before adoption, stop/shutdown cancellation, fresh-token retry, device recovery while another load is pending, stale/duplicate outcomes, and progress that arrives before its load has been adopted. Saturate the event channel/backlog: protected load outcomes must survive in order, including in the shutdown report, with exactly one outcome per accepted load and bounded pending-token storage. Recheck event reserve arithmetic alongside existing seek/fault/shutdown guarantees.
- Ratatui buffer/input tests cover normal, compact, tiny, empty, loading, failed, unknown-duration, and estimated-position screens; selection remains distinct from playback. Include mixed dimensions (100×20 and 60×40), saved-but-unloaded state, all §4 transport rules, volume aliases, mouse-capture toggle, and full redraw invalidation.
- Podcast resolver fixtures cover a rotated enclosure under the same episode key, reordered feeds, absent episode/subscription/cache fallback, present-but-unplayable entries, corrupt-cache failure, and stable checkpoint identity. Assert no network calls from restoration/browsing/enqueueing remote metadata; only explicit play uses remote preparation.
- Artwork fixtures cover supported images, fallback ordering, missing/corrupt/oversized input, and terminal resize cleanup. Inject unwinding panics into artwork and background metadata jobs: assert placeholder/unknown metadata, continued playback/rendering, unchanged fd-2 redirect, and a subsequent successful job after the contained flag resets. Panics outside these job boundaries must still take the fatal path.
- Spectrum tests use known tones, silence, opposite-phase channels, mono/stereo/multichannel output, dropped samples, ring wraparound, and stale revisions. At 44.1, 48, 96, and 192 kHz, assert every emitted band owns at least two bin centers, bands do not overlap, and no bin is duplicated; verify the merged low bands react to low-frequency tones. Test transport-instance/generation/epoch mappings across seek, device recreation, and generation reuse, sample/descriptor capacity exhaustion, frame alignment, and saturated visualization channels without blocking playback.
- Subprocess/PTY tests send SIGINT, SIGHUP, and SIGTERM during playback and stalled HTTP preparation for both `tui` and legacy `play`, including `play` without a terminal. Verify shutdown flush, exit code `128 + signal_number`, and that the next invocation can acquire the lock. Send a direct fd-2 write from a child/helper to verify it reaches the log rather than the PTY while the TUI is active. Verify normal restoration, a closed PTY that cannot be restored, and fatal panics before redirection, immediately after redirection, and after terminal entry; the default-hook diagnostic must use restored stderr after leaving any alternate screen. Exercise a busy/poisoned restoration slot without deadlock or double-drop, with the documented best-effort limit for a busy slot. Use a subprocess per signal/redirect/panic case so tests cannot damage the test runner's process state.
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
- [signal-hook signal iterator](https://docs.rs/signal-hook/latest/signal_hook/iterator/struct.SignalsInfo.html)
- [gag stderr redirection](https://docs.rs/gag/latest/gag/struct.Redirect.html)
- [gag redirect restoration source](https://docs.rs/gag/latest/src/gag/redirect.rs.html)
- [rtrb chunk operations](https://docs.rs/rtrb/latest/rtrb/chunks/index.html)
- [Rust panic hook order](https://doc.rust-lang.org/std/panic/fn.set_hook.html)
- [Rust unwind containment](https://doc.rust-lang.org/std/panic/fn.catch_unwind.html)
