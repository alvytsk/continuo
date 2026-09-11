//! Task 7 (§4.1-§4.8): RSS 2.0 parsing through strict decoding and
//! namespace-aware events.
//!
//! Encoded variants are built here rather than checked in as bytes. A fixture
//! file that claims to be Latin-1 while sitting on disk as UTF-8 proves
//! nothing; `utf16(...)` and `splice(...)` below produce the actual bytes.
//! See `tests/fixtures/feeds/README.md`.

use std::time::Duration;

use continuo::feed::error::FeedError;
use continuo::feed::parse::{ParseWarning, WarningKind, parse_feed};
use url::Url;

const RSS2_MINIMAL: &[u8] = include_bytes!("fixtures/feeds/rss2-minimal.xml");
const DECL_WITHOUT_ENCODING: &[u8] = include_bytes!("fixtures/feeds/decl-without-encoding.xml");

fn feed_url() -> Url {
    #[allow(clippy::unwrap_used)]
    Url::parse("https://example.org/feed.xml").unwrap()
}

/// Wraps a channel body in a minimal RSS 2.0 document.
fn rss(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<rss version=\"2.0\" \
         xmlns:i=\"http://www.itunes.com/dtds/podcast-1.0.dtd\" \
         xmlns:x=\"urn:example:extension\">\n<channel>{body}</channel>\n</rss>\n"
    )
}

/// UTF-16 bytes, with or without a byte-order mark.
fn utf16(xml: &str, little_endian: bool, bom: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    if bom {
        bytes.extend_from_slice(if little_endian {
            &[0xff, 0xfe]
        } else {
            &[0xfe, 0xff]
        });
    }
    for unit in xml.encode_utf16() {
        if little_endian {
            bytes.extend_from_slice(&unit.to_le_bytes());
        } else {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
    }
    bytes
}

/// Splices raw bytes into an ASCII document at the `@` placeholder, which is
/// how a document gets a byte no UTF-8 decoder will accept.
fn splice(ascii: &str, raw: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut parts = ascii.split('@');
    #[allow(clippy::unwrap_used)]
    bytes.extend_from_slice(parts.next().unwrap().as_bytes());
    for part in parts {
        bytes.extend_from_slice(raw);
        bytes.extend_from_slice(part.as_bytes());
    }
    bytes
}

// ---------------------------------------------------------------- structure

#[test]
fn rss_keeps_decoded_guid_whitespace() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(RSS2_MINIMAL, &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Radio & Friends"));
    assert_eq!(
        report.feed.items[0].guid.as_deref(),
        Some("  https://EXAMPLE.org/p?a=1&b=2  ")
    );
    assert_eq!(
        report.feed.items[0]
            .enclosure
            .as_ref()
            .map(|enclosure| enclosure.url.as_str()),
        Some("https://example.org/audio/one.mp3")
    );
    assert_eq!(
        report.feed.items[0].declared_duration,
        Some(Duration::from_secs(6120))
    );
    Ok(())
}

#[test]
fn rss_reports_channel_metadata_and_enclosure_details() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(RSS2_MINIMAL, &feed_url())?;
    assert_eq!(
        report.feed.site_link.as_ref().map(Url::as_str),
        Some("https://example.org/")
    );
    let enclosure = report.feed.items[0]
        .enclosure
        .as_ref()
        .ok_or("expected an enclosure")?;
    assert_eq!(enclosure.length, Some(42));
    assert_eq!(enclosure.mime_type.as_deref(), Some("audio/mpeg"));
    assert_eq!(report.feed.items[0].title.as_deref(), Some("Opening"));
    // 2026-09-06T18:00:00Z, the `pubDate` the fixture declares.
    assert_eq!(
        report.feed.items[0].published,
        Some(time::OffsetDateTime::from_unix_timestamp(1_788_717_600)?)
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn document_order_is_preserved_against_publication_dates() -> Result<(), Box<dyn std::error::Error>>
{
    let report = parse_feed(
        rss(concat!(
            "<item><guid>a</guid><pubDate>Sun, 06 Sep 2026 18:00:00 GMT</pubDate></item>",
            "<item><guid>b</guid><pubDate>Sat, 05 Sep 2026 18:00:00 GMT</pubDate></item>",
            "<item><guid>c</guid><pubDate>Mon, 07 Sep 2026 18:00:00 GMT</pubDate></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    let guids: Vec<_> = report
        .feed
        .items
        .iter()
        .filter_map(|item| item.guid.as_deref())
        .collect();
    assert_eq!(guids, ["a", "b", "c"]);
    Ok(())
}

#[test]
fn extension_elements_never_substitute_for_rss_fields() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<x:title>Extension channel title</x:title>",
            "<x:wrapper><title>Nested title</title><item><guid>nested</guid></item></x:wrapper>",
            "<title>Real channel title</title>",
            "<item><x:guid>extension guid</x:guid><guid>real guid</guid>",
            "<x:enclosure url=\"https://example.org/wrong.mp3\"/></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.title.as_deref(), Some("Real channel title"));
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("real guid"));
    assert_eq!(report.feed.items[0].enclosure, None);
    Ok(())
}

#[test]
fn deeply_nested_extensions_do_not_exhaust_the_stack() -> Result<(), Box<dyn std::error::Error>> {
    const DEPTH: usize = 20_000;
    let body = format!(
        "<x:deep>{}<title>buried</title>{}</x:deep><title>Real</title><item><guid>g</guid></item>",
        "<x:n>".repeat(DEPTH),
        "</x:n>".repeat(DEPTH),
    );
    let report = parse_feed(rss(&body).as_bytes(), &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Real"));
    assert_eq!(report.feed.items.len(), 1);
    Ok(())
}

#[test]
fn cdata_is_not_unescaped_a_second_time() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss("<title><![CDATA[Tom &amp; Jerry]]></title><item><guid><![CDATA[a&b]]></guid></item>")
            .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.title.as_deref(), Some("Tom &amp; Jerry"));
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("a&b"));
    Ok(())
}

// ---------------------------------------------------------- URLs, durations

#[test]
fn enclosures_resolve_against_the_innermost_xml_base() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item xml:base=\"https://cdn.example.net/season/\">",
            "<guid>a</guid><enclosure url=\"one.mp3\"/></item>",
            "<item xml:base=\"https://cdn.example.net/season/\">",
            "<guid>b</guid><enclosure xml:base=\"https://other.example.net/x/\" url=\"two.mp3\"/>",
            "<link>three.html</link></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(
        report.feed.items[0]
            .enclosure
            .as_ref()
            .map(|enclosure| enclosure.url.as_str()),
        Some("https://cdn.example.net/season/one.mp3")
    );
    assert_eq!(
        report.feed.items[1]
            .enclosure
            .as_ref()
            .map(|enclosure| enclosure.url.as_str()),
        Some("https://other.example.net/x/two.mp3")
    );
    assert_eq!(
        report.feed.items[1].link.as_ref().map(Url::as_str),
        Some("https://cdn.example.net/season/three.html")
    );
    Ok(())
}

#[test]
fn unusable_enclosures_are_discarded_but_the_item_stays() -> Result<(), Box<dyn std::error::Error>>
{
    let report = parse_feed(
        rss(concat!(
            "<item><guid>a</guid><enclosure url=\"ftp://example.org/one.mp3\"/></item>",
            "<item><guid>b</guid><enclosure url=\"http://[bad\"/></item>",
            "<item><guid>c</guid></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items.len(), 3);
    assert!(
        report
            .feed
            .items
            .iter()
            .all(|item| item.enclosure.is_none())
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(
        report.warnings,
        vec![
            ParseWarning {
                item: Some(1),
                kind: WarningKind::InvalidEnclosure
            },
            ParseWarning {
                item: Some(2),
                kind: WarningKind::InvalidEnclosure
            },
        ]
    );
    Ok(())
}

#[test]
fn a_bad_enclosure_length_yields_none_not_a_rejection() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item><guid>a</guid>",
            "<enclosure url=\"https://example.org/one.mp3\" length=\"-3\" type=\" \"/></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    let enclosure = report.feed.items[0]
        .enclosure
        .as_ref()
        .ok_or("expected the enclosure to survive a bad length")?;
    assert_eq!(enclosure.length, None);
    assert_eq!(enclosure.mime_type, None);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn the_first_usable_enclosure_wins_and_the_rest_are_counted()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item><guid>a</guid>",
            "<enclosure url=\"gopher://example.org/skipped.mp3\"/>",
            "<enclosure url=\"https://example.org/chosen.mp3\"/>",
            "<enclosure url=\"https://example.org/extra.mp3\"/></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(
        report.feed.items[0]
            .enclosure
            .as_ref()
            .map(|enclosure| enclosure.url.as_str()),
        Some("https://example.org/chosen.mp3")
    );
    assert_eq!(
        report.warnings,
        vec![
            ParseWarning {
                item: Some(1),
                kind: WarningKind::InvalidEnclosure
            },
            ParseWarning {
                item: Some(1),
                kind: WarningKind::ExtraEnclosures { ignored: 2 }
            },
        ]
    );
    Ok(())
}

#[test]
fn itunes_duration_accepts_three_forms_and_refuses_the_rest()
-> Result<(), Box<dyn std::error::Error>> {
    let accepted = [
        ("90", 90u64),
        ("0", 0),
        ("7:30", 450),
        ("90:00", 5400),
        ("1:42:00", 6120),
        ("  1:00:00  ", 3600),
        ("00:00:59", 59),
    ];
    let refused = [
        "",
        "-5",
        "1.5",
        "1:2:3:4",
        "1::2",
        ":30",
        "30:",
        "1:60",
        "1:60:00",
        "1:00:60",
        "one",
        "18446744073709551615:00",
        "1 : 2",
    ];

    let body: String = accepted
        .iter()
        .map(|(raw, _)| raw)
        .chain(refused.iter())
        .enumerate()
        .map(|(index, raw)| {
            format!("<item><guid>{index}</guid><i:duration>{raw}</i:duration></item>")
        })
        .collect();
    let report = parse_feed(rss(&body).as_bytes(), &feed_url())?;

    for (index, (raw, seconds)) in accepted.iter().enumerate() {
        assert_eq!(
            report.feed.items[index].declared_duration,
            Some(Duration::from_secs(*seconds)),
            "duration {raw:?} should parse"
        );
    }
    for (offset, raw) in refused.iter().enumerate() {
        let index = accepted.len() + offset;
        assert_eq!(
            report.feed.items[index].declared_duration, None,
            "duration {raw:?} should be refused"
        );
    }
    Ok(())
}

#[test]
fn an_unparsable_date_keeps_the_item_and_never_counts_as_skipped()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss("<item><guid>a</guid><pubDate>yesterday-ish</pubDate></item>").as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(report.feed.items[0].published, None);
    assert_eq!(report.skipped, 0);
    assert_eq!(
        report.warnings,
        vec![ParseWarning {
            item: Some(1),
            kind: WarningKind::InvalidDate
        }]
    );
    Ok(())
}

// -------------------------------------------------------------- identity

#[test]
fn an_unknown_entity_is_literal_in_a_title_and_fatal_in_an_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item><guid>kept</guid><title>Ten&nbsp;minutes</title></item>",
            "<item><guid>a&nbsp;b</guid><title>dropped</title></item>",
            "<item><guid>c</guid><link>https://example.org/&nbsp;</link></item>",
            "<item><guid>d</guid><enclosure url=\"https://example.org/&nbsp;.mp3\"/></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(
        report.feed.items[0].title.as_deref(),
        Some("Ten&nbsp;minutes")
    );
    assert_eq!(report.skipped, 3);
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| warning.kind == WarningKind::UnknownIdentityEntity),
        "{:?}",
        report.warnings
    );
    Ok(())
}

#[test]
fn numeric_character_references_resolve_everywhere() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss("<title>caf&#233;</title><item><guid>a&#x26;b</guid></item>").as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.title.as_deref(), Some("café"));
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("a&b"));
    assert_eq!(report.skipped, 0);
    Ok(())
}

#[test]
fn a_doctype_entity_declaration_is_never_expanded() -> Result<(), Box<dyn std::error::Error>> {
    let document = concat!(
        "<?xml version=\"1.0\"?>\n",
        "<!DOCTYPE rss [<!ENTITY secret \"leaked\">]>\n",
        "<rss version=\"2.0\"><channel><title>T&secret;</title>",
        "<item><guid>a</guid></item></channel></rss>",
    );
    let report = parse_feed(document.as_bytes(), &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("T&secret;"));
    Ok(())
}

#[test]
fn warning_payloads_carry_no_urls_or_guids_under_debug() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item><guid>urn:secret-guid:1</guid>",
            "<enclosure url=\"gopher://secret.example.net/a.mp3\"/>",
            "<enclosure url=\"https://secret.example.net/b.mp3\"/>",
            "<pubDate>not-a-date</pubDate></item>",
            "<item><guid>urn:secret-guid:&nbsp;</guid></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    let rendered = format!("{:?}", report.warnings);
    for leak in [
        "secret",
        "gopher",
        "https",
        "urn:",
        "example.net",
        "not-a-date",
    ] {
        assert!(
            !rendered.contains(leak),
            "warning debug output leaked {leak:?}: {rendered}"
        );
    }
    assert_eq!(report.warnings.len(), 4);
    Ok(())
}

// ------------------------------------------------------------- feed faults

#[test]
fn unsupported_document_elements_are_refused_by_name() {
    let cases: [&str; 3] = [
        "<?xml version=\"1.0\"?><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
         xmlns=\"http://purl.org/rss/1.0/\"><channel><title>T</title></channel></rdf:RDF>",
        "<?xml version=\"1.0\"?><feed xmlns=\"http://www.w3.org/2005/Atom\"><title>T</title></feed>",
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><title>no channel</title></rss>",
    ];
    for document in cases {
        assert!(
            matches!(
                parse_feed(document.as_bytes(), &feed_url()),
                Err(FeedError::UnsupportedFormat)
            ),
            "expected UnsupportedFormat for {document}"
        );
    }
}

#[test]
fn an_incomplete_document_is_malformed_even_after_a_good_item() {
    let cases = [
        // Truncated mid-document.
        rss("<item><guid>a</guid></item>").replace("</channel>\n</rss>\n", ""),
        // A complete item, then a suffix that never closes.
        rss("<item><guid>a</guid></item>").replace("</rss>\n", "<x:trailing>"),
        // Malformed XML inside an element this parser otherwise ignores.
        rss("<item><guid>a</guid></item><x:junk></x:mismatch>"),
        // A second document element after the first one closed.
        format!(
            "{}<rss version=\"2.0\"/>",
            rss("<item><guid>a</guid></item>")
        ),
    ];
    for document in cases {
        assert!(
            matches!(
                parse_feed(document.as_bytes(), &feed_url()),
                Err(FeedError::Malformed { .. })
            ),
            "expected Malformed for {document}"
        );
    }
}

#[test]
fn a_malformed_error_never_quotes_the_document() -> Result<(), Box<dyn std::error::Error>> {
    let document = rss("<item><guid>urn:secret-guid:1</guid></item>").replace("</rss>\n", "");
    let Err(error) = parse_feed(document.as_bytes(), &feed_url()) else {
        return Err("expected the truncated document to fail".into());
    };
    let rendered = format!("{error} / {error:?}");
    for leak in ["secret", "example.org", "guid"] {
        assert!(
            !rendered.contains(leak),
            "error output leaked {leak:?}: {rendered}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- encoding

#[test]
fn utf16_bom_is_decoded_strictly() -> Result<(), Box<dyn std::error::Error>> {
    let xml = "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>";
    let mut bytes = vec![0xff, 0xfe];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let report = parse_feed(&bytes, &"https://example.org/feed".parse()?)?;
    assert_eq!(report.feed.title.as_deref(), Some("Радио"));
    Ok(())
}

#[test]
fn every_utf16_framing_decodes_to_the_same_title() -> Result<(), Box<dyn std::error::Error>> {
    let undeclared = "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>";
    // The declaration is far longer than the reader's lookahead window once
    // encoded as UTF-16; the BOM (or the byte pattern) settles the encoding,
    // so nothing has to be switched.
    let declared = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><rss><channel><title>Радио</title></channel></rss>";
    let cases = [
        utf16(undeclared, true, true),
        utf16(undeclared, false, true),
        utf16(declared, true, false),
        utf16(declared, false, true),
    ];
    for bytes in cases {
        let report = parse_feed(&bytes, &feed_url())?;
        assert_eq!(report.feed.title.as_deref(), Some("Радио"));
    }
    Ok(())
}

#[test]
fn a_utf8_bom_is_stripped_with_and_without_a_declared_encoding()
-> Result<(), Box<dyn std::error::Error>> {
    let bodies = [
        "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>",
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><rss><channel><title>Радио</title></channel></rss>",
    ];
    for body in bodies {
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice(body.as_bytes());
        let report = parse_feed(&bytes, &feed_url())?;
        assert_eq!(report.feed.title.as_deref(), Some("Радио"));
    }
    Ok(())
}

#[test]
fn a_declared_single_byte_encoding_is_honored() -> Result<(), Box<dyn std::error::Error>> {
    let document = splice(
        "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\
         <rss version=\"2.0\"><channel><title>Caf@</title>\
         <item><guid>a</guid></item></channel></rss>",
        b"\xe9",
    );
    let report = parse_feed(&document, &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Café"));
    Ok(())
}

#[test]
fn malformed_bytes_are_refused_rather_than_replaced() -> Result<(), Box<dyn std::error::Error>> {
    // Once inside the reader's lookahead window, once well past it, so both
    // the prefix decode and the main decode path are exercised.
    let near = splice(
        "<?xml version=\"1.0\"?><rss><channel><title>@</title></channel></rss>",
        b"\xff",
    );
    let far = splice(
        &rss("<link>https://example.org/</link><item><guid>a</guid><title>@</title></item>"),
        b"\xff",
    );
    for document in [near, far] {
        match parse_feed(&document, &feed_url()) {
            Err(FeedError::Encoding) => {}
            other => return Err(format!("expected FeedError::Encoding, got {other:?}").into()),
        }
    }
    Ok(())
}

#[test]
fn nothing_is_ever_replaced_with_the_replacement_character()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        &utf16(
            "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>",
            true,
            true,
        ),
        &feed_url(),
    )?;
    let title = report.feed.title.unwrap_or_default();
    assert!(!title.contains('\u{fffd}'), "{title:?}");
    Ok(())
}

#[test]
fn an_absent_encoding_attribute_is_not_an_error() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(DECL_WITHOUT_ENCODING, &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Радио «Континуо»"));
    assert_eq!(report.feed.items[0].title.as_deref(), Some("Пролог"));
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn an_unrecognized_encoding_label_names_itself() -> Result<(), Box<dyn std::error::Error>> {
    let document = "<?xml version=\"1.0\" encoding=\"x-nonesuch\"?>\
                    <rss version=\"2.0\"><channel><title>T</title></channel></rss>";
    match parse_feed(document.as_bytes(), &feed_url()) {
        Err(FeedError::UnsupportedEncoding { label }) => assert_eq!(label, "x-nonesuch"),
        other => return Err(format!("expected UnsupportedEncoding, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_malformed_declaration_is_malformed_not_unsupported() -> Result<(), Box<dyn std::error::Error>>
{
    let cases = [
        // Unquoted encoding value: `encoding()` reports an attribute error.
        "<?xml version=\"1.0\" encoding=utf-8?>\
         <rss version=\"2.0\"><channel><title>T</title></channel></rss>",
        // No version at all.
        "<?xml encoding=\"utf-8\"?>\
         <rss version=\"2.0\"><channel><title>T</title></channel></rss>",
        // A version this parser does not know.
        "<?xml version=\"2.0\"?>\
         <rss version=\"2.0\"><channel><title>T</title></channel></rss>",
    ];
    for document in cases {
        match parse_feed(document.as_bytes(), &feed_url()) {
            Err(FeedError::Malformed { .. }) => {}
            other => return Err(format!("expected Malformed for {document}, got {other:?}").into()),
        }
    }
    Ok(())
}

/// Characterizes the bound this parser depends on rather than trusting it.
///
/// `DecodingReader` buffers 64 source bytes so that `set_encoding` can still
/// be called after the declaration has been parsed, and *asserts* once that
/// buffer drains. How long a declaration is, is feed-controlled, so the
/// parser checks the bound rather than assuming it. Past 64 bytes the
/// document is refused with a typed error and never a panic — as
/// `FeedError::Encoding` when the undecodable bytes are reached first, and as
/// `FeedError::Malformed` when the body is plain ASCII and the refused
/// encoding switch is the only thing wrong.
#[test]
fn a_declaration_longer_than_the_decoder_window_is_refused_not_panicked()
-> Result<(), Box<dyn std::error::Error>> {
    let latin1_body = "<rss version=\"2.0\"><channel><title>Caf@</title>\
                       <item><guid>a</guid></item></channel></rss>";
    let ascii_body = "<rss version=\"2.0\"><channel><title>Cafe</title>\
                      <item><guid>a</guid></item></channel></rss>";
    let shortest = "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>";
    assert_eq!(shortest.len(), 43);

    for padding in 0..=24 {
        let declaration =
            shortest.replace(" encoding=", &format!("{} encoding=", " ".repeat(padding)));
        let length = declaration.len();
        assert_eq!(length, 43 + padding);
        let within_window = length <= 64;

        let latin1 = splice(&format!("{declaration}{latin1_body}"), b"\xe9");
        match parse_feed(&latin1, &feed_url()) {
            Ok(report) if within_window => {
                assert_eq!(report.feed.title.as_deref(), Some("Café"));
            }
            Err(FeedError::Encoding) if !within_window => {}
            other => {
                return Err(
                    format!("{length}-byte declaration, Latin-1 body: got {other:?}").into(),
                );
            }
        }

        let ascii = format!("{declaration}{ascii_body}");
        match parse_feed(ascii.as_bytes(), &feed_url()) {
            Ok(report) if within_window => {
                assert_eq!(report.feed.title.as_deref(), Some("Cafe"));
            }
            Err(FeedError::Malformed { .. }) if !within_window => {}
            other => {
                return Err(format!("{length}-byte declaration, ASCII body: got {other:?}").into());
            }
        }
    }
    Ok(())
}
