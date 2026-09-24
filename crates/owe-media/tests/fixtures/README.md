# P4 media fixtures

Small committed media files, so the decode path is proven against real container
bytes rather than buffers a test built itself. Every fact the tests assert about
them — size, frame count, frame rate, duration, and that the three GIF frames are
three *different* pictures — comes from decoding these files through `owe-media`'s
public API, not from the recipe below.

Regenerate with `ffmpeg` (6.1 here). These are the commands that produced the
committed bytes, in order:

```sh
# still.png — 2×2, one frame.
ffmpeg -y -f lavfi -i color=c=red:s=2x2 -frames:v 1 still.png

# anim-3frame.gif — 32×32, three frames (red, green, blue) at 10 fps ⇒ 100 ms each.
ffmpeg -y -f lavfi -i color=c=red:s=32x32   -frames:v 1 f0.png
ffmpeg -y -f lavfi -i color=c=green:s=32x32 -frames:v 1 f1.png
ffmpeg -y -f lavfi -i color=c=blue:s=32x32  -frames:v 1 f2.png
ffmpeg -y -framerate 10 -i f%d.png \
  -vf "split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse" -loop 0 anim-3frame.gif

# video-5frame.mp4 — 64×48 H.264, five frames at 10 fps, 0.5 s.
ffmpeg -y -f lavfi -i testsrc=size=64x48:rate=10 -frames:v 5 \
  -c:v libx264 -pix_fmt yuv420p -g 1 -bf 0 -movflags +faststart video-5frame.mp4
```

Two details in those commands are deliberate. The MP4's `-g 1 -bf 0` leaves no
predictive frames, so each of the five is independently decodable and a pipeline that
drops one cannot be covered up by a neighbouring keyframe. And the GIF is red, green
and blue because the integration test asserts that its three frames differ in pixels —
a decoder handing out frame 0 three times would pass every shape assertion and fail
that one.

## Captured tool output (`video/`)

These are verbatim captures of the two probes the crate runs, kept because the
metadata parsers are pure functions over real tool output (the same
"capture, don't recollect" rule the Caelestia CLI surface is pinned with):

| File | Captured with | Why |
|---|---|---|
| `ffprobe-output.txt` | `ffprobe` with `probe::ffprobe_argv` against `video-5frame.mp4` | the only source of `nb_frames`; ffprobe reports a count, GStreamer does not |
| `gst-discoverer-output.txt` | `gst-discoverer-1.0 <file>` against the same file | the GStreamer half of the same facts, in a different syntax |
| `vaapi-failure.txt` | this machine's failed VA-API device initialisation | the evidence behind "software decode" on the reference profile, and why the probe refuses to trust an exit code: `ffmpeg -hwaccel vaapi` **exits 0** while decoding nothing in hardware (Haswell's `i965` driver is archived upstream) |

Re-capture the first two after regenerating `video-5frame.mp4`, or the parsers will
be tested against a file that no longer exists. The absolute paths inside the
captures are the recording machine's; nothing parses them.
