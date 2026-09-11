//! Feed bytes to parse-layer values (§4).
//!
//! [`parse_feed`] is subscription-independent on purpose: it takes bytes and
//! the URL they were retrieved from, and knows nothing about `FeedId`s, caches
//! or the network. That is what makes it testable against fixtures alone.
//!
//! Decoding is an explicit layer, never `NsReader` over raw bytes (§4.1):
//!
//! ```text
//! bytes -> quick_xml::encoding::DecodingReader -> NsReader<DecodingReader<&[u8]>>
//! ```
//!
//! `DecodingReader` transcodes with `decode_to_utf8_without_replacement`, so a
//! malformed byte becomes [`FeedError::Encoding`] rather than U+FFFD. A
//! silently mangled title is worse than a refusal that names its cause.

use std::io::ErrorKind;
use std::time::Duration;

use quick_xml::XmlVersion;
use quick_xml::encoding::DecodingReader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesDecl, BytesRef, BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::reader::NsReader;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;
use url::Url;

use super::error::FeedError;
use super::model::{Enclosure, ParsedFeed, ParsedItem};

/// The `itunes` namespace, the one extension this parser reads rather than
/// ignores (§4.3).
const ITUNES_NS: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";
/// The namespace the `xml` prefix is permanently bound to, so `xml:base`
/// is recognized by its expanded name rather than by a raw prefix (§4.4).
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// `quick_xml::encoding::DecodingReader`'s lookahead window, in **source**
/// bytes, minus any byte-order mark it strips.
///
/// `DecodingReader` reads this many bytes up front precisely so that
/// `set_encoding` can still be called after the declaration has been parsed;
/// once that buffer drains, `set_encoding` *asserts*. The value is a private
/// `PREFIX_CAP` constant in quick-xml 0.42 and is not exposed, so
/// [`declaration_fits_prefix`] is guarded by a characterization test —
/// `a_declaration_longer_than_the_decoder_window_is_refused_not_panicked` in
/// `tests/m4_feed_parse.rs` walks declaration lengths across the boundary —
/// rather than by trust.
const PREFIX_CAP: u64 = 64;

/// UTF-8 byte-order mark. `DecodingReader` strips it from its lookahead
/// window, shortening the window by exactly this many bytes.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The outcome of parsing one feed document.
///
/// `skipped` counts **parse-stage** item rejection only — an item whose
/// identity could not be decoded. Identityless and duplicate items are the
/// binding layer's concern, and a date that will not parse is neither (§4.6).
#[derive(Clone, Debug, PartialEq)]
pub struct ParseReport {
    pub feed: ParsedFeed,
    pub skipped: usize,
    pub warnings: Vec<ParseWarning>,
}

/// One non-fatal observation about a document.
///
/// `item` is the **1-based ordinal of the `<item>` element in document
/// order**, counted before anything is skipped, or `None` for a feed-level
/// observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseWarning {
    pub item: Option<usize>,
    pub kind: WarningKind,
}

/// What a [`ParseWarning`] is about.
///
/// These variants deliberately carry **no text**: no GUID, no URL, no title.
/// Redaction has to hold under `Debug` as well as `Display` (§7.2), and the
/// cheapest way to guarantee that is to give the payload nowhere to hide a
/// URL in the first place. The ordinal plus the category is enough to find
/// the item in the feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarningKind {
    /// An identity field contained an entity reference this parser cannot
    /// resolve, so the item was rejected (§4.8).
    UnknownIdentityEntity,
    /// A publication date would not parse. The item stays, with
    /// `published: None` (§4.6).
    InvalidDate,
    /// An enclosure's URL would not resolve, or its scheme is not HTTP(S),
    /// so that enclosure was discarded (§4.5).
    InvalidEnclosure,
    /// The item carried more than one `<enclosure>`; the first usable one in
    /// document order won and this many were ignored (§4.7).
    ExtraEnclosures { ignored: usize },
}

/// Parses one feed document into parse-layer values.
///
/// `retrieval_url` is the final URL after redirects, and is the base every
/// relative URL falls back to when no `xml:base` is in scope (§4.4).
///
/// # Errors
///
/// Feed-level faults only (§4.6): XML that will not parse, a decoding
/// failure, an unsupported document element, or a missing `<channel>`. Item
/// faults are counted in [`ParseReport::skipped`] instead.
pub fn parse_feed(bytes: &[u8], retrieval_url: &Url) -> Result<ParseReport, FeedError> {
    let mut reader = NsReader::from_reader(DecodingReader::new(bytes));
    // quick-xml 0.42 defaults are already what §4 asks for: no whitespace
    // trimming, end names checked, a dangling `&` refused. Nothing to set.
    let mut buf = Vec::new();
    let mut walker = Walker::new(retrieval_url);
    let mut at_start = true;

    loop {
        buf.clear();
        let event = match reader.read_event_into(&mut buf) {
            Ok(event) => event,
            Err(error) => return Err(map_xml_error(&error, reader.error_position())),
        };

        if at_start {
            at_start = false;
            if let Event::Decl(declaration) = &event {
                walker.version = apply_declaration(&mut reader, declaration, bytes)?;
                continue;
            }
        }

        match event {
            Event::Start(ref element) => walker.open(&reader, element)?,
            Event::Empty(ref element) => {
                walker.open(&reader, element)?;
                walker.close()?;
            }
            Event::End(_) => walker.close()?,
            Event::Text(ref text) => walker.push_text(&text.xml_content(walker.version)),
            Event::CData(ref data) => {
                // CDATA is already literal. Unescaping it a second time would
                // turn a title that legitimately reads `&amp;` into `&`.
                walker.push_text(data);
            }
            Event::GeneralRef(ref reference) => walker.push_reference(reference),
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) => {}
            // A DTD is never fetched and its entity declarations are never
            // expanded, so entities it declares stay unknown (§4.8).
            Event::DocType(_) => {}
            Event::Eof => break,
        }
    }

    walker.finish()
}

/// Honors the XML declaration's `encoding` attribute, if any (§4.1).
///
/// The declaration is inspected with [`BytesDecl::encoding`] rather than
/// [`BytesDecl::encoder`] because `encoder` chains `encoding()` -> `.ok()` ->
/// `Encoding::for_label` and collapses three different situations into one
/// `None`, which would send a perfectly valid `<?xml version="1.0"?>` down
/// the unsupported-encoding path. Once `encoding()` has told the four cases
/// apart, `encoder()` is what resolves the label — that keeps `encoding_rs`
/// out of this crate's direct dependencies.
fn apply_declaration(
    reader: &mut NsReader<DecodingReader<&[u8]>>,
    declaration: &BytesDecl<'_>,
    bytes: &[u8],
) -> Result<XmlVersion, FeedError> {
    let version = match declaration.version() {
        Ok(version) => match version.as_ref() {
            "1.0" => XmlVersion::Explicit1_0,
            "1.1" => XmlVersion::Explicit1_1,
            _ => return Err(malformed("XML declaration names an unknown version")),
        },
        Err(_) => return Err(malformed("XML declaration has no leading version")),
    };
    if let Some(Err(_)) = declaration.standalone() {
        return Err(malformed("invalid XML declaration"));
    }

    match declaration.encoding() {
        // No `encoding` attribute: keep whatever was detected from the BOM or
        // the declaration's byte pattern. Not an error.
        None => {}
        Some(Err(_)) => return Err(malformed("invalid XML declaration")),
        Some(Ok(label)) => {
            let label = label.into_owned();
            let Some(encoding) = declaration.encoder() else {
                return Err(FeedError::UnsupportedEncoding { label });
            };
            // Already decoding as the declared encoding — either it was
            // detected, or the label is a synonym for it. `set_encoding` would
            // be a no-op, and calling it is the one thing that could assert.
            if !encodings_agree(reader.get_ref().encoding().name(), encoding.name()) {
                if !declaration_fits_prefix(reader, bytes) {
                    return Err(malformed(
                        "XML declaration is too long for its encoding to be applied",
                    ));
                }
                reader.get_mut().set_encoding(encoding);
            }
        }
    }
    Ok(version)
}

/// Whether the declared label already describes how the stream is being
/// decoded, so that no switch is needed.
///
/// Names match for the ordinary cases. The exception is UTF-16: XML 1.0
/// §4.3.3 requires a UTF-16 entity to carry a byte-order mark and lets the
/// mark, not the label, settle the byte order, while `Encoding::for_label`
/// answers the bare label `UTF-16` with UTF-16LE. A big-endian document that
/// honestly declares `encoding="UTF-16"` is correct XML, and must not be
/// refused for disagreeing with a label that never carried the byte order in
/// the first place.
fn encodings_agree(detected: &str, declared: &str) -> bool {
    detected == declared || (detected.starts_with("UTF-16") && declared.starts_with("UTF-16"))
}

/// Whether `set_encoding` can still be called safely.
///
/// `DecodingReader` drains its lookahead window the moment the parser asks for
/// a byte past it, and `set_encoding` asserts once that has happened. Feed
/// bytes decide how long a declaration is, so this is checked rather than
/// assumed — a padded declaration must produce a typed error, never a panic.
///
/// Two conditions have to hold, and both are observable from outside the
/// crate:
///
/// * the stream is being decoded as UTF-8, so one decoded byte is one source
///   byte and `buffer_position()` counts source bytes directly. For UTF-16 the
///   window holds only half as many characters as bytes, and the position
///   would understate how far the parser has read.
/// * the declaration ended within the window, whose size shrinks by the length
///   of any BOM the reader stripped.
fn declaration_fits_prefix(reader: &NsReader<DecodingReader<&[u8]>>, bytes: &[u8]) -> bool {
    if reader.get_ref().encoding().name() != "UTF-8" {
        return false;
    }
    let stripped = if bytes.starts_with(&UTF8_BOM) {
        UTF8_BOM.len() as u64
    } else {
        0
    };
    reader.buffer_position() <= PREFIX_CAP - stripped
}

/// One open element. Owned, because the namespace resolver's view of a name
/// dies the moment the reader advances.
struct Frame {
    namespace: Option<String>,
    local: String,
    /// The `xml:base` in scope for this element, already resolved. `None`
    /// means nothing overrode the retrieval URL.
    base: Option<Url>,
    text: String,
    /// Whether text inside this element is worth keeping. Ignored extensions
    /// buffer nothing, so a huge one costs no memory.
    collect: bool,
    /// Whether the accumulated text contains an entity this parser could not
    /// resolve, and so cannot serve as an identity (§4.8).
    unknown_entity: bool,
}

impl Frame {
    fn is(&self, namespace: Option<&str>, local: &str) -> bool {
        self.namespace.as_deref() == namespace && self.local == local
    }
}

/// Where the walker is in the RSS 2.0 tree, derived from the whole open path
/// rather than from depth alone. An extension element with a matching local
/// name therefore cannot substitute for an RSS field: its parent chain does
/// not match.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
    /// Nothing is open yet; the next element is the document element.
    Document,
    /// Inside `rss`.
    Root,
    /// Inside `rss/channel`.
    Channel,
    /// Inside `rss/channel/item`.
    Item,
    /// Anywhere else, including everything below an unrecognized element.
    Ignored,
}

fn context(stack: &[Frame]) -> Context {
    match stack {
        [] => Context::Document,
        [root] if root.is(None, "rss") => Context::Root,
        [root, channel] if root.is(None, "rss") && channel.is(None, "channel") => Context::Channel,
        [root, channel, item]
            if root.is(None, "rss") && channel.is(None, "channel") && item.is(None, "item") =>
        {
            Context::Item
        }
        _ => Context::Ignored,
    }
}

/// One item under construction.
struct ItemBuilder {
    item: ParsedItem,
    /// Set when an identity field could not be decoded; the item is counted
    /// as skipped instead of retained.
    rejected: bool,
    /// Every `<enclosure>` seen, usable or not (§4.7).
    enclosure_count: usize,
}

struct Walker<'a> {
    retrieval: &'a Url,
    version: XmlVersion,
    stack: Vec<Frame>,
    feed: ParsedFeed,
    item: Option<ItemBuilder>,
    ordinal: usize,
    warnings: Vec<ParseWarning>,
    skipped: usize,
    saw_channel: bool,
    root_closed: bool,
}

impl<'a> Walker<'a> {
    fn new(retrieval: &'a Url) -> Self {
        Self {
            retrieval,
            version: XmlVersion::Implicit1_0,
            stack: Vec::new(),
            feed: ParsedFeed::default(),
            item: None,
            ordinal: 0,
            warnings: Vec::new(),
            skipped: 0,
            saw_channel: false,
            root_closed: false,
        }
    }

    /// The `xml:base` in scope, falling back to the retrieval URL (§4.4).
    fn base(&self) -> &Url {
        self.stack
            .iter()
            .rev()
            .find_map(|frame| frame.base.as_ref())
            .unwrap_or(self.retrieval)
    }

    fn warn(&mut self, kind: WarningKind) {
        let item = self.item.as_ref().map(|_| self.ordinal);
        self.warnings.push(ParseWarning { item, kind });
    }

    fn open(
        &mut self,
        reader: &NsReader<DecodingReader<&[u8]>>,
        element: &BytesStart<'_>,
    ) -> Result<(), FeedError> {
        let (namespace, local) = expanded(reader, element.name())?;
        let parent = context(&self.stack);

        if parent == Context::Document {
            if self.root_closed {
                return Err(malformed("content after the document element"));
            }
            // §4.2 dispatches on the *expanded* name. `{Atom}feed` joins this
            // match when the Atom mapping lands; until then every root but an
            // unqualified `rss` is refused by name rather than half-parsed.
            if namespace.is_some() || local != "rss" {
                return Err(FeedError::UnsupportedFormat);
            }
        }

        let base = self.element_base(reader, element)?;

        match (parent, namespace.as_deref(), local.as_str()) {
            (Context::Root, None, "channel") => self.saw_channel = true,
            (Context::Channel, None, "item") => {
                self.ordinal += 1;
                self.item = Some(ItemBuilder {
                    item: ParsedItem::default(),
                    rejected: false,
                    enclosure_count: 0,
                });
            }
            (Context::Item, None, "enclosure") => {
                // Attributes resolve against the base in scope *here*, which
                // includes this element's own `xml:base`.
                let effective = base.as_ref().unwrap_or_else(|| self.base()).clone();
                self.read_enclosure(element, &effective)?;
            }
            _ => {}
        }

        let collect = matches!(
            (parent, namespace.as_deref(), local.as_str()),
            (Context::Channel, None, "title" | "link")
                | (Context::Item, None, "guid" | "title" | "link" | "pubDate")
                | (Context::Item, Some(ITUNES_NS), "duration")
        );

        self.stack.push(Frame {
            namespace,
            local,
            base,
            text: String::new(),
            collect,
            unknown_entity: false,
        });
        Ok(())
    }

    /// This element's own `xml:base`, resolved against the base already in
    /// scope. `None` when the element does not set one, or sets one that will
    /// not resolve — in which case the enclosing base simply stays in force.
    fn element_base(
        &self,
        reader: &NsReader<DecodingReader<&[u8]>>,
        element: &BytesStart<'_>,
    ) -> Result<Option<Url>, FeedError> {
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|_| malformed("invalid attribute"))?;
            // An unprefixed attribute is in no namespace by definition, so
            // only a prefixed one is worth expanding.
            if !attribute.key.as_ref().contains(':') {
                continue;
            }
            let (namespace, local) = expanded_attribute(reader, attribute.key)?;
            if namespace.as_deref() == Some(XML_NS) && local == "base" {
                let Some(value) = normalized(&attribute, self.version) else {
                    return Ok(None);
                };
                return Ok(resolved_url(Some(self.base()), &value));
            }
        }
        Ok(None)
    }

    fn read_enclosure(&mut self, element: &BytesStart<'_>, base: &Url) -> Result<(), FeedError> {
        let mut href = None;
        let mut href_undecodable = false;
        let mut length = None;
        let mut mime_type = None;
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|_| malformed("invalid attribute"))?;
            // `enclosure`'s attributes are unprefixed, and an unprefixed
            // attribute is in no namespace: an extension cannot smuggle a
            // `foo:url` in here.
            let key = attribute.key.as_ref();
            if key.contains(':') {
                continue;
            }
            let value = normalized(&attribute, self.version);
            match key {
                "url" => match value {
                    Some(value) => href = Some(value),
                    None => href_undecodable = true,
                },
                "length" => length = value,
                "type" => mime_type = value,
                _ => {}
            }
        }

        let Some(builder) = self.item.as_mut() else {
            return Ok(());
        };
        builder.enclosure_count += 1;
        if builder.item.enclosure.is_some() {
            // Already have one; §4.7 keeps the first and counts the rest.
            return Ok(());
        }
        if href_undecodable {
            // An enclosure URL is an identity fallback, and an identity that
            // cannot be decoded consistently is not an identity (§4.8).
            builder.rejected = true;
            self.warn(WarningKind::UnknownIdentityEntity);
            return Ok(());
        }
        let url = href
            .as_deref()
            .and_then(|href| resolved_url(Some(base), href))
            .filter(|url| matches!(url.scheme(), "http" | "https"));
        match url {
            Some(url) => {
                let length = length.and_then(|raw| raw.trim().parse::<u64>().ok());
                let mime_type = mime_type
                    .map(|raw| raw.trim().to_owned())
                    .filter(|raw| !raw.is_empty());
                builder.item.enclosure = Some(Enclosure {
                    url,
                    length,
                    mime_type,
                });
            }
            // A bad `length` never reaches here: it yields `None`, not a
            // discarded enclosure.
            None => self.warn(WarningKind::InvalidEnclosure),
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str) {
        if let Some(frame) = self.stack.last_mut()
            && frame.collect
        {
            frame.text.push_str(text);
        }
    }

    fn push_reference(&mut self, reference: &BytesRef<'_>) {
        let resolved = if reference.is_char_ref() {
            reference
                .resolve_char_ref()
                .ok()
                .flatten()
                .map(String::from)
        } else {
            resolve_predefined_entity(reference).map(String::from)
        };
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        match resolved {
            Some(text) => {
                if frame.collect {
                    frame.text.push_str(&text);
                }
            }
            None => {
                // Unknown-entity passthrough is confined to display fields: the
                // literal text stays, and the flag stops an identity field from
                // ever being built out of it (§4.8).
                frame.unknown_entity = true;
                if frame.collect {
                    frame.text.push('&');
                    frame.text.push_str(reference);
                    frame.text.push(';');
                }
            }
        }
    }

    fn close(&mut self) -> Result<(), FeedError> {
        let Some(frame) = self.stack.pop() else {
            return Err(malformed("end tag without a matching start tag"));
        };
        let Frame {
            namespace,
            local,
            base,
            text,
            unknown_entity,
            ..
        } = frame;
        let parent = context(&self.stack);
        if parent == Context::Document {
            self.root_closed = true;
        }

        match (parent, namespace.as_deref(), local.as_str()) {
            (Context::Channel, None, "title") => {
                set_once(&mut self.feed.title, trimmed(&text));
            }
            (Context::Channel, None, "link") => {
                if unknown_entity {
                    self.warnings.push(ParseWarning {
                        item: None,
                        kind: WarningKind::UnknownIdentityEntity,
                    });
                } else {
                    let base = base.clone().unwrap_or_else(|| self.base().clone());
                    set_once(&mut self.feed.site_link, resolved_url(Some(&base), &text));
                }
            }
            (Context::Channel, None, "item") => self.finish_item(),
            (Context::Item, None, "guid") => {
                if unknown_entity {
                    self.reject_identity();
                } else if let Some(builder) = self.item.as_mut() {
                    // Stored exactly as assembled and decoded. Never trimmed:
                    // the surrounding whitespace is part of what the feed
                    // said, and identity may not be quietly rewritten.
                    set_once(&mut builder.item.guid, Some(text));
                }
            }
            (Context::Item, None, "title") => {
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.item.title, trimmed(&text));
                }
            }
            (Context::Item, None, "link") => {
                if unknown_entity {
                    self.reject_identity();
                } else {
                    let base = base.clone().unwrap_or_else(|| self.base().clone());
                    let link = resolved_url(Some(&base), &text);
                    if let Some(builder) = self.item.as_mut() {
                        set_once(&mut builder.item.link, link);
                    }
                }
            }
            (Context::Item, None, "pubDate") => {
                let parsed = parse_rfc2822(&text);
                let missing = self
                    .item
                    .as_ref()
                    .is_some_and(|builder| builder.item.published.is_none());
                if parsed.is_none() && missing && !text.trim().is_empty() {
                    self.warn(WarningKind::InvalidDate);
                }
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.item.published, parsed);
                }
            }
            (Context::Item, Some(ITUNES_NS), "duration") => {
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.item.declared_duration, parse_duration(&text));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn reject_identity(&mut self) {
        if let Some(builder) = self.item.as_mut() {
            builder.rejected = true;
        }
        self.warn(WarningKind::UnknownIdentityEntity);
    }

    fn finish_item(&mut self) {
        let Some(builder) = self.item.take() else {
            return;
        };
        if builder.enclosure_count > 1 {
            self.warnings.push(ParseWarning {
                item: Some(self.ordinal),
                kind: WarningKind::ExtraEnclosures {
                    ignored: builder.enclosure_count - 1,
                },
            });
        }
        if builder.rejected {
            tracing::warn!(
                item = self.ordinal,
                "skipping feed item: identity contains an entity that cannot be resolved"
            );
            self.skipped += 1;
            return;
        }
        self.feed.items.push(builder.item);
    }

    fn finish(self) -> Result<ParseReport, FeedError> {
        if !self.stack.is_empty() {
            return Err(malformed("document ended with elements still open"));
        }
        if !self.root_closed || !self.saw_channel {
            return Err(FeedError::UnsupportedFormat);
        }
        Ok(ParseReport {
            feed: self.feed,
            skipped: self.skipped,
            warnings: self.warnings,
        })
    }
}

/// Expands an element name, owning the result before the reader advances.
fn expanded(
    reader: &NsReader<DecodingReader<&[u8]>>,
    name: QName<'_>,
) -> Result<(Option<String>, String), FeedError> {
    let (resolved, local) = reader.resolver().resolve_element(name);
    Ok((own_namespace(resolved)?, local.as_ref().to_owned()))
}

/// Expands an attribute name. Unprefixed attributes are in no namespace,
/// which is why this cannot reuse [`expanded`].
fn expanded_attribute(
    reader: &NsReader<DecodingReader<&[u8]>>,
    name: QName<'_>,
) -> Result<(Option<String>, String), FeedError> {
    let (resolved, local) = reader.resolver().resolve_attribute(name);
    Ok((own_namespace(resolved)?, local.as_ref().to_owned()))
}

fn own_namespace(resolved: ResolveResult<'_>) -> Result<Option<String>, FeedError> {
    match resolved {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(namespace) => Ok(Some(namespace.as_ref().to_owned())),
        ResolveResult::Unknown(_) => Err(malformed("undeclared namespace prefix")),
    }
}

/// An attribute value with predefined entities expanded. `None` when it
/// contains a reference this parser will not resolve — custom DTD entities
/// are never expanded (§4.8).
fn normalized(attribute: &Attribute<'_>, version: XmlVersion) -> Option<String> {
    attribute
        .normalized_value_with(version, 1, resolve_predefined_entity)
        .ok()
        .map(|value| value.into_owned())
}

fn set_once<T>(slot: &mut Option<T>, value: Option<T>) {
    if slot.is_none() {
        *slot = value;
    }
}

fn trimmed(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn parse_rfc2822(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value.trim(), &Rfc2822).ok()
}

/// Resolves `value` against `base`, or parses it as absolute when there is no
/// base. A URL that will not resolve is absent, not fatal (§4.4).
fn resolved_url(base: Option<&Url>, value: &str) -> Option<Url> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match base {
        Some(base) => base.join(value).ok(),
        None => Url::parse(value).ok(),
    }
}

/// `itunes:duration` as `HH:MM:SS`, `MM:SS`, or bare seconds (§4.3).
///
/// Anything else is `None`: negatives, decimals, empty parts, four parts, a
/// seconds or minutes field that has a colon in front of it and is 60 or more,
/// and anything that overflows. Minutes in `MM:SS` may exceed 59 — a
/// 90-minute episode written `90:00` is a real thing feeds do.
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let mut parts = [0u64; 3];
    let mut count = 0usize;
    for part in value.split(':') {
        if count == parts.len() {
            return None;
        }
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        parts[count] = part.parse::<u64>().ok()?;
        count += 1;
    }

    let seconds = match count {
        0 => return None,
        1 => parts[0],
        2 => {
            if parts[1] >= 60 {
                return None;
            }
            parts[0].checked_mul(60)?.checked_add(parts[1])?
        }
        _ => {
            if parts[1] >= 60 || parts[2] >= 60 {
                return None;
            }
            parts[0]
                .checked_mul(3600)?
                .checked_add(parts[1].checked_mul(60)?)?
                .checked_add(parts[2])?
        }
    };
    Some(Duration::from_secs(seconds))
}

fn malformed(detail: &str) -> FeedError {
    FeedError::Malformed {
        detail: detail.to_owned(),
    }
}

/// Maps a quick-xml failure onto [`FeedError`].
///
/// The detail is a category and a byte offset, never a slice of the document:
/// a parse error is exactly the situation in which the bytes at fault are
/// most likely to be something a log should not repeat.
fn map_xml_error(error: &quick_xml::Error, position: u64) -> FeedError {
    let category = match error {
        quick_xml::Error::Encoding(_) => return FeedError::Encoding,
        quick_xml::Error::Io(io) if io.kind() == ErrorKind::InvalidData => {
            return FeedError::Encoding;
        }
        quick_xml::Error::Io(_) => "could not read the document",
        quick_xml::Error::Syntax(_) => "XML syntax error",
        quick_xml::Error::IllFormed(_) => "ill-formed XML",
        quick_xml::Error::InvalidAttr(_) => "invalid attribute",
        quick_xml::Error::Escape(_) => "invalid character or entity reference",
        quick_xml::Error::Namespace(_) => "invalid namespace declaration",
    };
    FeedError::Malformed {
        detail: format!("{category} at byte {position}"),
    }
}
