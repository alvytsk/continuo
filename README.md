# Continuo

A keyboard-first terminal audio player for local audio, finite HTTP media, and podcasts.

Milestones 0 through 6 are implemented: domain types and identities (M0), local playback over Symphonia and CPAL with position tracking (M1), durable checkpoint persistence (M2), finite HTTP media with capability probing and range-based seek (M3), RSS/Atom subscriptions with episode listing and progress (M4), a Ratatui terminal player with a persistent queue, a browser, cover art and a frequency spectrum (M5), and subscribing, refreshing and unsubscribing from the player's Podcasts tab (M6). M5's automated suites pass, but its manual checks in real terminals (Ghostty, Zellij, Herdr) have not been run yet; [docs/m5-acceptance.md](docs/m5-acceptance.md) records both. M6's automated suites also pass, but its manual check against a real Radio-T feed has not been run yet; [docs/m6-acceptance.md](docs/m6-acceptance.md) records it. `continuo tui` opens the [terminal player](#terminal-player). `continuo play` keeps its original interface: a status line plus a handful of keys (space to pause, the arrow keys to seek, `s`/`p` to stop/play, `q` to quit).

## Development

Install Rust through rustup. The repository pins Rust 1.98.1 and the rustfmt and clippy components. Dependency versions are recorded in the committed Cargo.lock.

Run from the repository root:

    cargo run --locked -- play <path-or-url>
    cargo run --locked -- tui
    RUST_LOG=continuo=debug cargo run --locked -- play <path-or-url>
    cargo fmt --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked

Logging goes to stderr, except under `tui`, which sends it to a per-run log file (see [Logs](#logs)). Runtime code forbids unsafe code and denies unwrap/expect; tests may use unwrap/expect for assertions and fixtures.

M0 has no audio system dependency; every later milestone requires libasound2-dev on Linux for CPAL — the runtime libasound.so.2 alone is insufficient.

## Usage

    continuo play ~/Music/episode.mp3
    continuo play https://example.com/podcast/episode-42.mp3
    continuo play https://example.com/podcast/episode-42.mp3 --probe-only
    continuo tui

`--probe-only` opens the source, prints what was found, and exits without touching an audio device or the terminal.

A **second** positional turns the first one into a subscribed feed's slug and the second into an episode index — `continuo play radio-t 3`. See [Podcasts](#podcasts).

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

## Terminal player

    continuo tui [--mouse on|off] [--artwork auto|blocks|off]

`tui` opens a full-screen player on the saved queue. It restores the queue,
the active entry, the volume and every checkpoint, and **never starts playing
on its own**: no track is loaded and nothing is fetched from the network until
you press a playback key. (Local files' tags and the active local entry's
cover art are read in the background.) The audio device is created on the
first load, so the player is usable on a machine with no output device.

A podcast episode's cover is the feed's `itunes:image` (the episode's own
first, then the channel's), as recorded in the feed cache at the last
`refresh`. It is downloaded only once playback has opened a network
connection, never for a merely restored or enqueued episode. A podcast
whose feed names no image, and a plain URL entry, show the front cover
embedded in the stream's own tag (ID3 `APIC`, FLAC `PICTURE`) once the
track is loaded; the decoder reads it while opening the stream, so it
costs no extra request. Until then, and when there is none, the
placeholder.

- `--mouse off` starts with mouse capture disabled, leaving the terminal's
  (or multiplexer's) own text selection and scrolling alone. `m` toggles it
  at any time. The default is `on`.
- `--artwork auto` (the default) asks the terminal which image protocol it
  supports and falls back to colored half-blocks when it does not answer
  within 250 ms. `blocks` always uses half-blocks without asking; `off` never
  loads artwork and shows only the placeholder.
- Under tmux (a `TERM` starting with `tmux`, or `TERM_PROGRAM=tmux`),
  `--artwork auto` and `--artwork blocks` run
  `tmux set -p allow-passthrough on`, which changes that pane's option for as
  long as the pane lives. The image library does this while choosing a
  protocol; `--artwork off` avoids it.

The layout adapts to the terminal size. At 80×28 and above it shows the
cover, track information, spectrum, transport and progress above the queue;
below 80 columns **or** 28 rows it is compact, below 50 **or** 18 it is
minimal (no cover, no spectrum), and below 30 **or** 8 it asks for a larger
window while space and `q` keep working. Either dimension alone is enough to
drop a tier, so 100×20 is compact.

### Keys

| Key | Action |
|---|---|
| Space | Pause or resume. Before anything is loaded, load the restored active entry (or, with none, the selected row); after the last entry ended, replay it |
| Enter | Play the selected queue entry |
| Up/Down or `j`/`k` | Move the selection; playback does not change |
| `J`/`K` | Move the selected entry down/up |
| Left/Right | Seek backward/forward 10 seconds (a burst of presses becomes one seek) |
| Home | Restart the track from the beginning |
| `-` or `_` / `+` or `=` | Volume down/up by 5% |
| `s` / `p` | Stop / play |
| `[` / `]` | Previous / next queue entry; never wraps |
| `d` | Remove the selected entry |
| `b` | Open the browser |
| `a` / `r` / `R` / `d` in the browser's Podcasts tab | Subscribe by URL, refresh the highlighted feed, refresh all, remove with `y` to confirm |
| `a` | Type a path or an `http(s)://` URL to enqueue |
| `c` | Clear the queue, after a `y` confirmation |
| `?` | Show the key help |
| `m` | Toggle mouse capture |
| Ctrl-L | Redraw the whole screen and re-place the cover |
| Esc | Close the open overlay or cancel typing |
| `q` / Ctrl-C | Quit |

Ctrl-C quits and Ctrl-L redraws from **anywhere**, including while typing, in
the help and confirmation overlays, and in the browser; Ctrl-L leaves an open
overlay open. Every other key belongs to what is open: while typing after `a`,
printable keys (`q` and space included) are text, Enter enqueues and Esc
cancels; the clear confirmation takes `y` and treats any other key as no; the
help closes on `?` or Esc. A Ctrl or Alt chord never fires a plain shortcut —
Ctrl-D does not remove an entry, Alt-q does not quit — while Shift still
reaches `J`, `K`, `+`, `_`, `?`, `[` and `]` on terminals that report it.

Seeking before anything is loaded answers `Play a track before seeking` and
opens nothing; while a track is loading it answers `Still loading`; after the
last entry ended, Left/Right answer `Track ended; press play to replay` and
Home restarts it. With an empty queue and nothing playing, Space, `p`, Enter,
Home and Left/Right answer `Queue is empty`.

With mouse capture on, a click selects a queue row and a click on the
already selected row plays it, the wheel over the queue moves the selection,
the transport buttons act like their keys, and a click on the progress bar
seeks — only for a loaded track whose duration the decoder confirmed. The
mouse does nothing while an overlay is open, and nothing depends on it.

### The browser

`b` opens a browser over one directory at a time — the active local entry's
directory, else the directory `tui` was started in — and over cached podcast
subscriptions. Up/Down or `j`/`k` move, Tab switches between Files and
Podcasts, Enter opens a directory or a feed and enqueues a file or an episode,
Space marks several rows to enqueue together, Backspace or Left goes back up,
and `b` or Esc closes it. Directories are read one level at a time; nothing
indexes a library recursively.

**Opening the browser never refreshes a feed.** The Podcasts tab lists what
was last cached, exactly like `continuo episodes`; updating it is an explicit
act, `r` or `R` in the browser or `continuo refresh` from a shell, and the
same goes for `a` and `d` beside `continuo subscribe` and `continuo
unsubscribe`. Enqueueing or restoring a URL or an episode makes no network
request either; only playing it does.

### The queue

Enqueueing appends and never changes what is playing. The queue is kept in
`state.json` with the checkpoints, so it survives a restart, and the same
file may hold the same track twice: duplicates share one listening history
but keep their own places in the queue. When a track ends, the next entry
starts from its own resume point (a finished one replays from the beginning);
the last entry simply ends — there is no wrap, shuffle or repeat. A load that
fails leaves the queue alone and waits for you rather than skipping ahead.

This build supports **256 queue occurrences**, including duplicates. An
enqueue that would go past that is refused whole with `Queue is full (256
entries)` rather than truncated. The limit is not a property of the file: a
larger queue written by some other build is reset on load (see below) and
never costs a checkpoint. Checkpoints keep their own cap of 512 media, and a
queued track's history can still be evicted by enough other listening; the
entry stays queued and then starts from zero.

A queue entry for a podcast episode remembers the episode, not a list
position. Before loading it, the player looks the episode up in the local
feed cache and uses its current enclosure; when the episode, the subscription
or the cache file is gone, it plays the URL it last saw and says `Using saved
episode source`.

Before the active entry is loaded, the progress line shows its **saved
history**, not a live position:

- `12:34 saved` — the checkpoint's resume point;
- `~12:34 saved` — an estimated resume point (see
  [Seeking accuracy](#seeking-accuracy));
- `played` — finished; playing it starts from the beginning;
- `position unknown` — a checkpoint without a position.

Queue rows show the same labels. They describe what was saved last time, not
proof that the file or URL is still there.

If the queue part of `state.json` is damaged — a malformed entry, duplicate
entry IDs, an entry whose source does not match its identity, or more entries
than this build supports — only the queue is reset: checkpoints, volume and
the current media survive. The original bytes are first copied to
`state.json.queue-recovery-<timestamp>`, and the player says what was reset
and where the copy is. A bad active-entry reference alone keeps the entries
and clears only that reference. If the copy cannot be made, the session runs
unsaved (`unsaved` in the header) and the original file is left untouched.

A write that fails while the player runs — a full disk, a directory that is
no longer writable — shows `not saving` in the header until a later write
succeeds. Continuo keeps retrying with the newest state in the meantime.

### Quitting, signals and exit status

`q` and Ctrl-C exit 0. SIGINT, SIGHUP and SIGTERM — including a pane or
terminal window closing — take the same path: the final position is captured
and flushed, the terminal is restored, and the process exits with
`128 + signal number` (130, 129 and 143). `continuo play` follows the same
contract, with or without a terminal. A flush that fails is reported as
`State was not saved: …` after the terminal is restored; the player never
claims a save it could not confirm. SIGKILL, a crash or power loss keep only
the last completed write.

### Logs

While `tui` runs, everything written to the process's standard error —
tracing, panics inside background jobs, and messages printed directly by ALSA
or other C libraries — goes to a new log file instead of the screen:

    $XDG_STATE_HOME/continuo/logs/continuo-tui-<UTC timestamp>-<pid>.log

Each run creates its own file and keeps the five most recent earlier ones.
The file has no size cap. `RUST_LOG=continuo=debug continuo tui` works as
usual; the output lands in that file. A crash is the exception: the panic
message is printed on the restored terminal, after the player has left the
alternate screen.

## Podcasts

    continuo subscribe http://feeds.rucast.net/radio-t --as radio-t
    continuo feeds
    continuo episodes radio-t -n 5
    continuo episodes web-standarts --reverse -n 5
    continuo play radio-t 3
    continuo play radio-t 3 --probe-only
    continuo refresh radio-t
    continuo refresh
    continuo unsubscribe radio-t

`subscribe` fetches the feed once, stores the subscription, and caches the
episodes it parsed. Everything after that reads the cache — `feeds`,
`episodes` and `play` never touch the network for feed data — so listings
work with the feed's server unreachable and with no network at all. **Nothing
refreshes on its own.** A feed's episode list changes only when you run
`refresh`, which is the whole of Continuo's update policy: no background
poller, no refresh-on-listing, no retry loop.

### Listing feeds and episodes

    $ continuo feeds
    SLUG        EPISODES  REFRESHED (UTC)   TITLE
    radio-t            4  2026-09-11 18:33  Радио-Т

A subscription with no cache behind it shows `—` episodes and `never`.
`subscribe` writes a cache immediately, so this is what a subscription looks
like once its cache file has gone: those two cells are the whole report, and
`feeds` exits zero for it rather than treating it as an error. A cache that
exists but cannot be read is a different thing entirely, and is not quietly
downgraded into this one; see
[When the cache is unusable](#when-the-cache-is-unusable).

    $ continuo episodes radio-t
      #  PROGRESS            AUDIO  PUBLISHED (UTC)  TITLE
      1  23:14               -      2026-09-06       Радио-Т 987
      2  ~18:02 / (1:42:00)  -      2026-08-30       Радио-Т 986
      3  played              -      2026-08-23       Радио-Т 985
      4  position unknown    none   2026-08-09       Bonus: outtakes

Every timestamp is UTC, and the column header says so. No local conversion is
attempted: the offset cannot be determined reliably in a multithreaded
process, and a mislabeled local time would be worse than an honest UTC one.

The `PROGRESS` column has five states:

- `—` — no checkpoint at all: this episode has never been opened.
- a position, such as `23:14` — decoder-confirmed, the resume point.
- `~18:02` — an **estimated** position, left by a byte-offset seek on an MP3
  with no seek index. It is never presented as a confirmed one (see
  [Seeking accuracy](#seeking-accuracy)).
- `played` — finished. Reopening it starts from the beginning.
- `position unknown` — a checkpoint exists but carries no position. Distinct
  from `—`, and deliberately not collapsed into it.

`/ (1:42:00)` beside a position is the **feed's own** `itunes:duration`
claim, in parentheses because nothing has verified it — Continuo never
decodes an episode to confirm a length, and checkpoints store none. It is
shown only beside a position, since `played / (1:42:00)` would imply a
comparison the cell is not making.

`AUDIO` is a column of its own. `none` means the feed item has an identity
but no usable enclosure, so there is nothing to play; progress recorded
earlier still shows in its own column rather than being hidden by a removed
enclosure.

The progress column is joined from `state.json`. A state file that cannot be
read fails the listing instead of printing every episode as unplayed —
misreporting a listener's whole history as empty would be worse than not
answering.

### Indices

Episode indices are 1-based and follow **feed order** — the order the
document listed its items, never re-sorted by date or title. `-n 5` changes
how many rows are displayed, never what an index means: index 3 is the third
item in the cached list whether you print five rows or all of them.

Feeds are not all ordered the same way. Radio-T lists its newest episode first;
web-standards lists its first episode of 2016 first, so its newest is index 542.
`--reverse` starts from the end of the feed instead, and `-n` then counts from
the end: `continuo episodes web-standarts --reverse -n 5` shows its five newest.
Every row keeps its index, so `play` still resolves the number you read. It is
the feed's own order reversed, not a sort by date — on Radio-T it puts the
oldest episode on top.

Indices renumber only when a `refresh` replaces the cache. Between refreshes
they are stable, so `continuo episodes radio-t` followed by `continuo play
radio-t 3` always plays the row that was printed.

### Playing

`play` keeps its original single-argument form: a path or an `http(s)://`
URL. A **second** positional makes the first one a subscription slug and the
second the episode index.

    continuo play ~/Music/episode.mp3     # a file
    continuo play https://example.com/x.mp3   # a URL
    continuo play radio-t 3               # a subscribed feed's episode

`--probe-only` works with both forms. With an index it applies *after* the
episode is resolved, so it prints what the episode's enclosure turned out to
be and writes no state at all.

There is **no offline audio**. Only the episode list is cached; playing an
episode streams its enclosure over HTTP every time, and needs the CDN
reachable.

### Slugs

`--as <slug>` names a subscription explicitly. A slug is 1–32 ASCII lowercase
letters, digits or hyphens; an explicit one that is already taken is refused
outright rather than silently renamed.

Without `--as`, a slug is derived from the feed's title: ASCII letters are
lowercased and kept, ASCII digits are kept, and every other run of characters
— punctuation, whitespace and non-ASCII letters alike — collapses to a single
A publication date is read as RFC 2822, plus the one spelling that standard
omits: real feeds routinely write the zero offset as `UTC`, which RFC 2822's
obsolete-zone rule does not define, so a strict reading blanks the date for
every episode in such a feed. That spelling is accepted; anything else that
will not parse leaves `PUBLISHED` as `—` and keeps the episode, because a bad
timestamp is not a reason to hide an episode.

`-`, with leading and trailing runs dropped entirely. **No transliteration is
attempted.** A title written entirely in a non-ASCII script therefore yields
nothing at all, and the slug falls back to the same transform over the feed
URL's host with a leading `www.` stripped — so `Радио-Т` at
`https://radio-t.com/rss/` becomes `radio-t-com`, not `radio-t`. A *derived*
slug that collides takes the first free `-2`, `-3`, … suffix, with the base
shortened so the total stays within 32 characters.

Pass `--as` when you want a name you chose rather than one derived this way.

### Refreshing, and what a nonzero exit means

`refresh <slug>` updates one subscription; `refresh` with no slug updates
every one, strictly in order, with no concurrency and no retries. Both send a
conditional request when there is a usable cache to revalidate, so an
unchanged feed costs a 304:

    $ continuo refresh radio-t
    radio-t: unchanged

    $ continuo refresh
    radio-t: updated, 412 episodes retained, 3 skipped
    sysdesign: failed: network error while Open: …

A batch prints **every** feed before deciding its status, and then exits
nonzero if any of them did not complete:

    continuo: 1 of 2 feeds did not complete successfully

That rule is general: **a partial success never exits zero.** `subscribe`,
`unsubscribe` and `refresh` each commit in two steps across two files, and
when the second step fails the command says exactly what did and did not
happen and still exits nonzero — `radio-t: 412 episodes were cached, but the subscription itself
could not be saved; nothing is subscribed: …`, or `radio-t: the subscription
was removed, but its cached episodes could not be deleted: …`. Read the line
before trusting the status: the first half really did land.

### When the cache is unusable

    continuo: no cached episodes for radio-t; run continuo refresh radio-t
    continuo: corrupt cache for radio-t: cache file is malformed (syntax error at line 1, column 2); run continuo refresh radio-t
    continuo: cache parser 99 differs from 1 for radio-t; run continuo refresh radio-t

All three name the same recovery, because all three have it: the cache is
refetchable data and `refresh` rebuilds it unconditionally when it is
missing, corrupt, or stamped by a parser this build does not recognize. A
corrupt file is reported and **left where it is** — listing a feed never
rewrites, quarantines or deletes anything.

### Identities, and what resubscribing does

A podcast episode and a direct URL are **different things to Continuo**, even
when the bytes are identical. `continuo play radio-t 3` checkpoints the
*episode* — the feed's identity plus the item's own: its GUID where it has
one, otherwise its enclosure URL, otherwise its link. `continuo play
https://cdn.example.org/987.mp3` checkpoints the *URL*. Progress does not
carry from one to the other.

**How much a change of CDN survives depends on which of those three the item
had.** For an item with a `<guid>` — which is nearly every podcast item, and
what publishers are supposed to provide — the identity is the feed plus that
GUID, so the episode's position survives the show moving its audio elsewhere.
For an item with **no** GUID, identity falls through to the enclosure URL
itself, and a move therefore changes the identity: that episode's position is
lost and it shows as unplayed again. Nothing is synthesized to prevent this —
an invented identity would be worse than an honest one that changed.

`unsubscribe` removes the subscription and its cached episodes and **keeps
every checkpoint**. Resubscribing to the same feed mints a new feed identity,
so those checkpoints are not reattached — they are orphaned, and the episodes
show as unplayed again. This is deliberate: reattaching would require
trusting a feed URL to mean the same feed forever, and feed URLs move.

### Feed-format limits

- **RSS 2.0 and Atom 1.0 only.** RSS 1.0 (RDF) is refused by name —
  `unsupported feed format` — rather than half-parsed.
- **XML only.** JSON Feed is not supported.
- Item titles are display text. An Atom `title[type=html]` is decoded once
  and then kept **literally, markup and all**: reading it properly needs an
  HTML parser, and this project does not add one. HTML entities that are not
  XML entities (`&nbsp;`, for instance) survive as their literal text for the
  same reason.
- An item with no GUID, no enclosure and no link has no identity and is
  skipped and counted; nothing is ever synthesized to stand in for one.
- An item with an identity but no usable enclosure is **kept** and listed
  with `AUDIO: none`.
- Bytes decide the encoding, not the declaration. A document that will not
  decode is refused rather than repaired with replacement characters.

### One player per profile

`continuo tui` and `continuo play` take an exclusive lock on
`state.lock`, next to `state.json`, before they read any playback state, and
hold it until their final write is flushed. A second player on the same
profile — `tui` or `play`, in any combination — refuses to start, before it
opens an audio device or touches the terminal:

    continuo: Another Continuo player is using this state profile

`play` still reports a missing file or an invalid source first, since it
resolves its source before asking for the lock. The lock file is never
deleted; the lock is released when the process exits, however it exits.

Feed commands (`subscribe`, `unsubscribe`, `refresh`, `feeds`, `episodes`)
and `play --probe-only` take no lock and can run beside a player. They are
still not serialized against **each other**: two commands writing
subscriptions at the same time can lose one of the two writes — each file
write is atomic on its own, so neither file can be left half-written, but
nothing serializes one whole command against another. Run those one at a
time.

### Where the files live

| Location | Contents |
|---|---|
| `$XDG_DATA_HOME/continuo/subscriptions.json` | Subscriptions — durable user data |
| `$XDG_CACHE_HOME/continuo/feeds/<feed-id>.json` | Cached episodes — refetchable |
| `$XDG_STATE_HOME/continuo/state.json` | Checkpoints, volume, the queue and its active entry |
| `$XDG_STATE_HOME/continuo/state.lock` | The player lock (empty, never deleted) |
| `$XDG_STATE_HOME/continuo/logs/` | One log per `tui` run; the five most recent earlier ones are kept |

On macOS and Windows these resolve to the platform's own data, cache and
local-data directories.

The cache and the logs are disposable, and `continuo
refresh <slug>` is the way to rebuild it — the same command the messages
above name. Subscriptions and checkpoints are not disposable: `continuo
unsubscribe <slug>` is how a subscription goes away.

## Design and roadmap

Read the [architecture](docs/architecture.md), the [M3 acceptance coverage map](docs/m3-acceptance.md), the [M5 acceptance record](docs/m5-acceptance.md), and the [approved foundation spec](docs/superpowers/specs/2026-09-07-continuo-foundation-design.md).

M1 added local playback, M2 durable resume, M3 finite HTTP playback, M4 feeds and subscriptions, and M5 the terminal player, over the same `continuo::library` functions the commands above already call.

Non-UTF-8 local paths are unsupported. Position will be an estimate when device latency is unavailable, and seek support may remain unknown until probed. HTTP transport never implies live radio.

## Playback state

Position, completion, volume and the `tui` queue are written to a small JSON
file so that quitting and relaunching the same file resumes where you left
off.

- **Linux:** `$XDG_STATE_HOME/continuo/state.json`, falling back to
  `~/.local/state/continuo/state.json`
- **macOS and Windows:** the platform's local data directory

The file holds one checkpoint per media identity, capped at 512 entries, plus
the queue (schema 3; files from earlier builds load with an empty queue), and is
replaced atomically — a crash mid-write cannot leave a truncated file. A file
this build cannot read is preserved rather than overwritten: garbage is moved
aside as `state.json.rejected-<timestamp>`, and a file from a newer build is
left exactly where it is with writing disabled for that session.

Reaching the end of a track marks it complete and keeps the position it ended
at; reopening a completed track starts from the beginning.

One checkpoint is kept per media identity, and a podcast episode's identity is
its feed plus the item's own identity rather than the URL its audio happens to
be served from — so `continuo play radio-t 3` and `continuo play
https://cdn.example.org/987.mp3` keep separate positions even when the bytes
are the same. What the item's own identity is, and therefore how much a change
of CDN survives, is covered under
[Identities, and what resubscribing does](#identities-and-what-resubscribing-does).

Deleting `state.json` forgets every remembered position — local files, URLs
and podcast episodes alike — which is the last way out if a stored position
ever stops a file from opening. There is no supported way to edit it by hand:
its checkpoint keys are canonical identities, and a key this build cannot
parse makes the whole file unreadable rather than the one entry.
