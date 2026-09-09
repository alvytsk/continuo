# Test fixtures

Self-generated, no third-party content, no licensing constraints.

`sine.wav` — 0.5 s, 440 Hz, 44100 Hz, stereo, PCM s16le:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=0.5" \
      -ac 2 -c:a pcm_s16le sine.wav

`sine.mp3` and `sine.flac` are transcoded from `sine.wav` with
`-c:a libmp3lame -b:a 128k` and `-c:a flac` respectively.

MP3 and FLAC are present because M1 promises those formats. Testing WAV alone
would leave both promised codecs unexercised, and symphonia's `mp3` feature is
not enabled by default.

`sine-5s.flac` — 5 s, 440 Hz, 44100 Hz, stereo, FLAC from s16:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -sample_fmt s16 -c:a flac sine-5s.flac

The engine contract tests play a fixture, pause it, and then have to play on
past wherever the pause landed. How far that is depends on how full the PCM
ring was when the park took effect and on how far the harness clock ran before
the test thread next looked, both of which stretch under CPU contention. Half a
second of media leaves no room for that; five seconds leaves an order of
magnitude more than the worst drift observed. Tests that need a track to *end*
keep using `sine.flac`, which is short on purpose.

`sine-noxing.mp3` — 5 s, 440 Hz, 44100 Hz, stereo, MP3 with **no Xing/LAME
header**:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -c:a libmp3lame -b:a 64k -write_xing 0 sine-noxing.mp3

`-write_xing 0` is load-bearing, not cosmetic: ffmpeg's default mp3 mux writes
a Xing/LAME header carrying the frame count, which symphonia reads back as
`track.num_frames` — so a fixture regenerated with defaults would silently
self-declare a duration again. This is the one fixture here whose entire
purpose is *not* establishing a length: nothing in its container, and (served
without `Content-Length` or `Content-Range`) nothing in its transport, can
tell a reader how long it is. It exists to prove `Continuity::Unresolved` is
reachable at all — every other fixture here (WAV's `data` chunk size, FLAC's
STREAMINFO, an ordinary MP3's Xing header) self-declares a length one way or
another, which made that refusal path untestable before this fixture existed.

`sine-5s.mp3` and `sine-5s.wav` — 5 s, 440 Hz, 44100 Hz, stereo, the same
duration as `sine-5s.flac` in MP3 and WAV:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -c:a libmp3lame -b:a 128k sine-5s.mp3
    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -c:a pcm_s16le sine-5s.wav

H1 needs all three promised formats to play through test output while the
body is still stalled mid-transfer, which takes the same five seconds of
headroom `sine-5s.flac` exists for above — half a second leaves no room to
stall after enough bytes have arrived to be audible and still have more
body left to stall.

`sine-5s.m4a` — 5 s, 440 Hz, 44100 Hz, mono, AAC in an ISO-BMFF container
whose `moov` atom sits after the audio data (a tail-`moov` layout, ffmpeg's
default placement with no `-movflags +faststart`):

    ffmpeg -f lavfi -i "sine=frequency=440:duration=5" -c:a aac -b:a 64k sine-5s.m4a

Generated with ffmpeg 9.0.1. §12's closing paragraph asks for evidence that a
tail-`moov` file can be *opened* over HTTP ranges at all: without byte
seeking, a demuxer can never reach the `moov` atom describing the stream, so
a sequential (range-less) source is expected to fail here where a
range-capable one succeeds. Verified empirically (top-level box layout below,
dumped by walking the file's box headers) that `moov` sits after `mdat`
rather than in front of it, so the fixture actually exercises that layout
rather than the `faststart` one ffmpeg produces when asked:

    ftyp @ 0 (28 B), free @ 28 (8 B), mdat @ 36 (40512 B), moov @ 40548 (1630 B)

`an_m4a_recording_opens_over_ranges` asserts `SeekSupport::Unknown`, not
`Native`: byte-range support is evidence the *transport* can seek, not that
this container's demuxer can — every remote source starts at `Unknown` and
is only promoted by a trial seek that actually lands (§6). The point of this
fixture is narrower than that promotion: it proves the tail-`moov` file opens
at all, which a sequential source cannot do.
