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
