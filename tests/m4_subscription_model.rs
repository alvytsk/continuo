use std::collections::BTreeSet;

use continuo::subscription::model::{choose_slug, new_feed_id, validate_feed_id, validate_slug};

#[test]
fn cyrillic_title_uses_host_and_suffix_stays_bounded() -> Result<(), Box<dyn std::error::Error>> {
    let url = url::Url::parse("https://www.radio-t.com/rss/")?;
    assert_eq!(
        choose_slug(Some("Радио-Т"), &url, None, &BTreeSet::new())?,
        "radio-t-com"
    );
    let full = "a".repeat(32);
    let used = BTreeSet::from([full.clone()]);
    let suffixed = choose_slug(Some(&full), &url, None, &used)?;
    assert_eq!(suffixed, format!("{}-2", "a".repeat(30)));
    validate_slug(&suffixed)?;
    assert!(validate_feed_id("../../state").is_err());
    Ok(())
}

#[test]
fn explicit_alias_is_rejected_on_collision_not_suffixed() -> Result<(), Box<dyn std::error::Error>>
{
    let url = url::Url::parse("https://example.org/feed")?;
    let occupied = BTreeSet::from(["taken".to_string()]);
    assert!(choose_slug(None, &url, Some("taken"), &occupied).is_err());
    // A free explicit alias is accepted verbatim, never suffixed.
    assert_eq!(
        choose_slug(None, &url, Some("free-slug"), &occupied)?,
        "free-slug"
    );
    Ok(())
}

#[test]
fn explicit_alias_pattern_is_validated_before_collision_check()
-> Result<(), Box<dyn std::error::Error>> {
    let url = url::Url::parse("https://example.org/feed")?;
    assert!(choose_slug(None, &url, Some("Not Valid!"), &BTreeSet::new()).is_err());
    assert!(choose_slug(None, &url, Some(""), &BTreeSet::new()).is_err());
    assert!(choose_slug(None, &url, Some(&"a".repeat(33)), &BTreeSet::new()).is_err());
    Ok(())
}

#[test]
fn leading_and_trailing_punctuation_collapses_and_trims() -> Result<(), Box<dyn std::error::Error>>
{
    let url = url::Url::parse("https://example.org/feed")?;
    let slug = choose_slug(Some("  Hello, World!  "), &url, None, &BTreeSet::new())?;
    assert_eq!(slug, "hello-world");
    Ok(())
}

#[test]
fn title_is_lowercased() -> Result<(), Box<dyn std::error::Error>> {
    let url = url::Url::parse("https://example.org/feed")?;
    let slug = choose_slug(Some("ABC123"), &url, None, &BTreeSet::new())?;
    assert_eq!(slug, "abc123");
    Ok(())
}

#[test]
fn missing_title_falls_back_to_host_without_www() -> Result<(), Box<dyn std::error::Error>> {
    let url = url::Url::parse("https://www.example.com/feed")?;
    let slug = choose_slug(None, &url, None, &BTreeSet::new())?;
    assert_eq!(slug, "example-com");
    Ok(())
}

#[test]
fn all_non_ascii_title_falls_back_to_host() -> Result<(), Box<dyn std::error::Error>> {
    let url = url::Url::parse("https://www.radio-t.com/rss/")?;
    let slug = choose_slug(Some("Радио Подкаст"), &url, None, &BTreeSet::new())?;
    assert_eq!(slug, "radio-t-com");
    Ok(())
}

#[test]
fn valid_32_hex_feed_id_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let id = validate_feed_id("0123456789abcdef0123456789abcdef")?;
    assert_eq!(id.as_str(), "0123456789abcdef0123456789abcdef");
    Ok(())
}

#[test]
fn feed_id_rejects_uppercase_and_wrong_length() -> Result<(), Box<dyn std::error::Error>> {
    assert!(validate_feed_id("0123456789ABCDEF0123456789abcdef").is_err());
    assert!(validate_feed_id(&"a".repeat(31)).is_err());
    assert!(validate_feed_id(&"a".repeat(33)).is_err());
    Ok(())
}

#[test]
fn generated_feed_ids_are_random_and_valid() -> Result<(), Box<dyn std::error::Error>> {
    let first = new_feed_id()?;
    let second = new_feed_id()?;
    assert_ne!(first, second);
    assert!(validate_feed_id(first.as_str()).is_ok());
    assert!(validate_feed_id(second.as_str()).is_ok());
    Ok(())
}

#[test]
fn validate_slug_accepts_and_rejects() -> Result<(), Box<dyn std::error::Error>> {
    validate_slug("radio-t")?;
    validate_slug(&"a".repeat(32))?;
    assert!(validate_slug("").is_err());
    assert!(validate_slug(&"a".repeat(33)).is_err());
    assert!(validate_slug("Radio-T").is_err());
    assert!(validate_slug("radio_t").is_err());
    Ok(())
}
