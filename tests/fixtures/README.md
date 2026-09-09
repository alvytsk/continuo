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
