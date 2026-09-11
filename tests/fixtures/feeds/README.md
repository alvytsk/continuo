# Feed fixtures

Self-generated, no third-party content. Every file here is **readable UTF-8**,
on purpose.

## Why so few files

The encoding matrix (§4.1) is exercised from `tests/m4_feed_parse.rs`, which
builds each variant's bytes in the test itself: `utf16(..)` encodes a UTF-16
document with or without a byte-order mark, and `splice(..)` drops a raw byte
into an ASCII document. That is deliberate. A file committed as UTF-8 while
its declaration claims `ISO-8859-1` is not a Latin-1 fixture, it is a lie that
happens to be legible in an editor — and the one thing these tests must prove
is that the bytes on the wire decide. Generating the bytes also keeps a
runtime fixture-generation dependency out of the tree.

So what lives here is the RSS the decoder is not the point of.

## `rss2-minimal.xml`

One channel, one item, exercising the whole RSS 2.0 mapping at once: a title
carrying `&amp;`, a `guid` whose surrounding whitespace is load-bearing (it is
stored exactly as decoded and never trimmed), a **relative** `enclosure@url`
that has to resolve against the retrieval URL, an RFC 2822 `pubDate`, and an
`itunes:duration` in `HH:MM:SS` form.

## `decl-without-encoding.xml`

`<?xml version="1.0"?>` — a declaration with no `encoding` attribute, which is
the case §4.1 warns about: `BytesDecl::encoder()` answers `None` for it, the
same `None` it answers for a label nothing recognizes, and treating the two
alike would refuse a perfectly ordinary feed. The Cyrillic title and item
title are there so the file also proves the UTF-8 default actually decoded,
rather than merely not failing.

## The supported declaration bound

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
