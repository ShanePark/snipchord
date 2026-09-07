# Capture latency measurement

Use `tests/latency_lab.py` for repeatable X11 phase measurements. It starts a
private Xvfb server for each candidate and condition, so the user's desktop,
clipboard, settings, and resident SnipChord process are not involved.

```sh
python3 tests/latency_lab.py \
  --candidate baseline=/tmp/snipchord-latency-lab/baseline \
  --candidate current=target/release/snipchord \
  --size 5560x1920 --size 1920x1080 \
  --condition warm --condition warm-mapped --condition cold --condition warm-contention \
  --iterations 20 \
  --json-out /tmp/snipchord-latency.json
```

Candidates run one after another. A warm run starts one resident daemon and
sends one region command per sample. A cold run starts a fresh capture process
for every sample. The contention run holds a foreign X11 keyboard grab for
150ms (change it with `--keyboard-hold-ms`) immediately before each command.
This is a controlled reproduction of the `keyboard is already grabbed` race;
it is useful for comparing implementations, but it does not model a specific
desktop compositor.

The plain `warm` and `cold` conditions send the XTEST drag as soon as the
internal marker is available (or after MapNotify for an older binary). They
are the early input correctness experiment. `warm-mapped` and `cold-mapped`
wait for the full-screen MapNotify before sending the drag; use them for a
stable overlay-display comparison when an implementation is intentionally
being tested for early-input behavior separately.

`warm-render` holds the first button press while moving to `(300,250)`, then
polls the overlay pixel at `(100,170)` until the white selection border is
observed with an X11 `GetImage` reply. It releases only after that server-side
pixel observation. This separates application redraw completion from
`MapNotify`; it still says nothing about compositor presentation or the
physical display.

Each raw sample contains these separate clocks:

| Field | Meaning |
| --- | --- |
| `launch_to_xfixes_cursor_notify_ms` | Process command launch to the XFIXES display-cursor event observed by the harness. |
| `launch_to_ready_marker_ms` | Launch to SnipChord's optional `selection_input_ready` diagnostic marker. This is an internal fallback signal. |
| `launch_to_overlay_map_ms` | Launch to the full-screen overlay `MapNotify`. The harness also records `overlay_unmap_notify`; a map that is immediately destroyed is a setup failure, not usable readiness. |
| `launch_to_gesture_ms` | Launch to the first XTEST motion/button request sent by the harness. |
| `gesture_to_capture_complete_ms` | XTEST gesture to `capture_complete`. A sample is successful only when the expected 200x150 capture completion is observed. |
| `gesture_to_server_border_pixel_ms` | For `warm-render`, motion to the first bright border pixel returned by X11 `GetImage`. |

The summary reports median and inclusive p95 for each phase, plus missing
values and successful/failed gesture counts. The JSON keeps every sample so a
candidate with a good median but intermittent setup failure cannot look like a
success.

XFIXES and MapNotify are server event delivery points. If XFIXES is selected
but the server emits no cursor event, the JSON explicitly falls back to the
internal marker and records zero XFIXES samples. Neither signal measures when
a compositor presents pixels on a monitor. The XTEST gesture removes
physical keyboard timing from the experiment as well. On a real desktop,
validate the remaining boundary manually: enable the desktop's existing
shortcut, start the observer on the same X11 display, and compare the cursor
change and overlay map events with the user's perceived response. Do not use a
global recorder for unrelated keys or content; restrict any live observation
to the SnipChord shortcut and stop it after a small sample.

The private Xvfb results are therefore suitable for regression comparisons
between builds and for locating work inside the application, while physical
presentation latency must remain an explicitly unmeasured quantity.
