# Capture latency experiments — 2026-09-07

The starting executable was the installed release with SHA-256
`990528c8acf96eb306052a52d38de3cead8c860b62e02369c717e6981b75f560`.
A copy is preserved at `/tmp/snipchord-latency-lab/baseline`; its source snapshot
is `/tmp/snipchord-latency-lab/baseline-source.tar`. These temporary files are local
experiment inputs, not permanent distribution artifacts.

The live desktop is 5560×1920. The installed process's journal contained repeated
`native selection unavailable: the X11 keyboard is already grabbed` messages.
The old path discarded its native snapshot when the shortcut still owned the
keyboard, then attempted a more expensive client-side capture. A cursor-ready
marker alone did not reveal that failure.

## Measurement and decision rules

Use [the measurement procedure](latency-measurement.md) and `tests/latency_lab.py`.
Measure the same preserved release executables with the same screen size and
input sequence. Keep compilation and other benchmarks out of timed runs.

- Separate internal pointer readiness, server overlay mapping, injected input,
  and capture completion. A mapped window may immediately disappear on setup
  failure, so a map event alone is not a successful capture.
- Report actual trial counts, median, p95, missing signals, failures, and timeouts.
  Reset a failed selection before starting another trial; do not count ignored
  commands behind the same stuck selection as independent failures.
- Include ordinary warm capture and a foreign keyboard grab representing the
  desktop shortcut. Check drag, release, Esc, Space, and right-click cancellation.
- Keep screenshot pixels frozen at capture start. Preserve clipboard contents,
  file destinations, preview clicks, and output dimensions.
- Do not describe Xvfb event or pipe timestamps as physical keyboard-to-monitor
  latency. X11 server observation excludes the physical input and compositor
  presentation boundary. The earlier 1.8–4ms numbers described pointer readiness,
  not the complete visible interaction.

## Candidates

| Candidate | Change | Decision rationale |
| --- | --- | --- |
| Baseline | Early pointer grab, dimmed snapshot, abort on a busy keyboard | Reference implementation |
| Keyboard retry | Keep the native snapshot and pointer selection; acquire the keyboard on the event-loop timer | Avoid unnecessary fallback when shortcut modifiers are still held |
| 500ms timeout | Abort idle selection after 500ms; stop retries when dragging begins | Rejected in review: can cancel a legitimate selection and prevent Esc/Space after modifier release during a drag |
| No dimming | Show the bright frozen snapshot; remove the second full-screen pixmap and Render dimming work | Reduce setup work without switching to a live, changing screenshot |
| Reduced redraw | Avoid the redundant initial frame copy when the mapped window already shows the same frozen background | Reduce repeated full-screen work while retaining atomic outlined frames |
| Event-loop queue fix | Drain buffered X11 events after timer callbacks and before the raw socket wait | A synchronous keyboard-grab reply can queue input internally; socket readiness alone does not imply that queue is empty |

The first five-trial, older-polling-method comparison was exploratory: baseline
and keyboard-only selection visibility medians were both about 151ms; no-dim
was about 110ms. These figures are not mixed with the event-based measurements
and are not the final acceptance evidence.

## Recorded comparison

The first combined candidate builds to SHA-256
`5bb929491834d3c2920434a50e7ac78a7f51d05fe4b4d99ff53aaa5bd7f647bd`.
It combines keyboard retry, no dimming, reduced initial redraw, removal of a
redundant overlay stacking request, and the event-loop queue fix.

[Raw warm trials](benchmarks/latency-2026-09-07.json) use a private 5560×1920
JetBrains-bundled Xvfb server, 20 attempts per executable. Times are milliseconds.

| Metric | Baseline median / p95 | Candidate median / p95 |
| --- | --- | --- |
| Command launch → overlay MapNotify | 121.165 / 139.771 | 41.102 / 48.788 |
| Command launch → capture completion | 274.907 / 323.945 | 121.972 / 149.602 |
| Successful early drags | 15/20 | 20/20 |

Completion percentiles include only completed samples: the baseline has five
timeouts, which must be considered alongside its speed. Failed selections were
cancelled, or their isolated daemon restarted, before the next attempt. Other
exploratory runs also saw intermittent failures in candidate builds; this one
successful batch does not establish a zero failure rate.

[Separate held-drag pixel probes](benchmarks/latency-render-2026-09-07-final.json)
observed the selection border on the server in 4/5 baseline attempts and 5/5
candidate attempts. Gesture → observed border median / p95 was 101.014 / 118.208ms
and 64.540 / 70.518ms, respectively. This small sample is supporting evidence,
not a precise tail-latency estimate.

XFIXES cursor events were absent in these runs. The roughly 2ms internal pointer
marker is recorded separately and does not establish visible cursor latency.
MapNotify timing includes observer scheduling; synchronous observer queries can
also perturb subsequent injected input. Neither this table nor the pixel probe
measures physical shortcut-to-monitor latency.

## Independent X server check

[Modern Xvfb trials](benchmarks/latency-modern-xvfb-2026-09-07.json) repeat
20 attempts per condition using Ubuntu Xvfb 21.1.12, extracted under `/tmp`
without installing a system package. These trials retain intermittent failures
on both binaries, so the older bundled server cannot alone explain them.

| Condition | Baseline successes | First combined candidate successes |
| --- | --- | --- |
| Warm, early input | 19/20 | 17/20 |
| Warm, after map | 19/20 | 20/20 |
| Cold, early input | 19/20 | 18/20 |
| Cold, after map | 18/20 | 19/20 |
| Warm, keyboard held 150ms | 0/20 | 20/20 |
| Cold, keyboard held 150ms | 0/20 | 18/20 |

Keyboard-contention improvement is clear. Faster successful samples alone do
not settle the early-input failures; investigation must distinguish application
stalls from measurement or input-fixture failures before treating this as final
acceptance evidence. The complete functional smoke suite on this candidate
passed 21 checks, including a keyboard hold exceeding 710ms, right-click cancel,
Escape after release during a drag, genuine pointer-grab failure cleanup, frozen
pixels, clipboard formats, saved output, and preview opening.

## Feedback-loop correction: checked MapWindow wait

The intermittent failures were real application stalls, not missing output from
the measurement harness. A failed process still owned the selection grabs, had
received the injected Motion/Button events, and was blocked inside x11rb's
reply wait. Temporary boundary logging located the wait at `map_cookie.check()`.

In the locally used x11rb-protocol 0.13.2 implementation, an incoming event with
the same sequence as MapWindow can advance `next_reply_expected` to that sequence.
The checked-void preparation then does not insert a later reply request (its test
is `< sequence`), while its completion test requires `last_sequence_read > sequence`.
Without another request/reply this can wait indefinitely. An explicit
GetInputFocus reply queued after MapWindow now establishes the later sequence
before the original checked-map error validation. This query does not change focus.

A trace build passed 40 consecutive reproducer attempts. Temporary tracing was
then removed; the release SHA-256 is
`24d23c75235b90f889cc7e098f0c30e99e5b414cb4a9a136993e3e150c8dc870`.
Format, check, all 47 Rust tests, and release build passed. The old failed
measurements remain in this report to show why the first fast candidate was
not accepted solely on its timing results.

## Accepted release measurements

[Clean release trials](benchmarks/latency-accepted-2026-09-07.json) use the same
modern Xvfb, 5560×1920 geometry, and early-input procedure. All 60 attempts
completed with the expected 200×150 result. This follows a separate 30/30 clean
release reproducer; an earlier reproducer was interrupted after nine attempts
by an Xvfb connection reset and is not counted as a complete passing run.

| Condition (20 attempts each) | Successful captures | Launch → map median / p95 | Launch → completion median / p95 |
| --- | --- | --- | --- |
| Warm | 20/20 | 45.185 / 54.899ms | 139.760 / 163.522ms |
| Cold | 20/20 | 36.974 / 40.519ms | 147.330 / 172.919ms |
| Warm, keyboard held 150ms | 20/20 | 46.052 / 56.590ms | 147.995 / 184.208ms |

The same-server baseline warm run had map median / p95 of 105.442 / 164.965ms
and completion median / p95 of 234.115 / 350.406ms, with 19/20 successful
completions. The accepted warm map median is about 57% lower. These are sequential
local experiments, not a universal performance guarantee; scheduling and
compositor differences remain outside the measurement. No physical monitor
cursor latency is claimed.

Retain the three-condition 20-attempt script as the regression feedback loop:
record an immutable build hash, run the isolated matrix, reject stalls or output
regressions, compare median and p95, then run the functional smoke suite before
installing the selected build.

Final functional acceptance: standard and Render-disabled Xvfb suites each
passed 21/21 checks on the accepted hash, and Python compilation passed. The
verified release was installed to `~/.local/bin/snipchord` without changing
settings or shortcuts. The prior running clipboard owner was preserved; the
new executable becomes active after a user-initiated quit and next invocation.
