#![cfg(target_os = "linux")]

//! The M4 command surface (design doc §6.1–§6.5): argument parsing, the
//! five feed commands as whole processes, and the two `play` forms.
//!
//! The parse tests drive `Cli::try_parse_from` directly — no process, no
//! filesystem — so an arity or value-parser regression is reported as a
//! parse failure rather than as whatever the command would have done with
//! the wrong arguments.

#[path = "support/process.rs"]
mod process;

use clap::Parser;
use tenuto::cli::{Cli, CliCommand};

#[test]
fn play_selectors_are_positive_and_single_source_still_parses()
-> Result<(), Box<dyn std::error::Error>> {
    assert!(Cli::try_parse_from(["tenuto", "play", "file.mp3"]).is_ok());
    assert!(Cli::try_parse_from(["tenuto", "play", "radio-t", "3", "--probe-only"]).is_ok());
    assert!(Cli::try_parse_from(["tenuto", "play", "radio-t", "0"]).is_err());
    assert!(Cli::try_parse_from(["tenuto", "play", "radio-t", "newest"]).is_err());
    assert!(Cli::try_parse_from(["tenuto", "episodes", "radio-t", "-n", "0"]).is_err());
    let parsed = Cli::try_parse_from(["tenuto", "refresh"])?;
    assert!(matches!(
        parsed.command,
        Some(CliCommand::Refresh { slug: None })
    ));
    Ok(())
}

/// `--reverse` belongs to `episodes`, after the subcommand, and composes with
/// `-n` in either position.
#[test]
fn reverse_is_an_episodes_option_that_composes_with_the_limit()
-> Result<(), Box<dyn std::error::Error>> {
    for args in [
        vec!["tenuto", "episodes", "web-standarts", "--reverse"],
        vec!["tenuto", "episodes", "--reverse", "web-standarts"],
        vec![
            "tenuto",
            "episodes",
            "web-standarts",
            "--reverse",
            "-n",
            "5",
        ],
        vec![
            "tenuto",
            "episodes",
            "web-standarts",
            "-n",
            "5",
            "--reverse",
        ],
    ] {
        let parsed = Cli::try_parse_from(&args)?;
        assert!(
            matches!(
                parsed.command,
                Some(CliCommand::Episodes { reverse: true, .. })
            ),
            "{args:?}"
        );
    }
    let plain = Cli::try_parse_from(["tenuto", "episodes", "web-standarts"])?;
    assert!(matches!(
        plain.command,
        Some(CliCommand::Episodes { reverse: false, .. })
    ));
    assert!(Cli::try_parse_from(["tenuto", "--reverse", "episodes", "web-standarts"]).is_err());
    Ok(())
}

/// The two `play` forms are one and two positionals; a third is not a
/// spelling this CLI has, and accepting it silently would mean an ignored
/// argument rather than a reported mistake.
#[test]
fn play_takes_one_or_two_positionals_and_never_three() -> Result<(), Box<dyn std::error::Error>> {
    let one = Cli::try_parse_from(["tenuto", "play", "file.mp3"])?;
    assert!(matches!(
        one.command,
        Some(CliCommand::Play {
            index: None,
            probe_only: false,
            ..
        })
    ));

    let two = Cli::try_parse_from(["tenuto", "play", "radio-t", "3"])?;
    match two.command {
        Some(CliCommand::Play {
            source,
            index: Some(index),
            probe_only,
        }) => {
            assert_eq!(source, "radio-t");
            assert_eq!(index.get(), 3);
            assert!(!probe_only);
        }
        other => panic!("expected the two-positional play form, got {other:?}"),
    }

    assert!(Cli::try_parse_from(["tenuto", "play", "radio-t", "3", "extra"]).is_err());
    assert!(Cli::try_parse_from(["tenuto", "play"]).is_err());
    Ok(())
}

/// `--probe-only` belongs to both forms (§6.3): with two positionals it
/// applies after resolution rather than being refused at parse time.
#[test]
fn probe_only_parses_with_either_play_form() -> Result<(), Box<dyn std::error::Error>> {
    for args in [
        ["tenuto", "play", "file.mp3", "--probe-only"].as_slice(),
        ["tenuto", "play", "radio-t", "7", "--probe-only"].as_slice(),
    ] {
        let parsed = Cli::try_parse_from(args)?;
        assert!(
            matches!(
                parsed.command,
                Some(CliCommand::Play {
                    probe_only: true,
                    ..
                })
            ),
            "{args:?} must set probe_only"
        );
    }
    Ok(())
}

#[test]
fn subscribe_takes_a_url_and_an_optional_alias() -> Result<(), Box<dyn std::error::Error>> {
    let bare = Cli::try_parse_from(["tenuto", "subscribe", "https://example.org/feed"])?;
    match bare.command {
        Some(CliCommand::Subscribe { url, slug }) => {
            assert_eq!(url, "https://example.org/feed");
            assert_eq!(slug, None);
        }
        other => panic!("expected subscribe, got {other:?}"),
    }

    let aliased = Cli::try_parse_from([
        "tenuto",
        "subscribe",
        "https://example.org/feed",
        "--as",
        "radio-t",
    ])?;
    match aliased.command {
        Some(CliCommand::Subscribe { slug, .. }) => assert_eq!(slug.as_deref(), Some("radio-t")),
        other => panic!("expected subscribe, got {other:?}"),
    }

    assert!(Cli::try_parse_from(["tenuto", "subscribe"]).is_err());
    Ok(())
}

#[test]
fn unsubscribe_and_feeds_take_exactly_their_own_arguments() -> Result<(), Box<dyn std::error::Error>>
{
    let unsubscribe = Cli::try_parse_from(["tenuto", "unsubscribe", "radio-t"])?;
    assert!(matches!(
        unsubscribe.command,
        Some(CliCommand::Unsubscribe { ref slug }) if slug == "radio-t"
    ));
    assert!(Cli::try_parse_from(["tenuto", "unsubscribe"]).is_err());

    let feeds = Cli::try_parse_from(["tenuto", "feeds"])?;
    assert!(matches!(feeds.command, Some(CliCommand::Feeds)));
    assert!(Cli::try_parse_from(["tenuto", "feeds", "radio-t"]).is_err());
    Ok(())
}

#[test]
fn episodes_requires_a_slug_and_a_positive_limit() -> Result<(), Box<dyn std::error::Error>> {
    let bare = Cli::try_parse_from(["tenuto", "episodes", "radio-t"])?;
    assert!(matches!(
        bare.command,
        Some(CliCommand::Episodes {
            ref slug,
            limit: None,
            reverse: false,
        }) if slug == "radio-t"
    ));

    let limited = Cli::try_parse_from(["tenuto", "episodes", "radio-t", "-n", "5"])?;
    match limited.command {
        Some(CliCommand::Episodes { limit: Some(n), .. }) => assert_eq!(n.get(), 5),
        other => panic!("expected episodes with a limit, got {other:?}"),
    }

    assert!(Cli::try_parse_from(["tenuto", "episodes"]).is_err());
    assert!(Cli::try_parse_from(["tenuto", "episodes", "radio-t", "-n", "-1"]).is_err());
    assert!(Cli::try_parse_from(["tenuto", "episodes", "radio-t", "-n", "many"]).is_err());
    Ok(())
}

#[test]
fn refresh_takes_an_optional_slug() -> Result<(), Box<dyn std::error::Error>> {
    let all = Cli::try_parse_from(["tenuto", "refresh"])?;
    assert!(matches!(
        all.command,
        Some(CliCommand::Refresh { slug: None })
    ));

    let one = Cli::try_parse_from(["tenuto", "refresh", "radio-t"])?;
    assert!(matches!(
        one.command,
        Some(CliCommand::Refresh { slug: Some(ref slug) }) if slug == "radio-t"
    ));

    assert!(Cli::try_parse_from(["tenuto", "refresh", "radio-t", "extra"]).is_err());
    Ok(())
}

/// The rejection has to name the two spellings that do exist, or a mistyped
/// selector leaves the listener guessing which form they were meant to use.
#[test]
fn a_non_positive_index_explains_both_play_forms() -> Result<(), Box<dyn std::error::Error>> {
    let error = match Cli::try_parse_from(["tenuto", "play", "radio-t", "0"]) {
        Err(error) => error.to_string(),
        Ok(parsed) => panic!("index 0 must be rejected, got {:?}", parsed.command),
    };
    assert!(
        error.contains("expected a positive episode index"),
        "{error}"
    );
    assert!(error.contains("play <slug> <index>"), "{error}");

    let count = match Cli::try_parse_from(["tenuto", "episodes", "radio-t", "-n", "0"]) {
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
mod cli_process {
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Output, Stdio};
    use std::time::{Duration, Instant};

    use crate::support::server::{DocumentReply, Script, TestServer};

    type Fallible = Result<(), Box<dyn std::error::Error>>;

    /// How long a spawned child may take to finish once its server has been
    /// released. Generous enough not to trip under CPU contention, bounded so
    /// a wedged child fails the test rather than hanging the suite.
    const CHILD_PATIENCE: Duration = Duration::from_secs(20);

    fn command(root: &Path, args: &[&str]) -> Command {
        let mut command = super::process::command_in(root);
        command.args(args).env("RUST_LOG", "tenuto=warn");
        command
    }

    fn run_cli(root: &Path, args: &[&str]) -> std::io::Result<Output> {
        command(root, args).output()
    }

    /// Starts the CLI without waiting for it, so a test can act on the
    /// filesystem while the process is parked mid-request.
    fn spawn_cli(root: &Path, args: &[&str]) -> std::io::Result<Child> {
        command(root, args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }

    /// Waits for `child` with a deadline, killing it rather than blocking the
    /// suite forever if it never exits. Both pipes are small here (a handful
    /// of lines), so reading them after exit cannot deadlock.
    fn wait_bounded(mut child: Child) -> Result<Output, Box<dyn std::error::Error>> {
        let deadline = Instant::now() + CHILD_PATIENCE;
        loop {
            if child.try_wait()?.is_some() {
                return Ok(child.wait_with_output()?);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the spawned tenuto process never exited".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Rewrites the one subscription's `fetch_url` in place, keeping every
    /// other field — and therefore the file's §5.6 validity — exactly as
    /// `subscribe` wrote it. This is how a test points an existing
    /// subscription at a different loopback server without depending on a
    /// freed port being re-bindable.
    fn repoint(path: &Path, url: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let entry = file
            .get_mut("subscriptions")
            .and_then(|list| list.get_mut(0))
            .ok_or("the subscriptions file has no first record")?;
        entry["fetch_url"] = serde_json::json!(url);
        std::fs::write(path, serde_json::to_vec(&file)?)?;
        Ok(())
    }

    /// Puts a nonempty directory where a file is about to be written, which
    /// is what makes the atomic rename over it fail. Deliberately not a
    /// permission bit: a suite running as root would not be stopped by one,
    /// and a directory is refused by `rename(2)` for every user.
    fn obstruct(path: &Path) -> std::io::Result<()> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        std::fs::create_dir_all(path)?;
        std::fs::write(path.join("sentinel"), b"keep")
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
        root.join("data/tenuto/subscriptions.json")
    }

    fn feeds_dir(root: &Path) -> PathBuf {
        root.join("cache/tenuto/feeds")
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

        assert!(!root.path().join("data").exists());
        assert!(!root.path().join("cache").exists());
        assert!(!root.path().join("state").exists());

        // `refresh` over an empty snapshot is an empty, successful batch. As a
        // writer it takes `subscriptions.lock`, and that file is the only
        // thing it leaves behind: no subscriptions, no cache, no state.
        let batch = succeeded(&run_cli(root.path(), &["refresh"])?)?;
        assert!(batch.is_empty(), "{batch}");
        assert_eq!(
            files_under(&root.path().join("data")),
            ["subscriptions.lock"]
        );
        assert!(!root.path().join("cache").exists());
        assert!(!root.path().join("state").exists());
        Ok(())
    }

    /// Every regular file below `dir`, by name, sorted.
    fn files_under(dir: &std::path::Path) -> Vec<String> {
        let mut names = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    names.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        names.sort();
        names
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

        let state = root.path().join("state/tenuto/state.json");
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
        let requests = media.requests();
        media.shutdown();
        // The probe ran against the *enclosure*, which is the half that
        // distinguishes it from probing the feed URL.
        assert!(
            requests.iter().any(|request| request.path == "/audio.flac"),
            "the probe never opened the enclosure: {requests:?}"
        );

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

    // --- Step 3: episode probing and partial-failure exits ------------

    /// §6.4's "refused at resolution": an item with identity but no
    /// enclosure never becomes a request. The media server here is alive and
    /// answering for the whole test, so an empty request log is evidence
    /// that nothing was opened rather than evidence that nothing could be.
    #[test]
    fn an_episode_without_audio_never_reaches_the_media_server() -> Fallible {
        let media = TestServer::start(Script::from_fixture("sine-5s.flac"));
        let feed = TestServer::start(Script::serving(
            feed_xml(&media.url("/audio.flac")).into_bytes(),
        ));
        let root = tempfile::tempdir()?;
        succeeded(&subscribe(root.path(), &feed)?)?;
        feed.shutdown();

        // Episode 2 is `feed_xml`'s item with a guid and no enclosure.
        for args in [
            ["play", "radio-t", "2"].as_slice(),
            ["play", "radio-t", "2", "--probe-only"].as_slice(),
        ] {
            let error = failed(&run_cli(root.path(), args)?)?;
            assert!(
                error.contains("has no audio enclosure"),
                "{args:?}: {error}"
            );
        }

        let requests = media.requests();
        media.shutdown();
        assert!(
            requests.is_empty(),
            "a resolution failure must open nothing: {requests:?}"
        );
        // No device, no terminal, and no checkpoint: `NotPlayable` happens
        // before any of the three exist.
        assert!(!root.path().join("state").exists());
        Ok(())
    }

    /// §5.3/§6.4: `unsubscribe` removes the subscription first, so a cache
    /// it then cannot delete leaves the listener genuinely unsubscribed —
    /// and that half is committed text, printed before the nonzero status
    /// the incomplete cleanup earns.
    #[test]
    fn an_undeletable_cache_still_unsubscribes_and_exits_nonzero() -> Fallible {
        let root = tempfile::tempdir()?;
        let server = TestServer::start(Script::serving(
            feed_xml("https://cdn.example.org/1.mp3").into_bytes(),
        ));
        succeeded(&subscribe(root.path(), &server)?)?;
        server.shutdown();

        let cache = cache_file(root.path())?;
        obstruct(&cache)?;

        let output = run_cli(root.path(), &["unsubscribe", "radio-t"])?;
        let error = failed(&output)?;
        let committed = stdout(&output);
        assert!(
            committed.contains(
                "radio-t: the subscription was removed, but its cached \
                                episodes could not be deleted"
            ),
            "{committed}"
        );
        assert!(!error.is_empty(), "the failure must say why on stderr");

        // The subscription really is gone, and the obstruction is untouched.
        let listing = succeeded(&run_cli(root.path(), &["feeds"])?)?;
        assert_eq!(listing.lines().count(), 1, "{listing}");
        assert_eq!(std::fs::read(cache.join("sentinel"))?, b"keep");
        Ok(())
    }

    /// §5.3's subscribe commit order, as a whole process. The obstruction is
    /// created while the response is still withheld, so the cache write
    /// cannot possibly have started yet: the failure is injected rather than
    /// raced for. One park/release cycle only — the harness shares a single
    /// stall gate per server.
    #[test]
    fn a_subscription_that_cannot_be_saved_reports_that_nothing_is_subscribed() -> Fallible {
        let root = tempfile::tempdir()?;
        let server = TestServer::start(
            Script::documents(vec![reply(
                "/feed",
                vec![("Content-Type", "application/rss+xml")],
                feed_xml("https://cdn.example.org/1.mp3").as_bytes(),
            )])
            .stall_headers(),
        );

        let url = server.url("/feed");
        let child = spawn_cli(root.path(), &["subscribe", &url, "--as", "radio-t"])?;
        let stalled = server.wait_until_stalled(Duration::from_secs(10));
        if stalled {
            // `subscriptions.json` does not exist yet; a nonempty directory
            // in its place is what makes the eventual rename fail.
            obstruct(&subscriptions(root.path()))?;
        }
        let released = server.release();
        let output = wait_bounded(child);
        server.shutdown();

        assert!(stalled, "subscribe's request never reached the server");
        assert!(
            released,
            "the stalled connection was not parked as expected"
        );
        let output = output?;
        let error = failed(&output)?;
        let committed = stdout(&output);
        assert!(
            committed.contains(
                "episodes were cached, but the subscription itself \
                                could not be saved; nothing is subscribed"
            ),
            "{committed}"
        );
        assert!(!error.is_empty(), "the failure must say why on stderr");

        // Nothing is subscribed, and the cache file is left behind
        // unreferenced — recoverable, exactly as §5.3 describes.
        assert_eq!(
            std::fs::read(subscriptions(root.path()).join("sentinel"))?,
            b"keep"
        );
        assert!(cache_file(root.path()).is_ok());
        Ok(())
    }

    /// §5.3's refresh commit order: the cache lands first, so a subscription
    /// update that cannot be recorded afterwards is reported *with* the fact
    /// that the episodes were saved.
    ///
    /// Two independently bound servers, and the subscription's `fetch_url`
    /// repointed at the second between the two commands. The obvious
    /// alternative — shut the first server down and rebind its port — is
    /// what this test did first and it flaked under the full parallel suite
    /// with `Address already in use`: `SO_REUSEADDR` lets a port in
    /// `TIME_WAIT` be rebound, but it cannot win a race against another
    /// test's `bind_ephemeral` claiming the freed port first. Repointing the
    /// durable field instead touches no port at all, and is what a feed that
    /// moved would look like on disk anyway. Each server still parks at most
    /// once: the first never stalls, the second stalls exactly here.
    #[test]
    fn a_refresh_that_cannot_record_a_changed_title_reports_the_saved_cache() -> Fallible {
        let root = tempfile::tempdir()?;
        let first = TestServer::start(Script::serving(
            feed_xml("https://cdn.example.org/1.mp3").into_bytes(),
        ));
        succeeded(&subscribe(root.path(), &first)?)?;
        first.shutdown();

        let renamed = feed_xml("https://cdn.example.org/1.mp3")
            .replace("<title>Radio T</title>", "<title>Radio T Renamed</title>");
        let second = TestServer::start(
            Script::documents(vec![reply(
                "/feed",
                vec![("ETag", "\"v2\""), ("Content-Type", "application/rss+xml")],
                renamed.as_bytes(),
            )])
            .stall_headers(),
        );
        repoint(&subscriptions(root.path()), &second.url("/feed"))?;

        let child = spawn_cli(root.path(), &["refresh", "radio-t"])?;
        let stalled = second.wait_until_stalled(Duration::from_secs(10));
        if stalled {
            obstruct(&subscriptions(root.path()))?;
        }
        let released = second.release();
        let output = wait_bounded(child);
        second.shutdown();

        assert!(stalled, "refresh's request never reached the server");
        assert!(
            released,
            "the stalled connection was not parked as expected"
        );
        let output = output?;
        let error = failed(&output)?;
        let committed = stdout(&output);
        assert!(
            committed.contains("radio-t: updated, 2 episodes retained, 0 skipped"),
            "{committed}"
        );
        assert!(
            committed.contains(
                "radio-t: the episode cache was saved, but the feed's \
                                changed title could not be recorded"
            ),
            "{committed}"
        );
        assert!(!error.is_empty(), "the failure must say why on stderr");
        assert_eq!(
            std::fs::read(subscriptions(root.path()).join("sentinel"))?,
            b"keep"
        );
        Ok(())
    }

    /// §6.4: a batch prints every feed before its status is decided, and one
    /// bad feed neither hides the others nor exits zero. The count on stderr
    /// is `BatchIncomplete`'s, so it says how many of how many.
    #[test]
    fn a_refresh_batch_names_every_slug_and_counts_what_failed() -> Fallible {
        let root = tempfile::tempdir()?;
        let healthy = TestServer::start(Script::documents(vec![reply(
            "/feed",
            vec![("ETag", "\"v1\""), ("Content-Type", "application/rss+xml")],
            feed_xml("https://cdn.example.org/1.mp3").as_bytes(),
        )]));
        let doomed = TestServer::start(Script::serving(
            feed_xml("https://cdn.example.org/2.mp3").into_bytes(),
        ));

        succeeded(&run_cli(
            root.path(),
            &["subscribe", &healthy.url("/feed"), "--as", "radio-t"],
        )?)?;
        succeeded(&run_cli(
            root.path(),
            &["subscribe", &doomed.url("/feed"), "--as", "sysdesign"],
        )?)?;
        // Only the second feed's server goes away, so exactly one of the two
        // can fail.
        doomed.shutdown();

        let output = run_cli(root.path(), &["refresh"])?;
        let error = failed(&output)?;
        let listing = stdout(&output);
        healthy.shutdown();

        assert!(listing.contains("radio-t: unchanged"), "{listing}");
        assert!(listing.contains("sysdesign: failed:"), "{listing}");
        assert_eq!(listing.lines().count(), 2, "{listing}");
        assert!(
            error.contains("1 of 2 feeds did not complete successfully"),
            "{error}"
        );
        Ok(())
    }
}
