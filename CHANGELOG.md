# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-09-17

### Fixed

- A seek submitted right as the engine was free to act on it could be
  reported as cancelled instead of running: the submitter's wake-up of a
  blocked network read could land on the fetch the seek itself had just
  opened. The wake-up now targets only the read that was in flight when the
  seek was submitted.

## [0.1.0] - 2026-09-17

First release. Published to crates.io as `tenuto`.

### Added

- Plays MP3, FLAC, WAV and M4A files, direct `http(s)://` URLs, and episodes
  of subscribed RSS or Atom feeds.
- Remembers the position in every track, URL and episode, and resumes there.
- Seeks over HTTP with range requests, including MP3 podcasts with no seek
  index.
- A full-screen terminal player with a persistent queue, a file and podcast
  browser, cover art and a spectrum display.
- Feed management from the player: subscribe, refresh and unsubscribe.
- A bare `tenuto` opens the player.

[Unreleased]: https://github.com/alvytsk/tenuto/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/alvytsk/tenuto/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/alvytsk/tenuto/releases/tag/v0.1.0
