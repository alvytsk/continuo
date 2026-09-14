# Continuo

A keyboard-first terminal audio player for local audio, finite HTTP media, and podcasts.

Milestones 0 through 4 are implemented: domain types and identities (M0), local playback over Symphonia and CPAL with position tracking (M1), durable checkpoint persistence (M2), finite HTTP media with capability probing and range-based seek (M3), and RSS/Atom subscriptions with episode listing and progress (M4). A full terminal UI (M5) is not implemented yet; today's interface is a status line plus a handful of keys (space to pause, the arrow keys to seek, `s`/`p` to stop/play, `q` to quit).

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

## Podcasts

    continuo subscribe http://feeds.rucast.net/radio-t --as radio-t
    continuo feeds
    continuo episodes radio-t -n 5
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

### One process at a time

There is no locking between processes. Two `continuo` commands writing
subscriptions at the same time can lose one of the two writes — each file
write is atomic on its own, so neither file can be left half-written, but
nothing serializes one whole command against another. Run one at a time.

### Where the files live

| Location | Contents |
|---|---|
| `$XDG_DATA_HOME/continuo/subscriptions.json` | Subscriptions — durable user data |
| `$XDG_CACHE_HOME/continuo/feeds/<feed-id>.json` | Cached episodes — refetchable |
| `$XDG_STATE_HOME/continuo/state.json` | Checkpoints and volume |

On macOS and Windows these resolve to the platform's own data, cache and
local-data directories.

The cache is the only one of the three that is disposable, and `continuo
refresh <slug>` is the way to rebuild it — the same command the messages
above name. Subscriptions and checkpoints are not disposable: `continuo
unsubscribe <slug>` is how a subscription goes away.

## Design and roadmap

Read the [architecture](docs/architecture.md), the [M3 acceptance coverage map](docs/m3-acceptance.md), and the [approved foundation spec](docs/superpowers/specs/2026-09-07-continuo-foundation-design.md).

M1 added local playback, M2 durable resume, M3 finite HTTP playback, and M4 feeds and subscriptions. M5 adds the TUI, over the same `continuo::library` functions the commands above already call.

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
