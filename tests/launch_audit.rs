//! §12: no test may launch the binary against the developer's real profile.
//! The only file allowed to name the binary is the process helper.

use std::path::{Path, PathBuf};

mod support;
#[path = "support/process.rs"]
mod unsupported_process;

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_files(&path, found)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    Ok(())
}

#[test]
fn only_the_process_helper_names_the_binary() -> Result<(), Box<dyn std::error::Error>> {
    // Built with concat! so this file does not match its own search.
    let needle = concat!("CARGO_BIN_EXE_", "continuo");
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"),
        &mut files,
    )?;
    let offenders: Vec<_> = files
        .into_iter()
        .filter(|path| !path.ends_with("support/process.rs"))
        .filter(|path| std::fs::read_to_string(path).is_ok_and(|text| text.contains(needle)))
        .collect();
    assert!(
        offenders.is_empty(),
        "launch the binary through tests/support/process.rs: {offenders:?}"
    );
    Ok(())
}

#[test]
fn subprocess_suites_are_gated_to_verified_platforms() -> Result<(), Box<dyn std::error::Error>> {
    // Every test file that imports mod process; must be gated to Linux where
    // the isolation is verified.
    let process_import = concat!("#[path = \"support/process.rs\"]", "\nmod process;");
    let pty_import = concat!("mod ", "pty;");
    let linux_gate = "#![cfg(target_os = \"linux\")]";
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"),
        &mut files,
    )?;

    let offenders: Vec<_> = files
        .into_iter()
        .filter(|path| !path.ends_with("launch_audit.rs"))
        .filter_map(|path| {
            std::fs::read_to_string(&path).ok().and_then(|text| {
                if (text.contains(process_import) || text.contains(pty_import))
                    && !text.contains(linux_gate)
                {
                    Some(path)
                } else {
                    None
                }
            })
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "subprocess test files must gate with #![cfg(target_os = \"linux\")]: {offenders:?}"
    );

    // On non-Linux, verify that the helper panics before launching anything.
    #[cfg(not(target_os = "linux"))]
    {
        let result = std::panic::catch_unwind(|| unsupported_process::binary());
        assert!(
            result.is_err(),
            "binary() must panic on unsupported platforms to prevent accidental launches"
        );
    }

    // On Linux, verify profile isolation works: the binary writes
    // subscriptions and data under profile directories, not under the
    // inherited HOME. We test with subscriptions.json because no command
    // yet exercises a headless state.json write (that lands with a later
    // task's NullOutput virtual device); subscriptions.json is written by
    // the subscribe command today and resolves through the same
    // directories::ProjectDirs-based path logic, so it verifies the same
    // redirection mechanism.
    #[cfg(target_os = "linux")]
    {
        use support::server::{Script, TestServer};

        let profile = unsupported_process::Profile::new()?;
        let root = profile.root();

        // Create a test server serving a minimal RSS feed.
        let feed = TestServer::start(Script::serving(
            b"<rss><channel><title>Test</title></channel></rss>".to_vec(),
        ));
        let feed_url = feed.url("/feed");

        // Subscribe to the feed; this writes subscriptions.json under the
        // profile's data directory via the same ProjectDirs logic.
        let output = profile
            .command()
            .args(["subscribe", &feed_url, "--as", "test"])
            .output()?;

        feed.shutdown();

        assert!(
            output.status.success(),
            "subscribe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify that subscriptions.json was written to the isolated profile's
        // data directory, not to the inherited HOME.
        let subscriptions_file = root
            .join("data")
            .join("continuo")
            .join("subscriptions.json");

        assert!(
            subscriptions_file.exists(),
            "subscriptions.json not found at {}",
            subscriptions_file.display()
        );

        assert!(
            subscriptions_file.starts_with(root),
            "subscriptions.json {} should be under profile root {}, not HOME",
            subscriptions_file.display(),
            root.display()
        );

        // Also verify it's not under the inherited HOME.
        if let Ok(home) = std::env::var("HOME") {
            assert!(
                !subscriptions_file.starts_with(&home),
                "subscriptions.json {} should not be under HOME {}",
                subscriptions_file.display(),
                home
            );
        }
    }

    Ok(())
}
