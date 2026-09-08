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
