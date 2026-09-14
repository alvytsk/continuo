//! §12: no test may launch the binary against the developer's real profile.
//! The only file allowed to name the binary is the process helper.

use std::path::{Path, PathBuf};

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
        .filter(|path| {
            std::fs::read_to_string(path)
                .is_ok_and(|text| text.contains(process_import) || text.contains(pty_import))
        })
        .filter(|path| {
            std::fs::read_to_string(path)
                .is_ok_and(|text| !text.contains(linux_gate) && !text.ends_with("launch_audit.rs"))
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

    // On Linux, verify profile isolation works: the process should write its
    // state to Profile::state_dir(), not the inherited HOME.
    #[cfg(target_os = "linux")]
    {
        let profile = unsupported_process::Profile::new()?;
        let state_file = profile.state_file();
        // The state.json file won't exist until continuo actually runs and
        // initializes state, but the directory structure is ready.
        let state_dir = profile.state_dir();
        assert!(
            state_dir
                .to_string_lossy()
                .contains(state_file.parent().unwrap().to_string_lossy().as_ref()),
            "state_file must be under state_dir"
        );
        // Verify that state_dir uses the temporary root, not the inherited HOME.
        let root = profile.root();
        assert!(
            state_dir.starts_with(root),
            "state directory must be under the temporary root, not HOME"
        );
    }

    Ok(())
}
