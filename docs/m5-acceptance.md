# Milestone 5 acceptance record

`docs/superpowers/specs/2026-09-14-continuo-ratatui-design.md` §12 lists the evidence M5's terminal player must produce: automated suites for the queue, durable state, the load contract, the Ratatui screens, podcast resolution, artwork, the spectrum and the process lifecycle, then manual runs in three real terminal environments, then the repository gates. This page records both halves: which tests discharge each automated bullet, and — where discharge is partial — exactly why; and the manual terminal checks, **none of which has been performed yet**.

## Honesty statement

The manual rows below were written by an automated implementation agent that has no terminal emulator, no multiplexer and no display. It could not run Ghostty, Zellij or Herdr, and it did not. **Every manual row is marked "not run — requires a human at the terminal"**; none is marked passed, and nothing on this page should be read as evidence that images, mouse input, pane switching or exit restoration work in those environments. Browser-rendered checks were not substituted: §12 says they do not count. A person has to perform each row and replace its result.

What *was* done without a terminal: `cargo build --release --locked` succeeded on this commit (Linux, `target/release/continuo`, `continuo tui --help` prints the `--mouse on|off` and `--artwork auto|blocks|off` options), and the automated suites below ran under pseudo-terminals, which exercise raw mode, the alternate screen, signals and fd-2 redirection but not a real emulator's image protocols, mouse reporting or multiplexer behavior.

## Automated evidence (§12)

Five bullets are only partly discharged; each is marked **Partial** with the reason.

| §12 bullet | Discharged by | Notes |
|---|---|---|
| Existing playback, cancellation, provenance, HTTP, feed and persistence suites remain green | The whole `cargo test --locked` run: `tests/engine_*.rs`, `tests/http_*.rs`, `tests/m4_*.rs`, `tests/persistence_*.rs`, `tests/resume_contract.rs`, `tests/estimated_*.rs` and the rest | Their continuing to pass is the evidence. |
| Queue: duplicates, reorder without reload, active removal, end of queue, failure, stale/double completion, both completion provenances, auto-advance into partial/completed episodes, the 256 cap, atomic rejection of oversized batches, eviction of queued media | `tests/m5_queue.rs` (duplicates, reorder keeps IDs, removal successor/predecessor, oversized batch rejected whole, no wrap); `tests/m5_session_queue.rs` (active removal captures and stops, non-playing removal, clear keeps history, `advancing_into_partial_and_completed_entries_uses_the_resume_policy`, `a_queued_track_can_lose_its_history_to_eviction_and_stays_queued`); `tests/m5_session_adoption.rs` (`completion_advances_once_for_either_provenance_and_never_from_a_stale_revision`, `the_last_entry_ends_the_queue_without_wrapping`); `tests/m5_runtime.rs` (`completion_advances_once_and_the_last_entry_stays_ended`, `a_failed_load_keeps_the_queue_and_does_not_skip`, `an_oversized_enqueue_is_rejected_whole_with_a_visible_message`) | **Partial.** "Reorder without reload" is structural rather than asserted end to end: `Queue` is pure data with no engine access, and `AppCommand::Move` goes only through `Session::move_entry` and a snapshot submit. No test drives a move through `PlayerRuntime` and asserts that no `Load` was sent. |
| State: v1/v2 migration, queue restoration, single-writer ownership, persistence failure, every queue-only recovery row, byte-exact backup, backup failure, same-schema larger queue, read-only history, invalid base state | `tests/m5_state_schema.rs` (round trip, v2 migration, wrong queue type, every whole-queue problem including a 257-entry same-schema queue, active-reference problems, no inference from `MediaId`); `tests/m5_state_recovery.rs` (byte-for-byte backup, no overwrite of an earlier backup, failed backup disables writing, read-only snapshots, invalid base state and newer versions); `tests/m5_session_queue.rs::queue_and_checkpoint_writes_interleave_into_one_latest_snapshot` (a deliberately slow sink); `tests/m5_runtime.rs::a_failed_final_flush_is_reported_not_claimed_as_saved` | |
| State: lock — a process holding A's lock through a newer durable fact, B refused meanwhile, B reading it afterwards; contention for `tui` and `play`; lock errors; initialization-failure release; source errors before contention | `tests/m5_tui_process.rs::play_refuses_while_tui_holds_the_profile_and_tui_keeps_its_volume`, `::tui_refuses_a_held_profile_before_entering_raw_mode`; `tests/m5_lock_process.rs` (second `play` refused, source errors before contention, probe and feed listings ignore the lock); `tests/m5_profile_lock.rs` (contention until drop, initialization failure releases, exact message) | **Partial.** Per implementation decision 17 the newer fact is volume, not a checkpoint. After A exits, the test reads `state.json` itself rather than launching a fresh B that reads the value. That a fresh invocation acquires the profile is shown separately, by the lifecycle cases below. Lock errors other than contention (no state directory, a directory that cannot be prepared, an I/O failure opening or locking `state.lock`) have no test. |
| Engine/Session: two loads of one `MediaId` from distinct rows, reordered/removed pending entries, rejected admission, failure before adoption, stop/shutdown cancellation, fresh-token retry, recovery while another load is pending, stale/duplicate outcomes, progress before adoption, saturation with one ordered outcome per load, bounded pending tokens, reserve arithmetic | `tests/m5_session_adoption.rs` (two pending loads adopt their own rows, reorder, removed target never resurrected, failure before adoption, `pending_registrations_are_bounded_and_retractable`, device recovery with a pending load, unknown/duplicate outcomes, stale-revision rejection); `tests/m5_engine_load_outcomes.rs` (`every_accepted_load_has_exactly_one_ordered_outcome_under_saturation`, stop and shutdown during a stalled open, token-scoped automatic start); `tests/m5_runtime.rs::space_retries_a_failed_switch_with_a_fresh_token`, `::two_loads_of_one_media_submitted_together_end_on_the_later_row`; existing `tests/engine_contract.rs` and `tests/engine_shutdown.rs` for seek/fault/shutdown guarantees | |
| Ratatui buffers and input: normal, compact, tiny, empty, loading, failed, unknown-duration, estimated-position screens; selection apart from playback; 100×20 and 60×40; saved-but-unloaded; all §4 transport rules; volume aliases; mouse-capture toggle; full redraw | `tests/m5_tui_render.rs` (tiers, zero-sized areas, every listed screen, `compact_drops_secondary_metadata_at_mixed_dimensions`, `a_saved_but_unloaded_entry_shows_saved_history_not_live_progress`); `tests/m5_tui_input.rs` (selection, both volume spellings, `mouse_toggle_and_full_redraw`, `ctrl_l_redraws_from_every_overlay_without_closing_it`, Ctrl/Alt chords, mouse hit regions); `tests/m5_transport_rules.rs` (every §4 row); `tests/m5_browser.rs` | |
| Podcast resolver: rotated enclosure, reordered feed, absent episode/subscription/cache fallback, present-but-unplayable, corrupt cache, stable identity; no network from restoring, browsing or enqueueing | `tests/m5_podcast_resolve.rs` (all eight cases); `tests/m5_no_network.rs` (`restoring_enqueueing_and_browsing_remote_entries_make_no_requests`, `only_an_explicit_play_prepares_the_remote_source`) | |
| Artwork: supported images, fallback order, missing/corrupt/oversized input, resize cleanup; injected panics in artwork and metadata jobs leave a placeholder/unknown metadata, playback and rendering continue, fd 2 stays redirected, a later job succeeds; panics outside job boundaries stay fatal | `tests/m5_artwork.rs`; `tests/m5_tui_images.rs` (`a_prepared_cover_is_reused_until_media_mode_or_area_changes` requests placement cleanup on an area change, `an_encoding_panic_is_contained_and_a_later_preparation_succeeds`); `tests/m5_metadata.rs::a_panicking_probe_is_contained_and_the_worker_serves_the_next_job`; `tests/m5_contained_panic.rs` (the flag resets); `tests/m5_tui_process.rs::contained_artwork_and_metadata_panics_keep_the_player_running` (all three job hooks in a real child: the placeholder status, a later track loads and draws, a later successful job's debug line reaches the log after the contained panic, the hook message never reaches the PTY, and a probed entry keeps its unknown title) and `::an_uncontained_worker_panic_restores_the_terminal_and_fails` | **Partial.** Resize cleanup is proven at the cache level: an area change asks for placement cleanup. That a real terminal leaves no stray image after a resize is one of the manual checks below. |
| Spectrum: tones, silence, opposite phase, mono/stereo/multichannel, dropped samples, ring wraparound, stale revisions; band geometry at 44.1/48/96/192 kHz; low bands react; mappings across seek, device recreation and generation reuse; capacity exhaustion; frame alignment; saturated channels never block playback | `tests/m5_spectrum_bands.rs`; `tests/m5_spectrum_tap.rs` (whole frames only, block dropped whole, descriptor exhaustion, wraparound, an unread tap never blocks the writer); `tests/m5_spectrum_engine.rs` (generation retirement and instance IDs, current-revision labels, device recreation, expiry, 20 fps limit); `tests/m5_tui_render.rs` spectrum cases; unit test `stopping_a_spectrum_thread_that_already_panicked_completes` in `src/playback/spectrum/worker.rs` | **Partial.** A seek is covered as what it does to the tap mapping, a retired generation. No test runs the spectrum across a seek command on a live engine. |
| Subprocess/PTY: SIGINT, SIGHUP and SIGTERM during playback and stalled HTTP preparation for `tui` and legacy `play` (including `play` without a terminal), with flush, `128 + n` and a next invocation that acquires the lock; a direct fd-2 write reaches the log; normal restoration; a closed PTY; fatal panics before redirection, right after it and after terminal entry, printed on restored stderr after leaving the alternate screen; a busy/poisoned restoration slot | `tests/m5_tui_process.rs`: `tui_signals_during_playback_flush_and_exit_128_plus_n`, `tui_signals_during_stalled_http_preparation_exit_128_plus_n`, `a_direct_fd2_write_reaches_the_log_not_the_pty`, `tui_opens_idle_on_an_empty_queue_and_q_restores_the_terminal`, `sigterm_restores_the_terminal_and_exits_143`, `a_hung_up_pty_still_flushes_and_releases_the_profile`, `a_panic_before_redirection_reaches_the_terminal`, `a_panic_right_after_redirection_is_printed_on_restored_stderr`, `a_panic_after_terminal_entry_leaves_the_alternate_screen_first`; `tests/m5_signals_play.rs` (both `play` cases, without a terminal); `tests/m5_contained_panic.rs` (`a_busy_slot_is_skipped_without_deadlock`, `a_poisoned_slot_still_yields_its_value`, `a_slot_is_taken_once_and_dropped_once`) | **Partial.** Legacy `play`'s signal cases run with no terminal, the case §12 names explicitly. No test sends a signal to `play` while it runs in raw mode under a PTY. The busy and poisoned slot cases are in-process unit tests of the slot, not subprocess cases. |
| Manual terminals: Ghostty, Ghostty + Zellij, Ghostty + Herdr | [Manual terminal checks](#manual-terminal-checks) below | **Not run.** See the honesty statement. |
| Gates: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked` | Run on the commit that adds this page; see the task report | |

## Manual terminal checks

**Media to prepare.** A local album whose tracks carry embedded front-cover art; a folder of tracks without embedded art that has a `cover.jpg`; one podcast episode enqueued from a subscribed feed through the browser's Podcasts tab; and one direct `https://` URL enqueued with `a`. Build with `cargo build --release --locked` and run `target/release/continuo tui` from a shell in each environment.

**Environments.** *Ghostty* — Ghostty directly, no multiplexer. *Ghostty + Zellij* — `continuo tui` in a Zellij pane inside Ghostty. *Ghostty + Herdr* — `continuo tui` in a Herdr pane inside Ghostty.

**What each check means.**

| Check | Procedure and pass condition |
|---|---|
| Image protocol or fallback | With `--artwork auto`, play the embedded-art track and the `cover.jpg` track. Record which protocol drew the cover (Kitty, Sixel, iTerm2) or that it fell back to half-blocks, and that the fallback came within about a quarter second. The podcast episode and the URL show the placeholder. |
| `--artwork blocks` | Half-block cover drawn, no terminal query visible. |
| `--artwork off` | Placeholder only, no image ever placed. |
| Resize through all four tiers | Shrink the window or pane through normal (≥80×28), compact, minimal (<50 or <18, no cover or spectrum) and the resize message (<30 or <8), then back. Space and `q` keep working at every size, and no stale image fragment remains at any step. |
| Pane switch and return | Switch to another tab or pane and back while a cover shows and audio plays. Playback continues and the screen and cover are intact, or Ctrl-L restores them. |
| Keyboard map | Every row of the README key table, including `J`/`K`, `[`/`]`, `d`, `c` then `y`, `a` with a typed URL containing `q` and a space, `?`, Esc, and a Ctrl-D that must not remove anything. |
| Mouse | With capture on: click selects, clicking the selected row plays, the wheel scrolls the queue, the transport buttons work, and clicking the progress bar seeks a decoded local track. `m` turns capture off, and the terminal or multiplexer's own text selection then works; `m` again restores clicks. |
| Ctrl-L | Redraws the whole view and re-places the cover, including with the browser and the help open, without closing them. |
| Quit with `q` | Exits 0 with the terminal restored. |
| Quit with Ctrl-C | Exits 0 with the terminal restored, also while typing after `a`. |
| Close the pane | Close the pane or window while playing. Reopen `tui` in a new pane: it starts (no lock message), and the queue and a recent position are restored. |
| Terminal state after exit | After each exit: the cursor is visible, the shell echoes typed text normally (raw mode is off), mouse clicks no longer print escape sequences, and no image is left on the screen or in scrollback. |

**Results.**

| Environment | Check | Result | Note |
|---|---|---|---|
| Ghostty | Image protocol or fallback | not run — requires a human at the terminal | |
| Ghostty | `--artwork blocks` | not run — requires a human at the terminal | |
| Ghostty | `--artwork off` | not run — requires a human at the terminal | |
| Ghostty | Resize through all four tiers | not run — requires a human at the terminal | |
| Ghostty | Pane switch and return | not run — requires a human at the terminal | |
| Ghostty | Keyboard map | not run — requires a human at the terminal | |
| Ghostty | Mouse (selection, scroll, transport, `m` with text selection while off) | not run — requires a human at the terminal | |
| Ghostty | Ctrl-L | not run — requires a human at the terminal | |
| Ghostty | Quit with `q` | not run — requires a human at the terminal | |
| Ghostty | Quit with Ctrl-C | not run — requires a human at the terminal | |
| Ghostty | Close the pane | not run — requires a human at the terminal | |
| Ghostty | Terminal state after exit | not run — requires a human at the terminal | |
| Ghostty + Zellij | Image protocol or fallback | not run — requires a human at the terminal | |
| Ghostty + Zellij | `--artwork blocks` | not run — requires a human at the terminal | |
| Ghostty + Zellij | `--artwork off` | not run — requires a human at the terminal | |
| Ghostty + Zellij | Resize through all four tiers | not run — requires a human at the terminal | |
| Ghostty + Zellij | Pane switch and return | not run — requires a human at the terminal | |
| Ghostty + Zellij | Keyboard map | not run — requires a human at the terminal | |
| Ghostty + Zellij | Mouse (selection, scroll, transport, `m` with text selection while off) | not run — requires a human at the terminal | |
| Ghostty + Zellij | Ctrl-L | not run — requires a human at the terminal | |
| Ghostty + Zellij | Quit with `q` | not run — requires a human at the terminal | |
| Ghostty + Zellij | Quit with Ctrl-C | not run — requires a human at the terminal | |
| Ghostty + Zellij | Close the pane | not run — requires a human at the terminal | |
| Ghostty + Zellij | Terminal state after exit | not run — requires a human at the terminal | |
| Ghostty + Herdr | Image protocol or fallback | not run — requires a human at the terminal | |
| Ghostty + Herdr | `--artwork blocks` | not run — requires a human at the terminal | |
| Ghostty + Herdr | `--artwork off` | not run — requires a human at the terminal | |
| Ghostty + Herdr | Resize through all four tiers | not run — requires a human at the terminal | |
| Ghostty + Herdr | Pane switch and return | not run — requires a human at the terminal | |
| Ghostty + Herdr | Keyboard map | not run — requires a human at the terminal | |
| Ghostty + Herdr | Mouse (selection, scroll, transport, `m` with text selection while off) | not run — requires a human at the terminal | |
| Ghostty + Herdr | Ctrl-L | not run — requires a human at the terminal | |
| Ghostty + Herdr | Quit with `q` | not run — requires a human at the terminal | |
| Ghostty + Herdr | Quit with Ctrl-C | not run — requires a human at the terminal | |
| Ghostty + Herdr | Close the pane | not run — requires a human at the terminal | |
| Ghostty + Herdr | Terminal state after exit | not run — requires a human at the terminal | |

When a person performs a row, replace its result with `pass` or `fail`. Fill in the note: the Ghostty, Zellij or Herdr version, the protocol detected, and for a failure what was seen. A failure stays recorded here even after it is fixed, with the fixing commit named.
