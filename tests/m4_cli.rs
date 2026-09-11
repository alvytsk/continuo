//! The M4 command surface (design doc §6.1–§6.5): argument parsing, the
//! five feed commands as whole processes, and the two `play` forms.
//!
//! The parse tests drive `Cli::try_parse_from` directly — no process, no
//! filesystem — so an arity or value-parser regression is reported as a
//! parse failure rather than as whatever the command would have done with
//! the wrong arguments.

use clap::Parser;
use continuo::cli::{Cli, CliCommand};

#[test]
fn play_selectors_are_positive_and_single_source_still_parses()
-> Result<(), Box<dyn std::error::Error>> {
    assert!(Cli::try_parse_from(["continuo", "play", "file.mp3"]).is_ok());
    assert!(Cli::try_parse_from(["continuo", "play", "radio-t", "3", "--probe-only"]).is_ok());
    assert!(Cli::try_parse_from(["continuo", "play", "radio-t", "0"]).is_err());
    assert!(Cli::try_parse_from(["continuo", "play", "radio-t", "newest"]).is_err());
    assert!(Cli::try_parse_from(["continuo", "episodes", "radio-t", "-n", "0"]).is_err());
    let parsed = Cli::try_parse_from(["continuo", "refresh"])?;
    assert!(matches!(parsed.command, CliCommand::Refresh { slug: None }));
    Ok(())
}

/// The two `play` forms are one and two positionals; a third is not a
/// spelling this CLI has, and accepting it silently would mean an ignored
/// argument rather than a reported mistake.
#[test]
fn play_takes_one_or_two_positionals_and_never_three() -> Result<(), Box<dyn std::error::Error>> {
    let one = Cli::try_parse_from(["continuo", "play", "file.mp3"])?;
    assert!(matches!(
        one.command,
        CliCommand::Play {
            index: None,
            probe_only: false,
            ..
        }
    ));

    let two = Cli::try_parse_from(["continuo", "play", "radio-t", "3"])?;
    match two.command {
        CliCommand::Play {
            source,
            index: Some(index),
            probe_only,
        } => {
            assert_eq!(source, "radio-t");
            assert_eq!(index.get(), 3);
            assert!(!probe_only);
        }
        other => panic!("expected the two-positional play form, got {other:?}"),
    }

    assert!(Cli::try_parse_from(["continuo", "play", "radio-t", "3", "extra"]).is_err());
    assert!(Cli::try_parse_from(["continuo", "play"]).is_err());
    Ok(())
}

/// `--probe-only` belongs to both forms (§6.3): with two positionals it
/// applies after resolution rather than being refused at parse time.
#[test]
fn probe_only_parses_with_either_play_form() -> Result<(), Box<dyn std::error::Error>> {
    for args in [
        ["continuo", "play", "file.mp3", "--probe-only"].as_slice(),
        ["continuo", "play", "radio-t", "7", "--probe-only"].as_slice(),
    ] {
        let parsed = Cli::try_parse_from(args)?;
        assert!(
            matches!(
                parsed.command,
                CliCommand::Play {
                    probe_only: true,
                    ..
                }
            ),
            "{args:?} must set probe_only"
        );
    }
    Ok(())
}

#[test]
fn subscribe_takes_a_url_and_an_optional_alias() -> Result<(), Box<dyn std::error::Error>> {
    let bare = Cli::try_parse_from(["continuo", "subscribe", "https://example.org/feed"])?;
    match bare.command {
        CliCommand::Subscribe { url, slug } => {
            assert_eq!(url, "https://example.org/feed");
            assert_eq!(slug, None);
        }
        other => panic!("expected subscribe, got {other:?}"),
    }

    let aliased = Cli::try_parse_from([
        "continuo",
        "subscribe",
        "https://example.org/feed",
        "--as",
        "radio-t",
    ])?;
    match aliased.command {
        CliCommand::Subscribe { slug, .. } => assert_eq!(slug.as_deref(), Some("radio-t")),
        other => panic!("expected subscribe, got {other:?}"),
    }

    assert!(Cli::try_parse_from(["continuo", "subscribe"]).is_err());
    Ok(())
}

#[test]
fn unsubscribe_and_feeds_take_exactly_their_own_arguments() -> Result<(), Box<dyn std::error::Error>>
{
    let unsubscribe = Cli::try_parse_from(["continuo", "unsubscribe", "radio-t"])?;
    assert!(matches!(
        unsubscribe.command,
        CliCommand::Unsubscribe { ref slug } if slug == "radio-t"
    ));
    assert!(Cli::try_parse_from(["continuo", "unsubscribe"]).is_err());

    let feeds = Cli::try_parse_from(["continuo", "feeds"])?;
    assert!(matches!(feeds.command, CliCommand::Feeds));
    assert!(Cli::try_parse_from(["continuo", "feeds", "radio-t"]).is_err());
    Ok(())
}

#[test]
fn episodes_requires_a_slug_and_a_positive_limit() -> Result<(), Box<dyn std::error::Error>> {
    let bare = Cli::try_parse_from(["continuo", "episodes", "radio-t"])?;
    assert!(matches!(
        bare.command,
        CliCommand::Episodes {
            ref slug,
            limit: None
        } if slug == "radio-t"
    ));

    let limited = Cli::try_parse_from(["continuo", "episodes", "radio-t", "-n", "5"])?;
    match limited.command {
        CliCommand::Episodes { limit: Some(n), .. } => assert_eq!(n.get(), 5),
        other => panic!("expected episodes with a limit, got {other:?}"),
    }

    assert!(Cli::try_parse_from(["continuo", "episodes"]).is_err());
    assert!(Cli::try_parse_from(["continuo", "episodes", "radio-t", "-n", "-1"]).is_err());
    assert!(Cli::try_parse_from(["continuo", "episodes", "radio-t", "-n", "many"]).is_err());
    Ok(())
}

#[test]
fn refresh_takes_an_optional_slug() -> Result<(), Box<dyn std::error::Error>> {
    let all = Cli::try_parse_from(["continuo", "refresh"])?;
    assert!(matches!(all.command, CliCommand::Refresh { slug: None }));

    let one = Cli::try_parse_from(["continuo", "refresh", "radio-t"])?;
    assert!(matches!(
        one.command,
        CliCommand::Refresh { slug: Some(ref slug) } if slug == "radio-t"
    ));

    assert!(Cli::try_parse_from(["continuo", "refresh", "radio-t", "extra"]).is_err());
    Ok(())
}

/// The rejection has to name the two spellings that do exist, or a mistyped
/// selector leaves the listener guessing which form they were meant to use.
#[test]
fn a_non_positive_index_explains_both_play_forms() -> Result<(), Box<dyn std::error::Error>> {
    let error = match Cli::try_parse_from(["continuo", "play", "radio-t", "0"]) {
        Err(error) => error.to_string(),
        Ok(parsed) => panic!("index 0 must be rejected, got {:?}", parsed.command),
    };
    assert!(
        error.contains("expected a positive episode index"),
        "{error}"
    );
    assert!(error.contains("play <slug> <index>"), "{error}");

    let count = match Cli::try_parse_from(["continuo", "episodes", "radio-t", "-n", "0"]) {
        Err(error) => error.to_string(),
        Ok(parsed) => panic!("-n 0 must be rejected, got {:?}", parsed.command),
    };
    assert!(count.contains("expected a positive count"), "{count}");
    Ok(())
}

#[cfg(target_os = "linux")]
mod support;

/// Whole-process behavior, driven through temporary XDG directories so that
/// nothing here can read or write the developer's own library.
///
/// Linux-gated because the assertions are about XDG paths specifically:
/// `ProjectDirs` resolves elsewhere on macOS and Windows, where these env
/// vars mean nothing. The formatting and exit-status rules these commands
/// rely on are covered platform-independently by the unit tests inside
/// `src/commands.rs`, against injected writers rather than a process.
#[cfg(target_os = "linux")]
mod process {
    use std::path::{Path, PathBuf};
    use std::process::Output;

    use crate::support::server::{DocumentReply, Script, TestServer};

    type Fallible = Result<(), Box<dyn std::error::Error>>;

    fn run_cli(root: &Path, args: &[&str]) -> std::io::Result<Output> {
        std::process::Command::new(env!("CARGO_BIN_EXE_continuo"))
            .args(args)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("RUST_LOG", "continuo=warn")
            .output()
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn succeeded(output: &Output) -> Result<String, String> {
        if output.status.success() {
            Ok(stdout(output))
        } else {
            Err(format!(
                "expected success\nstdout: {}\nstderr: {}",
                stdout(output),
                stderr(output)
            ))
        }
    }

    fn failed(output: &Output) -> Result<String, String> {
        if output.status.success() {
            Err(format!(
                "expected a nonzero exit; stdout: {}",
                stdout(output)
            ))
        } else {
            Ok(stderr(output))
        }
    }

    fn subscriptions(root: &Path) -> PathBuf {
        root.join("data/continuo/subscriptions.json")
    }

    fn feeds_dir(root: &Path) -> PathBuf {
        root.join("cache/continuo/feeds")
    }

    /// The single cache file a one-feed library has. The name is a minted
    /// `FeedId`, so it is discovered rather than assumed.
    fn cache_file(root: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let entry = std::fs::read_dir(feeds_dir(root))?
            .next()
            .ok_or("no cache entry was written")??;
        Ok(entry.path())
    }

    fn reply(path: &str, headers: Vec<(&str, &str)>, body: &[u8]) -> DocumentReply {
        DocumentReply {
            path: path.to_string(),
            status: 200,
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.to_vec(),
            conditional: true,
            header_delay: std::time::Duration::ZERO,
        }
    }

    /// Two items: one with an enclosure and one without, which is what makes
    /// the `AUDIO` column and `NotPlayable` (§6.2, §6.4) reachable from a
    /// real subscription rather than a hand-built cache.
    fn feed_xml(enclosure: &str) -> String {
        format!(
            "<rss><channel><title>Radio T</title>\
             <item><guid>ep-1</guid><title>First</title>\
             <pubDate>Wed, 01 Jan 2020 00:00:00 GMT</pubDate>\
             <enclosure url=\"{enclosure}\"/></item>\
             <item><guid>ep-2</guid><title>No audio at all</title></item>\
             </channel></rss>"
        )
    }

    /// Subscribes `root` to a feed served from `server`, returning the
    /// command's own output so a caller can assert on it.
    fn subscribe(root: &Path, server: &TestServer) -> std::io::Result<Output> {
        run_cli(
            root,
            &["subscribe", &server.url("/feed"), "--as", "radio-t"],
        )
    }

    /// §8.4: a listing changes no files. On a machine that has never
    /// subscribed there is nothing to list and nothing to create — not even
    /// the directories the first write would make.
    #[test]
    fn an_empty_library_lists_nothing_and_creates_no_directories() -> Fallible {
        let root = tempfile::tempdir()?;
        let listing = succeeded(&run_cli(root.path(), &["feeds"])?)?;
        assert!(listing.contains("SLUG"), "{listing}");
        assert_eq!(listing.lines().count(), 1, "{listing}");

        // `refresh` over an empty snapshot is an empty, successful batch.
        let batch = succeeded(&run_cli(root.path(), &["refresh"])?)?;
        assert!(batch.is_empty(), "{batch}");

        assert!(!root.path().join("data").exists());
        assert!(!root.path().join("cache").exists());
        assert!(!root.path().join("state").exists());
        Ok(())
    }

    /// §5.5/§8.4: the listings are answered from disk, so they keep working
    /// with the feed's server gone. This is also the end-to-end proof that
    /// `subscribe` committed both halves — cache and subscription — since a
    /// later process reads them back.
    #[test]
    fn a_subscription_outlives_the_server_it_came_from() -> Fallible {
        let root = tempfile::tempdir()?;
        let server = TestServer::start(Script::serving(
            feed_xml("https://cdn.example.org/1.mp3").into_bytes(),
        ));
        let subscribed = succeeded(&subscribe(root.path(), &server)?)?;
        assert!(
            subscribed.starts_with("radio-t: subscribed, 2 episodes retained, 0 skipped"),
            "{subscribed}"
        );
        server.shutdown();

        let feeds = succeeded(&run_cli(root.path(), &["feeds"])?)?;
        assert!(feeds.contains("radio-t"), "{feeds}");
        assert!(feeds.contains("Radio T"), "{feeds}");

        let episodes = succeeded(&run_cli(root.path(), &["episodes", "radio-t"])?)?;
        assert!(episodes.contains("First"), "{episodes}");
        assert!(episodes.contains("2020-01-01"), "{episodes}");
        // Nothing has ever played, and no enclosure exists on the second
        // item: the two columns say so separately (§6.2).
        assert!(episodes.contains("none"), "{episodes}");
        assert!(episodes.contains("—"), "{episodes}");

        let limited = succeeded(&run_cli(root.path(), &["episodes", "radio-t", "-n", "1"])?)?;
        assert_eq!(limited.lines().count(), 2, "{limited}");

        // The feed's server is gone, so a refresh cannot commit anything —
        // and a refresh that committed nothing must not exit zero (§6.4).
        // The outcome is still printed before the status is decided.
        let output = run_cli(root.path(), &["refresh", "radio-t"])?;
        let error = failed(&output)?;
        assert!(
            stdout(&output).starts_with("radio-t: failed:"),
            "{}",
            stdout(&output)
        );
        assert!(!error.is_empty(), "a failed refresh must say why on stderr");
        Ok(())
    }

    /// A 304 revalidates the cache and still exits zero: `refresh` promises
    /// fetch status, not that anything changed (§5.2).
    #[test]
    fn refreshing_an_unchanged_feed_reports_it_as_unchanged() -> Fallible {
        let root = tempfile::tempdir()?;
        let server = TestServer::start(Script::documents(vec![reply(
            "/feed",
            vec![("ETag", "\"v1\""), ("Content-Type", "application/rss+xml")],
            feed_xml("https://cdn.example.org/1.mp3").as_bytes(),
        )]));
        succeeded(&subscribe(root.path(), &server)?)?;

        let one = succeeded(&run_cli(root.path(), &["refresh", "radio-t"])?)?;
        assert_eq!(one, "radio-t: unchanged\n", "{one}");

        let all = succeeded(&run_cli(root.path(), &["refresh"])?)?;
        assert_eq!(all, "radio-t: unchanged\n", "{all}");
        server.shutdown();
        Ok(())
    }

    /// §6.4's cache policy, all three states, and §8.4's "a listing changes
    /// no files" through each of them: a missing cache is the normal
    /// never-refreshed state and `feeds` exits zero for it; corrupt and
    /// parser-mismatched data are not that state and must not be silently
    /// downgraded into it.
    #[test]
    fn the_cache_states_follow_their_exit_policy_without_mutating_anything() -> Fallible {
        let root = tempfile::tempdir()?;
        let server = TestServer::start(Script::serving(
            feed_xml("https://cdn.example.org/1.mp3").into_bytes(),
        ));
        succeeded(&subscribe(root.path(), &server)?)?;
        server.shutdown();

        let cache = cache_file(root.path())?;
        let healthy = std::fs::read(&cache)?;
        let subscriptions_before = std::fs::read(subscriptions(root.path()))?;

        // Missing: a zero-information row and exit zero, since this is the
        // normal state between `subscribe` and the first `refresh`.
        // `episodes` has nothing to show and says so nonzero.
        std::fs::remove_file(&cache)?;
        let listing = succeeded(&run_cli(root.path(), &["feeds"])?)?;
        assert!(listing.contains("never"), "{listing}");
        assert!(listing.contains("—"), "{listing}");
        let error = failed(&run_cli(root.path(), &["episodes", "radio-t"])?)?;
        assert!(error.contains("no cached episodes"), "{error}");
        assert!(!cache.exists(), "a listing must not create a cache entry");

        // Corrupt: not "never refreshed", so neither command may pretend it
        // is, and neither may quarantine or rewrite the file.
        let corrupt = b"{ this is not json".to_vec();
        std::fs::write(&cache, &corrupt)?;
        for args in [["feeds"].as_slice(), ["episodes", "radio-t"].as_slice()] {
            let error = failed(&run_cli(root.path(), args)?)?;
            assert!(error.contains("corrupt cache"), "{args:?}: {error}");
        }
        assert_eq!(std::fs::read(&cache)?, corrupt);

        // Parser-mismatched: a file this build's parser did not write.
        // Recovered by a refetch, never by displaying it anyway.
        let mut entry: serde_json::Value = serde_json::from_slice(&healthy)?;
        entry["parser_version"] = serde_json::json!(99);
        let mismatched = serde_json::to_vec(&entry)?;
        std::fs::write(&cache, &mismatched)?;
        for args in [["feeds"].as_slice(), ["episodes", "radio-t"].as_slice()] {
            let error = failed(&run_cli(root.path(), args)?)?;
            assert!(error.contains("cache parser"), "{args:?}: {error}");
        }
        assert_eq!(std::fs::read(&cache)?, mismatched);

        // Through all three states, the durable subscription is untouched.
        assert_eq!(
            std::fs::read(subscriptions(root.path()))?,
            subscriptions_before
        );
        Ok(())
    }

    /// §5.5: a state file that cannot be read is not "nothing has played".
    /// The listing fails rather than printing every episode as unplayed,
    /// which would quietly misreport a listener's whole history.
    #[test]
    fn an_unreadable_state_file_never_prints_unplayed_rows() -> Fallible {
        let root = tempfile::tempdir()?;
        let server = TestServer::start(Script::serving(
            feed_xml("https://cdn.example.org/1.mp3").into_bytes(),
        ));
        succeeded(&subscribe(root.path(), &server)?)?;
        server.shutdown();

        let state = root.path().join("state/continuo/state.json");
        std::fs::create_dir_all(state.parent().ok_or("state has a parent")?)?;
        std::fs::write(&state, b"not json at all")?;

        let output = run_cli(root.path(), &["episodes", "radio-t"])?;
        let error = failed(&output)?;
        assert!(error.contains("malformed"), "{error}");
        assert!(
            stdout(&output).is_empty(),
            "no row may be printed from unreadable state: {}",
            stdout(&output)
        );
        // Read-only: the unreadable file is preserved, never quarantined.
        assert_eq!(std::fs::read(&state)?, b"not json at all");
        Ok(())
    }

    /// §6.3/§6.4: every way of naming an episode that cannot be played, and
    /// the message each one owes the listener.
    #[test]
    fn selection_failures_are_refused_before_any_device_is_opened() -> Fallible {
        let root = tempfile::tempdir()?;
        let server = TestServer::start(Script::serving(
            feed_xml("https://cdn.example.org/1.mp3").into_bytes(),
        ));
        succeeded(&subscribe(root.path(), &server)?)?;
        server.shutdown();

        let zero = failed(&run_cli(root.path(), &["play", "radio-t", "0"])?)?;
        assert!(zero.contains("expected a positive episode index"), "{zero}");

        let word = failed(&run_cli(root.path(), &["play", "radio-t", "newest"])?)?;
        assert!(word.contains("expected a positive episode index"), "{word}");

        let past_end = failed(&run_cli(root.path(), &["play", "radio-t", "99"])?)?;
        assert!(past_end.contains("outside 1..=2"), "{past_end}");

        let unknown = failed(&run_cli(root.path(), &["play", "nosuch", "1"])?)?;
        assert!(unknown.contains("unknown feed: nosuch"), "{unknown}");

        let limit = failed(&run_cli(root.path(), &["episodes", "radio-t", "-n", "0"])?)?;
        assert!(limit.contains("expected a positive count"), "{limit}");

        let missing = failed(&run_cli(root.path(), &["episodes"])?)?;
        assert!(
            missing.contains("<SLUG>") || missing.contains("SLUG"),
            "{missing}"
        );

        // An item with identity but no enclosure: refused at resolution, so
        // nothing further is ever built for it (§6.4). `--probe-only` is
        // refused the same way, since the probe applies after resolution.
        for args in [
            ["play", "radio-t", "2"].as_slice(),
            ["play", "radio-t", "2", "--probe-only"].as_slice(),
        ] {
            let error = failed(&run_cli(root.path(), args)?)?;
            assert!(
                error.contains("has no audio enclosure"),
                "{args:?}: {error}"
            );
            assert!(error.contains("No audio at all"), "{args:?}: {error}");
        }
        Ok(())
    }

    /// §6.3: `--probe-only` with two positionals resolves the episode first
    /// and then probes *its enclosure*, with no device and no terminal. The
    /// one-positional form is unchanged and covered by `tests/http_cli.rs`.
    #[test]
    fn probing_an_episode_resolves_it_before_opening_anything() -> Fallible {
        let media = TestServer::start(Script::from_fixture("sine-5s.flac"));
        let feed = TestServer::start(Script::serving(
            feed_xml(&media.url("/audio.flac")).into_bytes(),
        ));
        let root = tempfile::tempdir()?;
        succeeded(&subscribe(root.path(), &feed)?)?;
        feed.shutdown();

        let probed = succeeded(&run_cli(
            root.path(),
            &["play", "radio-t", "1", "--probe-only"],
        )?)?;
        assert!(probed.contains("44100"), "{probed}");
        assert!(probed.contains("Finite"), "{probed}");
        media.shutdown();

        // A probe reads no playback state and writes none.
        assert!(!root.path().join("state").exists());
        Ok(())
    }

    /// §6.1/§6.4: `unsubscribe` removes the subscription and its cache, and
    /// an unknown slug is reported rather than silently doing nothing.
    #[test]
    fn unsubscribing_removes_both_halves() -> Fallible {
        let root = tempfile::tempdir()?;
        let server = TestServer::start(Script::serving(
            feed_xml("https://cdn.example.org/1.mp3").into_bytes(),
        ));
        succeeded(&subscribe(root.path(), &server)?)?;
        server.shutdown();

        let unknown = failed(&run_cli(root.path(), &["unsubscribe", "nosuch"])?)?;
        assert!(unknown.contains("unknown feed: nosuch"), "{unknown}");

        let removed = succeeded(&run_cli(root.path(), &["unsubscribe", "radio-t"])?)?;
        assert_eq!(removed, "radio-t: unsubscribed\n", "{removed}");
        assert!(std::fs::read_dir(feeds_dir(root.path()))?.next().is_none());

        let listing = succeeded(&run_cli(root.path(), &["feeds"])?)?;
        assert_eq!(listing.lines().count(), 1, "{listing}");
        Ok(())
    }
}
