//! Design doc M5 §8–§9: local tags and the embedded front cover are read by
//! a container probe alone, on at most a few background workers whose jobs
//! are contained and cancellable.

#[path = "support/tagged_flac.rs"]
mod tagged_flac;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use continuo::application::enrich::{EnrichOutcome, MetadataWorkers, TagProbe};
use continuo::media::id::{AbsolutePath, MediaId};
use continuo::media::tags::{CoverBytes, LocalTags, probe_local_tags};

fn png_2x2() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 100, 50, 255]))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap_or_else(|error| panic!("png: {error}"));
    bytes
}

#[test]
fn tags_and_the_front_cover_are_read_without_decoding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = tagged_flac::tagged_flac(
        dir.path(),
        "Morning Tide",
        "Harbor",
        "Coast",
        Some(&png_2x2()),
    );
    let tags = probe_local_tags(&AbsolutePath::new(path).expect("abs")).expect("probe");
    assert_eq!(
        (
            tags.title.as_deref(),
            tags.artist.as_deref(),
            tags.album.as_deref()
        ),
        (Some("Morning Tide"), Some("Harbor"), Some("Coast"))
    );
    assert_eq!(tags.duration, Some(Duration::from_millis(500)));
    assert!(!tags.cover_oversized);
    assert!(
        tags.front_cover
            .is_some_and(|c| c.data.starts_with(b"\x89PNG"))
    );
}

#[test]
fn an_untagged_file_has_no_names_but_a_duration() {
    let path = AbsolutePath::new(
        std::fs::canonicalize(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sine.flac"
        ))
        .expect("fixture"),
    )
    .expect("abs");
    let tags = probe_local_tags(&path).expect("probe");
    assert_eq!(tags.title, None);
    assert_eq!(tags.duration, Some(Duration::from_millis(500)));
}

fn next_result(workers: &MetadataWorkers) -> continuo::application::enrich::EnrichResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = workers.try_result() {
            return result;
        }
        assert!(Instant::now() < deadline, "no enrichment result");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_panicking_probe_is_contained_and_the_worker_serves_the_next_job() {
    let panicked_once = Arc::new(AtomicBool::new(false));
    let flag = panicked_once.clone();
    let probe: TagProbe = Arc::new(move |_path| {
        if !flag.swap(true, Ordering::SeqCst) {
            panic!("injected decoder panic");
        }
        assert!(continuo::lifecycle::panic::in_contained_job());
        Ok(LocalTags {
            title: Some("ok".into()),
            ..LocalTags::default()
        })
    });
    let workers = MetadataWorkers::spawn(1, probe, continuo::lifecycle::hooks::TestHook::None);
    let path = AbsolutePath::new("/music/a.flac".into()).expect("abs");
    workers.request(MediaId::LocalFile(path.clone()), path.clone());
    assert!(matches!(
        next_result(&workers).outcome,
        EnrichOutcome::Panicked
    ));
    workers.request(MediaId::LocalFile(path.clone()), path);
    assert!(
        matches!(next_result(&workers).outcome, EnrichOutcome::Tags(t) if t.title.as_deref() == Some("ok"))
    );
}

#[test]
fn enrichment_results_do_not_carry_the_cover_bytes() {
    let probe: TagProbe = Arc::new(|_path| {
        Ok(LocalTags {
            title: Some("with art".into()),
            front_cover: Some(CoverBytes {
                data: vec![0; 4096],
                media_type: Some("image/png".into()),
            }),
            ..LocalTags::default()
        })
    });
    let workers = MetadataWorkers::spawn(1, probe, continuo::lifecycle::hooks::TestHook::None);
    let path = AbsolutePath::new("/music/a.flac".into()).expect("abs");
    workers.request(MediaId::LocalFile(path.clone()), path);
    match next_result(&workers).outcome {
        EnrichOutcome::Tags(tags) => {
            assert_eq!(tags.title.as_deref(), Some("with art"));
            assert!(tags.front_cover.is_none(), "artwork has its own loader");
        }
        other => panic!("tags expected: {other:?}"),
    }
}

#[test]
fn cancelled_results_are_discarded() {
    let probe: TagProbe = Arc::new(|path| {
        std::thread::sleep(Duration::from_millis(200));
        Ok(LocalTags {
            title: Some(path.as_path().display().to_string()),
            ..LocalTags::default()
        })
    });
    let workers = MetadataWorkers::spawn(2, probe, continuo::lifecycle::hooks::TestHook::None);
    let path = AbsolutePath::new("/music/a.flac".into()).expect("abs");
    workers.request(MediaId::LocalFile(path.clone()), path);
    workers.cancel_all();
    std::thread::sleep(Duration::from_millis(400));
    assert!(workers.try_result().is_none());

    // Positive control: without it, a request that never ran at all would
    // pass the check above just as well.
    let later = AbsolutePath::new("/music/b.flac".into()).expect("abs");
    workers.request(MediaId::LocalFile(later.clone()), later.clone());
    let result = next_result(&workers);
    assert_eq!(result.media, MediaId::LocalFile(later));
    assert!(
        matches!(result.outcome, EnrichOutcome::Tags(t) if t.title.as_deref() == Some("/music/b.flac"))
    );
}
