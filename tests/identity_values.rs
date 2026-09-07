use continuo::media::id::{AbsolutePath, EpisodeKey, FeedId, NormalizedUrl};
use std::path::PathBuf;
use url::Url;

#[test]
fn paths_validate_original_spelling_without_io() {
    let root = if cfg!(windows) { "C:/" } else { "/" };
    assert!(AbsolutePath::new(PathBuf::from(format!("{root}does-not-exist/episode.mp3"))).is_ok());
    for suffix in [
        "a/./episode.mp3",
        "a/../episode.mp3",
        "a/link/../episode.mp3",
        "a/.",
        "a/..",
        "a//episode.mp3",
        "a/episode.mp3/",
    ] {
        assert!(
            AbsolutePath::new(PathBuf::from(format!("{root}{suffix}"))).is_err(),
            "{suffix}"
        );
    }
    assert!(AbsolutePath::new(PathBuf::from(root)).is_ok());
    assert!(AbsolutePath::new(PathBuf::from(format!("{root}/episode.mp3"))).is_err());
    assert!(AbsolutePath::new(PathBuf::from("episode.mp3")).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_non_utf8_path() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    assert!(
        AbsolutePath::new(PathBuf::from(OsString::from_vec(
            b"/audio/\xff.mp3".to_vec()
        )))
        .is_err()
    );
}

#[test]
fn identity_url_normalizes_only_identity_fields() {
    let url = NormalizedUrl::parse("HTTPS://EXAMPLE.COM:443/a?b=2&a=%2f&a=%2F#part").unwrap();
    assert_eq!(url.as_str(), "https://example.com/a?b=2&a=%2f&a=%2F");
    assert!(NormalizedUrl::parse("not a url").is_err());
    assert!(NormalizedUrl::parse("file:///audio.mp3").is_err());
    assert!(FeedId::new(String::new()).is_err());
}

#[test]
fn episode_priority_and_opaque_guids() {
    let enclosure = Url::parse("https://example.com/audio.mp3#part").unwrap();
    let link = Url::parse("https://example.com/item").unwrap();
    let guid = " #/:? opaque GUID ";
    assert_eq!(
        EpisodeKey::resolve(Some(guid), Some(&enclosure), Some(&link)).unwrap(),
        EpisodeKey::resolve(Some(guid), None, None).unwrap()
    );
    assert_eq!(
        EpisodeKey::resolve(None, Some(&enclosure), Some(&link)).unwrap(),
        EpisodeKey::resolve(None, Some(&enclosure), None).unwrap()
    );
    assert_eq!(
        EpisodeKey::resolve(Some(""), None, Some(&link)).unwrap(),
        EpisodeKey::resolve(None, Some(&link), None).unwrap()
    );
    assert_ne!(
        EpisodeKey::resolve(Some(guid), None, None).unwrap(),
        EpisodeKey::resolve(Some(guid.trim()), None, None).unwrap()
    );
    assert_ne!(
        EpisodeKey::resolve(Some(enclosure.as_str()), None, None).unwrap(),
        EpisodeKey::resolve(None, Some(&enclosure), None).unwrap()
    );
    assert!(EpisodeKey::resolve(None, None, None).is_err());
}
