# 0.1 validation — 2026-09-07

## Preview cache and selection contrast

Installed release SHA-256:
`3866dc8ef72d91dbddae8ece7b7e72bf89a36eb043c63e1d1093b1b70a149026`.
Thumbnail padding preserves all four image corners. Clipboard previews prepare
PNGs off the UI thread and retain the latest five private cache files. Explicit
save paths and clipboard contents are preserved. A dark under-stroke keeps the
white selection border visible on white content without full-screen dimming.

`cargo fmt --check`, `cargo check --locked`, `cargo test --locked` (49 passed),
strict Clippy, release build, and diff checks passed. The isolated preview
benchmark completed 40/40 samples: clipboard click-to-launcher median fell from
149.990ms to 4.916ms when the cached PNG was ready. Real viewer startup is outside
this metric. See [methods, results, and limitations](preview-improvements.md)
and [raw trials](benchmarks/preview-2026-09-07.json).

Full X11 smoke passed 22/22 checks at both 1024×768 and 5560×1920, including
exact latest-five retention across restart, saved-file/clipboard preservation,
thumbnail corners, and selection contrast. High-resolution redraw sampling
passed 16/16 after increasing collection time without relaxing assertions.

Installed with `python3 tools/install.py`; release and installed hashes match.
No keyboard shortcuts were changed. The running clipboard-owning process was
left alive to preserve its clipboard image. Quit it after using the current
clipboard, then invoke a shortcut to start the updated executable.

## No-dim latency experiments

Accepted release SHA-256:
`24d23c75235b90f889cc7e098f0c30e99e5b414cb4a9a136993e3e150c8dc870`.
The frozen selection background stays bright, redundant initial redraw and
stacking requests are removed, and a busy shortcut keyboard grab is retried
while pointer selection remains active. The event loop drains buffered X11
events after timer replies. A later reply barrier before the checked MapWindow
request fixes a reproduced x11rb sequence-related indefinite wait. Output and
preview contracts are unchanged.

See [experiment decisions and results](latency-experiments.md),
[measurement method](latency-measurement.md), and
[accepted raw trials](benchmarks/latency-accepted-2026-09-07.json).
The clean release passed 30 consecutive fast-input reproducer attempts, then
60/60 captures across warm, cold, and keyboard-contention conditions. Warm
launch-to-map median / p95 was 45.185 / 54.899ms versus baseline
105.442 / 164.965ms. These are server observations, not physical display latency.

Source verification: `cargo fmt --check`, `cargo check --locked`,
`cargo test --locked` (47 passed), and release build passed. Final source review
confirmed reply-barrier ordering, pixmap ownership/cleanup, keyboard retry
cleanup, and removal of temporary trace/error-suppression experiments.

Final functional verification on the accepted binary: standard Xvfb full smoke
21/21 passed; Render-disabled full smoke 21/21 passed; Python compile checks
passed. Render-disabled now validates extension independence, not forced
client-side fallback. Artifacts are `/tmp/snipchord-final-reply-barrier-smoke`
and `/tmp/snipchord-final-reply-barrier-no-render-smoke`.

Installed using `python3 tools/install.py`; installed and release hashes match.
Settings and shortcuts were preserved. The already-running prior process was
left alive because it owned CLIPBOARD and no CLIPBOARD_MANAGER was available;
this prevents losing the existing clipboard image. The installed release takes
effect after quitting the prior process and invoking SnipChord again.

## Early input and click cancellation follow-up

Installed and running binary SHA-256:
`990528c8acf96eb306052a52d38de3cead8c860b62e02369c717e6981b75f560`.
Region capture installs the root pointer grab and crosshair before configuration reads,
compositor settling, or snapshot preparation. Root-coordinate events remain valid through
native and client-side setup without transferring the grab to the overlay. Releasing a plain
click without a valid selection cancels capture without changing clipboard contents or files.

Verification: 47 Rust tests, format and strict Clippy checks, and full native and Render-disabled
Xvfb smoke suites passed. A direct XTEST drag on a 5560×1920 display was injected while the
selection overlay was not viewable and produced the correct 200×150 capture. The fallback suite
also captured input injected before overlay visibility. The click test restores its temporary
settings to prevent interference with later preferences tests.

The final binary's warm cursor/input-ready marker was measured directly with
`tests/benchmark_capture.py --metric cursor --iterations 5 --width 5560 --height 1920`:
median 2.13 ms, minimum 1.71 ms, maximum 4.07 ms. This measures command launch through confirmed
early pointer readiness on private Xvfb, not physical keyboard-to-display latency or full overlay
rendering. It is not compared as a percentage with earlier overlay-visibility measurements.
The previous installed binary is preserved at `/tmp/snipchord-before-early-input/baseline`
with SHA-256 `e31a762b9f40bf3af08fe5ae6b197e7c0ef6fac176a5377bd01a0fec25d887bc`.

## Minimal selection overlay follow-up

Installed and running release SHA-256:
`e31a762b9f40bf3af08fe5ae6b197e7c0ef6fac176a5377bd01a0fec25d887bc`.
All selection hints, coordinates, and dimension badges were removed. Idle pointer motion no
longer repaints the selection surface; unchanged window targets do not repaint either.
Render format negotiation is cached per connection, and native dimming uses one masked Render
pass instead of a copy followed by a black overlay. The compositor settling delay remains intact.

Verification: 46 Rust tests, format and strict Clippy checks, and complete private-Xvfb suites
with Render enabled and disabled passed. Idle, drag, and window-pick overlays had zero pixel
mismatches outside the expected selection border. The installed and running binaries matched.

An intermediate 177.85 ms measurement was not tied to a preserved executable; it must not be
used as a verified comparison with the previous installed `49186bcc` release. No verified
percentage improvement over that release is claimed. Earlier migration measurements below
describe their explicitly identified historical binaries.

## Previous feature release

## Current Rust validation

The current target is the Rust release binary at `target/release/snipchord`. Build and
integration checks ran on Ubuntu GNOME 46 with X11. The X11 smoke checks use a private Xvfb
server, temporary home/configuration directories, and an isolated clipboard; they do not touch
the live desktop or the user's clipboard. Python and GTK used by the smoke harness are test
dependencies only. The runtime binary has no Python or GTK runtime dependency.

The final release binary passed its Rust, installer, format, lint, and full private-Xvfb checks.
The final binary was installed through the managed user installer and the running resident process
was verified against the same binary. No live desktop capture or physical-keypress end-to-end
timing was performed.

## Final Rust evidence

- `cargo test --locked`: 46 Rust tests passed on the final source.
- Eleven Python installer tests passed on the final source.
- `cargo fmt --all -- --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed on the final source.
- Final release SHA-256: `49186bcc8accb6c64f16936ed268e246dfadaeb4013c977f20c83613e4c64627`.
- The exact-hash `python3 tests/rust_x11_smoke.py --binary target/release/snipchord` run passed
  on private Xvfb with native server-side capture and the Render-disabled fallback. It covered the
  immutable capture snapshot, a live Composite child, a clipped window, all four output paths and
  cancellation, and thumbnail focus/timeout behavior.
- The final thumbnail is image-only, rounded, clickable, has a 2px border, is capped at 220×140,
  and dismisses after four seconds.
- Region and full-desktop clipboard/file modes passed with `--clipboard` and `--save`; the
  display-free `--save-dir ~/Downloads` setting and configured save outputs passed.
- Space-before-drag window selection passed against a real X11 child. Composite is preferred for
  the selected window; when it is unavailable or unsuitable, especially for alpha-bearing window
  edges, the implementation uses the frozen visible root crop. The Render fallback passed in the
  isolated validation environment.
- `python3 tools/install.py --shortcuts` succeeded with the final binary. The existing region
  binding was reused and three bindings were added; eight total custom entries remained, including four
  unrelated entries, and the conflicting GNOME built-in screenshot binding was empty.
- The configured `--save-dir /home/shane/Downloads` path succeeded. After restarting the resident
  daemon, `/proc/<pid>/exe` resolved to the final binary with the same SHA-256.

## Latency observation

`python3 tests/benchmark_capture.py` is a reproducible benchmark for the warm resident path. On
the same private Xvfb server at 5560×1920 with five samples, it measured process spawn through the
new selection window reaching `Map State: IsViewable`:

- Baseline candidate `c7b2`: median 867.66 ms.
- Current candidate `1abb`: median 116.86 ms.
- Difference: approximately 86.5% lower median time.

The interval includes preview cleanup, root capture, conversion, and selection setup. It is a
same-server observation, not an end-to-end latency guarantee on a physical desktop, and it does
not establish Wine paste or compositor behavior. The audit-only final edge changes kept the same
startup path used for the `1abb` measurement.

## Earlier Rust baseline

Passed:

The following checks are retained from the earlier Rust baseline.

- `cargo build --release --locked` produced the release binary.
- `cargo test --locked`: 25 Rust unit tests passed.
- `cargo fmt --all -- --check` passed.
- `cargo clippy --all-targets --all-features -- -D warnings` passed.
- Six staged installer checks passed, covering the ELF release binary, `--version`, update,
  paths containing spaces, unmanaged-file protection, and `desktop-file-validate`.
- The stripped release binary is about 841 KiB (861,088 bytes) and dynamically depends only on
  `libc` and `libgcc_s`; it does not load GTK or Python at runtime.
- `python3 tests/rust_x11_smoke.py --binary target/release/snipchord` passed on the private Xvfb
  display for the capture, clipboard, demo, selection, preferences, preview, and recovery checks.
- The final rerun also restored focus to a sentinel client, kept preferences open across an
  unrelated X11 client message, made repeated Save idempotent (one file), and allowed
  selection → preferences → new capture at 200×150.
- An external keyboard-grab fault-injection run passed: the capture failed cleanly, the app
  released its pointer and instance resources, and a subsequent daemon start and `--quit` worked.
- Private Xvfb capture produced an exact 200×150 region crop from the controlled fixture. Both
  `image/png` and `image/bmp` clipboard targets preserved the expected dimensions and fixture
  pixels.
- Full-desktop capture at 1024×768 exposed both clipboard targets. The recorded encoded sizes
  were 3,146,804 bytes for PNG and 2,359,350 bytes for BMP. The large PNG transfer began with an
  `INCR` property and was drained completely.
- Demo mode left the sentinel clipboard unchanged. Escape cancelled a region capture, and Space
  movement preserved a 200×150 selection.
- The preferences checkbox persisted to the temporary settings file.
- The earlier Rust snapshot measured its native preview at 240×246 at `(760, 498)` in the isolated
  display, and its selection/preview PNGs were visually inspected. That snapshot predates the
  current image-only thumbnail contract.
- The selection overlay was visually inspected for the frozen frame, dimmed exterior, white edges,
  dimensions, and controls.

Earlier baseline memory observation:

- One idle sample on the same Xvfb server reported Rust at RSS 2,476 KiB / PSS 925 KiB and the
  Python baseline at RSS 43,116 KiB / PSS 19,833 KiB. This is an observational single sample,
  not a benchmark, and it does not measure end-to-end capture latency.

Not verified in this environment:

- Live desktop capture and physical keypress end-to-end timing; the latency figure above uses
  private Xvfb and command spawn through selection-window visibility.
- Direct paste into Wine KakaoTalk from the Rust application.
- Mixed-DPI monitors, rotated displays, hotplug, physical monitor layouts, and live compositor
  behavior.
- Login autostart in a new session; the final binary is installed, but no new login session was
  exercised.
- The user's actual XDG Pictures directory was not used by validation; the explicit Downloads
  destination was exercised successfully.
- Wayland, intentionally unsupported in 0.1.

## Historical Python baseline

The following records the previous Python/GTK implementation and is retained as a behavioral
reference. It is not validation evidence for the current Rust binary.

Environment: Ubuntu GNOME 46, X11, GTK 3.24, Python 3.12.

Passed:

- 12 unit/installation tests: four-direction selection, clamped movement,
  pixel scaling, invalid click rejection, settings round trip and invalid input,
  staged installation/update (including spaces in paths), unmanaged-file protection.
- `desktop-file-validate` on staged installed desktop entry.
- Python compilation and shell launcher syntax.
- Synthetic clipboard integration: PNG and BMP both decoded to the same 8×6 color image.
- Actual X11 capture of a controlled GTK color fixture: both clipboard targets decoded
  to exactly 200×120 pixels, with expected RGB (51, 153, 204).
- Visual inspection of the synthetic selection UI: frozen image, dimmed exterior,
  all four white edges, corner marks, dimensions, and help text.
- Visual inspection of the completed thumbnail: image, size, Save/Copy/Close controls.
- Space-drag movement completed with original selection dimensions (300×200).
- Escape cancellation, resident action routing, and application quit.

Timing observations are not a benchmark: demo warm invocation to cursor change was
20–106 ms in two samples. Cursor readiness and first rendered frame are distinct;
these measurements do not establish cold-start or real-desktop capture latency.

Not verified:

- Direct paste into Wine KakaoTalk from the Python app (MIME/pixel compatibility verified only).
- Mixed-DPI monitors, rotated displays, hotplug, and all monitor layouts.
- PNG Save button and auto-save on every filesystem/locale; actual user Pictures directory
  was not modified during testing.
- Login autostart in a new session; installer generated/tested in a temporary prefix only.
- Wayland, intentionally unsupported in 0.1.

The existing screenshot-area-for-wine script and Ctrl+Alt+Shift+4 binding were not
replaced. The historical default-prefix installation was updated to its tested Rust binary; no
remote repository was created.
