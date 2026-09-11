# Feed fixtures

Self-generated, no third-party content. Every file here is **readable UTF-8**,
on purpose.

## Index

Every committed fixture, and the one thing it exists to prove. Each has its own
section below with the full expected result.

| Fixture | Expected result |
|---|---|
| `rss2-minimal.xml` | The whole RSS 2.0 mapping: entity-bearing title, whitespace-preserving `guid`, relative enclosure, RFC 2822 date, `HH:MM:SS` duration |
| `xml-base-relative.xml` | Three nested `xml:base`es resolve; popping an item restores the channel's, not the sibling's |
| `cdata-title.xml` | CDATA is not unescaped a second time; `&amp;` outside it still decodes |
| `unknown-entity-in-title.xml` | `&nbsp;` survives literally in a title; nothing skipped, nothing warned |
| `unknown-entity-in-guid.xml` | The same entity in an identity skips **that item only**, and counts it |
| `itunes-duration-forms.xml` | `1:02:03`, `62:03`, `3723` all parse to 3,723 s; `1:60`, a negative and a `u64` overflow are `None` |
| `enclosure-malformed-with-guid.xml` | An unparseable `enclosure@url` is dropped with a warning; the item stays on its `guid` |
| `enclosure-scheme-unsupported.xml` | `file:`, `data:`, `ftp:` are discarded; the `https:` one wins |
| `multiple-enclosures.xml` | The first *usable* enclosure in document order wins; the other three are counted |
| `atom-minimal.xml` | The whole Atom 1.0 mapping, behind a non-`atom` namespace prefix |
| `atom-title-types.xml` | `text`, `html` (kept literal), `xhtml` (descendant text), and an absent `@type` (= `text`) |
| `atom-link-no-rel.xml` | A rel-less `link` is `alternate`; a `related` is passed over; the first alternate wins |
| `guid-absent-uses-enclosure.xml` | Identity falls through GUID → enclosure URL |
| `item-without-enclosure.xml` | Identity without playability: retained with `source: None` |
| `item-without-identity.xml` | Skipped with `WarningKind::MissingIdentity`; nothing is synthesized |
| `duplicate-identity.xml` | First occurrence wins; the duplicate and the identityless item are both skipped |
| `cyrillic-title.xml` | Cyrillic title, item title and `guid` preserved verbatim — no transliteration, case-folding or normalization |
| `decl-without-encoding.xml` | A declaration with no `encoding` attribute is not an error; UTF-8 is the default |
| `rss1-rdf.xml` | `FeedError::UnsupportedFormat` — refused by name |
| `truncated-xml.xml` | `FeedError::Malformed` for the whole document, however much of it parsed |

## Why the encoding matrix is not here

Spec §8.1 names `latin1-declared`, `utf16le-bom`, `utf16be-bom`,
`utf16le-declared-no-bom`, `bom-utf8`, `utf8-invalid-bytes`,
`encoding-unknown-label` and `decl-malformed-encoding-attr`. None of them is a
file. The encoding matrix (§4.1) is exercised from `tests/m4_feed_parse.rs`, which
builds each variant's bytes in the test itself: `utf16(..)` encodes a UTF-16
document with or without a byte-order mark, and `splice(..)` drops a raw byte
into an ASCII document. That is deliberate. A file committed as UTF-8 while
its declaration claims `ISO-8859-1` is not a Latin-1 fixture, it is a lie that
happens to be legible in an editor — and the one thing these tests must prove
is that the bytes on the wire decide. Generating the bytes also keeps a
runtime fixture-generation dependency out of the tree.

So what lives here is every fixture the decoder is not the point of: the
RSS 2.0 and Atom 1.0 mappings, `xml:base`, entities, and the two documents
that must be refused outright.

### The encoded variants, byte for byte

Each row is built by `tests/m4_feed_parse.rs`'s `utf16(xml, little_endian,
bom)` or `splice(ascii, raw)`. `utf16` writes an optional byte-order mark
(`FF FE` little-endian, `FE FF` big-endian) and then every UTF-16 code unit in
that byte order. `splice` replaces each `@` in an ASCII document with the raw
bytes given, which is how a document acquires a byte no UTF-8 decoder will
accept. The document body in every case is the minimal
`<rss><channel><title>…</title></channel></rss>`.

| Spec name (§8.1) | Byte construction | Expected result |
|---|---|---|
| `utf16le-bom` | `FF FE` + LE code units; declaration carries no `encoding` | `Ok`, title `Радио` |
| `utf16be-bom` | `FE FF` + BE code units; declaration carries no `encoding` | `Ok`, title `Радио` |
| `utf16le-declared-no-bom` | LE code units, **no** BOM; declaration says `encoding="UTF-16"` | `Ok`, title `Радио` — the `3C 00 3F 00` pattern settles byte order |
| (BE, declared, with BOM) | `FE FF` + BE code units; declaration says `encoding="UTF-16"` | `Ok`, title `Радио` — XML 1.0 §4.3.3 makes the mark, not the bare label, authoritative |
| `bom-utf8` | `EF BB BF` + the UTF-8 document, once with no `encoding` and once with `encoding="utf-8"` | `Ok` both ways, title `Радио`, BOM stripped |
| `latin1-declared` | `splice("<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>…<title>Caf@</title>…", b"\xe9")` — declaration 43 bytes, inside the window | `Ok`, title `Café` |
| `utf8-invalid-bytes` | `splice(doc, b"\xff")`, once with the byte inside the 64-byte lookahead window and once far past it | `FeedError::Encoding` both times — never a U+FFFD replacement |
| `encoding-unknown-label` | `encoding="x-nonesuch"`, ASCII body | `FeedError::UnsupportedEncoding { label: "x-nonesuch" }` |
| `decl-malformed-encoding-attr` | three spellings: unquoted `encoding=utf-8`; no `version` at all; `version="2.0"` | `FeedError::Malformed` for each |

The UTF-16 rows are why the matrix is generated rather than committed: a file
stored as UTF-8 while claiming to be UTF-16 would prove the opposite of what
these assert, which is that the **bytes on the wire** decide.

## RSS 2.0

### `rss2-minimal.xml`

One channel, one item, exercising the whole RSS 2.0 mapping at once: a title
carrying `&amp;`, a `guid` whose surrounding whitespace is load-bearing (it is
stored exactly as decoded and never trimmed), a **relative** `enclosure@url`
that has to resolve against the retrieval URL, an RFC 2822 `pubDate`, and an
`itunes:duration` in `HH:MM:SS` form.

### `xml-base-relative.xml`

Three `xml:base` attributes nested inside one another — on `rss`, on `channel`,
on the first `item` — so that resolving `one.mp3` has to walk all three. The
second item sets none, which is the half that is easy to get wrong: popping the
first item's frame has to put the *channel's* base back in force rather than
leave the sibling's behind.

### `cdata-title.xml`

A CDATA title beside a reference in ordinary text. `<![CDATA[A &amp; B]]>`
displays `A &amp; B` — CDATA is already literal, and unescaping the assembled
field a second time would silently rewrite it — while the `&amp;` standing
outside the CDATA in the item title still decodes to `&`.

### `unknown-entity-in-title.xml`

`&nbsp;` is not an XML entity and this parser has no HTML entity table (§4.8).
In a title it therefore survives as the literal text `&nbsp;`, while a standard
`&amp;` in the same string still decodes. Nothing is skipped and nothing is
warned about: a title is display-only.

### `unknown-entity-in-guid.xml`

The same entity in the same document, in a `guid` instead. An identity that
cannot be decoded consistently is not an identity, so that item is skipped and
counted — and the valid items on either side of it are kept, which is the point
of the item tier existing at all (§4.6).

### `itunes-duration-forms.xml`

`1:02:03`, `62:03` and `3723` are all 3,723 seconds: `MM:SS` minutes may exceed
59, because a 90-minute episode written `90:00` is a real thing feeds do. `1:60`
(a seconds field that is not a seconds field), a negative, and a value that
overflows `u64` are all `None`.

### `enclosure-malformed-with-guid.xml`

An `enclosure@url` that will not parse at all. The enclosure is discarded with a
warning and the item stays, because its `guid` still answers the identity
question (§4.5).

### `enclosure-scheme-unsupported.xml`

`file:`, `data:` and `ftp:` enclosures, then an `https:` one. Only `http(s)`
is a playback source; the other three resolve perfectly well as URLs and are
discarded anyway.

### `multiple-enclosures.xml`

Four enclosures on one item: an unusable scheme, the winner, and two more. The
first *usable* one in document order wins, and the warning counts the three
that lost — including the one that was never usable (§4.7).

## Atom 1.0

### `atom-minimal.xml`

The whole Atom mapping at once, with the namespace behind the prefix `a` to
prove the prefix is immaterial: a relative `xml:base` on `feed` that resolves
against the retrieval URL, a second one on `entry` that nests inside it, an
`id` whose surrounding whitespace is load-bearing, an xhtml title, a
`link[rel=enclosure]` with `@length` and `@type`, an RFC 3339 `published`, and
a feed-level `link` with no `rel` at all — which is an `alternate`, and so the
site link.

### `atom-title-types.xml`

The three values of `title@type` (§4.8), plus an absent one, which is `text`.
`text` is character content; `html` is decoded and then kept literally, markup
and all, because reading it properly needs an HTML parser this project is not
adding; `xhtml` is the concatenated descendant text of the wrapper `<div>`,
with the markup dropped and the whitespace at each markup boundary kept.

### `atom-link-no-rel.xml`

`link` with no `rel` is `alternate` (RFC 4287 §4.2.7.2), applied before any
selection. Neither entry has an `id` or an enclosure, so that rel-less link is
the only identity either one has. The second entry also shows a `related` link
being passed over and the first alternate beating a later one.

## Refused outright

### `rss1-rdf.xml`

RSS 1.0. Refused by name — `FeedError::UnsupportedFormat` — rather than
half-parsed: it is rare for podcasts, and pretending to support it is worse
than saying no (§4.2).

### `truncated-xml.xml`

A document that stops mid-element, after one complete and perfectly usable
item. A truncated document is a feed-level fault, and the tier it lands in is
not negotiated by how much of it happened to parse (§4.6). Committed without a
trailing newline, on purpose.

## Unicode

### `cyrillic-title.xml`

A Cyrillic channel title with guillemets and an em dash, a Cyrillic item title,
and a `guid` with Cyrillic in it. All three are preserved verbatim: the parser
does not transliterate, case-fold or normalize. Deriving an ASCII slug from a
title like this is the subscription layer's problem (§2.5).

## Declarations

### `decl-without-encoding.xml`

`<?xml version="1.0"?>` — a declaration with no `encoding` attribute, which is
the case §4.1 warns about: `BytesDecl::encoder()` answers `None` for it, the
same `None` it answers for a label nothing recognizes, and treating the two
alike would refuse a perfectly ordinary feed. The Cyrillic title and item
title are there so the file also proves the UTF-8 default actually decoded,
rather than merely not failing.

### The supported declaration bound

`quick_xml::encoding::DecodingReader` reads 64 source bytes ahead — three
fewer after a UTF-8 BOM — so that `set_encoding` can still be called once the
declaration has been parsed. Past that window the reader *asserts*, and
declaration length is feed-controlled, so the parser checks the bound before
switching and refuses the document instead:

| declaration | body | outcome |
|---|---|---|
| ≤ 64 bytes | anything | the declared encoding is applied |
| > 64 bytes | undecodable bytes | `FeedError::Encoding` |
| > 64 bytes | plain ASCII | `FeedError::Malformed` |

A declaration only needs switching when the stream is being decoded as UTF-8
and the label says otherwise. UTF-16 never gets there: the byte-order mark (or
the `3C 00 3F 00` pattern) already settled it, and XML 1.0 §4.3.3 makes the
mark rather than the label authoritative for byte order — so a big-endian
document declaring the bare label `UTF-16` is honored, not refused.

There is a third consequence of the same window, and it is not a refusal.
Those first 64 bytes are decoded under the **detected** encoding, and
`set_encoding` does not go back and re-decode them — so under a declared
legacy encoding, any non-ASCII byte *inside* the window is interpreted as
UTF-8 rather than as that encoding. This is unreachable for the formats this
parser accepts: an RSS or Atom document's first 64 bytes are the XML
declaration and the opening of the document element, which are ASCII by
construction, and a stray non-ASCII byte there would be invalid UTF-8 and
refused as `FeedError::Encoding` anyway. It is recorded because it is a
property of the decoder this parser depends on, not a property of these
fixtures, and a future format with a non-ASCII prologue would meet it.

## Episode binding

Used from `tests/m4_episode_binding.rs` (§2.4): plain UTF-8 RSS, nothing about
decoding, so — unlike the encoding matrix above — there is no reason for
these to be built in the test itself rather than committed as files.

### `guid-absent-uses-enclosure.xml`

One item with no `guid` at all and a usable `enclosure`. Identity falls
through to the enclosure URL.

### `item-without-enclosure.xml`

One item with a `guid` and no enclosure. Identity survives; the bound episode
is retained with `source: None` (§2.4 "identity without playability").

### `item-without-identity.xml`

One item with no `guid`, no enclosure and no link — no identity at all. It is
skipped with `WarningKind::MissingIdentity` (§2.4 "no synthesis").

### `duplicate-identity.xml`

Two items sharing the guid `same` — the second also carries a usable
enclosure the first lacks — plus a third item with no identity at all. The
first occurrence of `same` wins and is retained with `source: None`; the
second is skipped as a duplicate; the third is skipped as identityless
(§4.7).
