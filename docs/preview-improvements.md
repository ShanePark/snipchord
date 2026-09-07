# Preview and selection visibility — 2026-09-07

## Behavior

- Thumbnail border padding is 5px and its outer radius is 8px. The entire
  thumbnail image fits inside the rounded X11 window shape, including corners.
- Clipboard captures prepare a private PNG off the X11 event thread. Clicking
  a ready thumbnail reuses that file; an early click waits asynchronously for
  the image associated with that click. Explicit file saves open their existing
  file and do not create a second cache copy.
- Keep the latest five preview PNGs in `$XDG_CACHE_HOME/snipchord/previews`
  (default `~/.cache/snipchord/previews`). User-saved screenshots are excluded
  from rotation. Cache directory permissions are 0700 and files are 0600.
- Selection outlines combine a thin white line with a dark under-stroke, so
  they remain visible on both white and black content without full-screen
  dimming. These decorations are drawn only into the presentation frame and
  must not appear in captured pixels.

## macOS reference and limits

[Apple's screenshot guide](https://support.apple.com/en-asia/102646) includes
[an official region-selection example](https://cdsassets.apple.com/live/7WUAS350/images/macos/tahoe/macos-tahoe-screenshot-portion-of-screen.png).
Visual inspection shows a thin white outline and translucent selection tint.
The guide does not specify the exact rendering algorithm on a pure white
background. SnipChord uses an explicit dark/white contrast boundary to address
that case while retaining immediate, undimmed selection startup; this is not
a claim of pixel-identical reproduction of macOS's private implementation.

## Verification method

Use isolated Xvfb displays and a fake `xdg-open` executable that records the
request without launching a user's real viewer. Measure click injection to
viewer-stub invocation separately for saved and clipboard captures, and check
PNG contents, clipboard payloads, early clicks, and five-file rotation. This
measures application-side opening overhead, not a viewer's cold startup or
monitor presentation.

## Build and source review

The release under verification has SHA-256
`3866dc8ef72d91dbddae8ece7b7e72bf89a36eb043c63e1d1093b1b70a149026`.
`cargo fmt --check`, `cargo test --locked` (49 passed), strict Clippy, release
build, and `git diff --check` passed. Source review checked capture-generation
ordering, early clicks on completed-but-unpublished jobs, cache-only rotation,
worker shutdown, and preservation of the post-callback X11 event drain.

The thumbnail and selection changes do not add full-screen image processing.
Dark and white corner handles are each drawn in one batched request, keeping
selection decoration requests below the prior per-corner drawing count.

Full X11 smoke passed 22/22 checks at both 1024×768 and 5560×1920. These include
thumbnail corner pixels and shape containment, reuse of the prepared PNG,
unchanged clipboard data, exactly the newest five cache files surviving quit
and restart, saved-file preservation, and white/dark selection contrast with
clean captured pixels. The high-resolution redraw sampler initially collected
8 of the required 16 frames in six seconds; allowing 20 seconds above 2MP
produced 16/16 without changing sample or pixel acceptance criteria.

Visual artifacts are in `/tmp/snipchord-preview-final-smoke-1024` and
`/tmp/snipchord-preview-final-smoke-5560`. The release was installed with
`python3 tools/install.py`, with matching installed SHA-256 and no shortcut
changes. The previous clipboard-owning process remains alive until the user
quits it after using the current clipboard image.

## Measured opening overhead

[Raw measurements](benchmarks/preview-2026-09-07.json) compare the preserved
installed baseline (`24d23c7…`) with the release above on an isolated
5560×1920 X11 display. There are 10 successful samples per cell, 40/40 total.

| Output | Baseline median / p95 | Prepared-cache release median / p95 |
| --- | --- | --- |
| Saved file | 4.175 / 5.176ms | 4.878 / 8.675ms |
| Clipboard | 149.990 / 172.857ms | 4.916 / 7.229ms |

The prepared-cache condition waits for the PNG to be ready before the timed
click. PNG preparation remains real work (roughly 163ms after capture completion
in this large-image fixture); it is moved off the UI thread and ahead of the
click, not eliminated. An earlier click is queued until its PNG is available.
The stub records a wall-clock timestamp at invocation, using the same clock as
the test start. This avoids counting xdotool's default post-click delay in the
metric. Launcher/stub process overhead remains included; real viewer startup
and screen presentation are not measured.

Reproduce with:

```sh
python3 tests/preview_latency.py \
  --baseline /path/to/preserved-baseline \
  --candidate target/release/snipchord \
  --samples 10 --width 5560 --height 1920 \
  --output /tmp/preview-latency.json
```
