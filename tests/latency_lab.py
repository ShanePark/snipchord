#!/usr/bin/env python3
"""Measure SnipChord's capture phases on a private X11 server.

This is the event-oriented companion to ``benchmark_capture.py``.  It keeps
the X server and (for warm runs) the resident process alive while collecting
twenty or more samples.  The measurements intentionally separate signals
which are often conflated:

* ``xfixes_cursor_notify`` is an XFIXES server event.  It says that the
  server changed the pointer cursor; it is not proof that a monitor presented
  a new cursor image.
* ``selection_input_ready`` is SnipChord's optional internal marker, emitted
  after the early input grabs.  Older binaries simply have no marker.
* ``overlay_map_notify`` is the root SubstructureNotify event for the
  full-screen overlay.  A failed keyboard grab can map this window and then
  immediately destroy it, so this is recorded as a phase and never treated
  as a successful selection by itself.
* ``gesture_injected`` is the timestamp at which the harness sends the first
  XTEST motion/button request.  ``capture_complete`` is the application's
  completion line.  Their difference is the usable input/capture phase.

No phase measures compositor presentation or the physical keyboard-to-server
delay.  Those require a live desktop and hardware instrumentation.  Use this
script's ``--help`` output and the examples below to reproduce the private-X11
measurements while keeping that boundary explicit.

Examples::

    python3 tests/latency_lab.py \
      --candidate baseline=/tmp/snipchord-latency-lab/baseline \
      --candidate current=target/release/snipchord \
      --size 5560x1920 --size 1920x1080 \
      --condition warm --condition warm-contention \
      --iterations 20 --json-out /tmp/snipchord-latency.json

The candidates are run sequentially.  Every run gets a fresh Xvfb server and
private HOME/XDG directories; no live clipboard or desktop is touched.
"""

from __future__ import annotations

import argparse
import contextlib
from dataclasses import dataclass, field
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import select
import statistics
import subprocess
import sys
import threading
import time
from typing import Any, Mapping, Sequence

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))

import rust_x11_smoke as smoke  # noqa: E402


CAPTURE_RE = smoke.CAPTURE_RE
READY_RE = smoke.READY_RE
class LatencyError(RuntimeError):
    """A latency run could not satisfy its fixture contract."""


def _parse_size(value: str) -> tuple[int, int]:
    match = re.fullmatch(r"(\d+)x(\d+)", value.strip().lower())
    if not match:
        raise argparse.ArgumentTypeError("size must look like WIDTHxHEIGHT")
    width, height = (int(match.group(1)), int(match.group(2)))
    if width <= 0 or height <= 0 or width > 65535 or height > 65535:
        raise argparse.ArgumentTypeError("size must be between 1 and 65535 pixels")
    return width, height


def _parse_candidate(value: str) -> tuple[str, Path]:
    if "=" in value:
        name, raw_path = value.split("=", 1)
        name = name.strip()
    else:
        raw_path = value
        name = Path(value).name
    path = Path(raw_path).expanduser()
    if not name or not raw_path:
        raise argparse.ArgumentTypeError("candidate must look like NAME=PATH")
    if not path.is_file() or not os.access(path, os.X_OK):
        raise argparse.ArgumentTypeError(f"candidate is not executable: {path}")
    return name, path


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _tool_version(command: Sequence[str]) -> str | None:
    with contextlib.suppress(Exception):
        result = subprocess.run(
            list(command), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=3
        )
        text = result.stdout.decode(errors="replace").strip()
        return text.splitlines()[0] if text else None
    return None


def _percentile(values: Sequence[float], percentile: float) -> float | None:
    if not values:
        return None
    if len(values) == 1:
        return float(values[0])
    ordered = sorted(values)
    # ``inclusive`` is deterministic for small n and makes p95 explicit in
    # the JSON artifact instead of relying on a Python-version default.
    index = (len(ordered) - 1) * percentile / 100.0
    lower = int(index)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = index - lower
    return ordered[lower] + (ordered[upper] - ordered[lower]) * fraction


def _summary(samples: Sequence[dict[str, Any]], key: str) -> dict[str, Any]:
    values = [float(sample[key]) for sample in samples if sample.get(key) is not None]
    return {
        "n": len(values),
        "missing": len(samples) - len(values),
        "median_ms": round(statistics.median(values), 3) if values else None,
        "p95_ms": round(_percentile(values, 95.0), 3) if values else None,
        "min_ms": round(min(values), 3) if values else None,
        "max_ms": round(max(values), 3) if values else None,
    }


class LinePump:
    """Timestamp complete output lines without blocking the event observer."""

    def __init__(self, process: Any):
        self.process = process
        self.events: queue.Queue[tuple[int, str]] = queue.Queue()
        self.done = threading.Event()
        self.thread = threading.Thread(target=self._run, name="snipchord-output", daemon=True)
        self.thread.start()

    def _run(self) -> None:
        stream = self.process.stdout
        if stream is None:
            self.done.set()
            return
        try:
            for raw_line in iter(stream.readline, b""):
                self.events.put((time.perf_counter_ns(), raw_line.decode(errors="replace").rstrip()))
        finally:
            self.done.set()

    def drain(self) -> list[tuple[int, str]]:
        result: list[tuple[int, str]] = []
        while True:
            try:
                result.append(self.events.get_nowait())
            except queue.Empty:
                return result

    def close(self) -> None:
        self.thread.join(timeout=1)


class X11Observer:
    """Observe server-side cursor and root window events on a private display."""

    def __init__(self, env: Mapping[str, str], root_size: tuple[int, int]):
        try:
            from Xlib import X, display
            from Xlib.ext import xfixes
        except ImportError as error:  # pragma: no cover - depends on host image
            raise LatencyError("python-xlib with XFIXES support is required") from error
        self.X = X
        self.xfixes = xfixes
        self.connection = display.Display(env["DISPLAY"])
        self.root = self.connection.screen().root
        self.root_size = root_size
        self.root.change_attributes(event_mask=X.SubstructureNotifyMask)
        self.has_xfixes = "XFIXES" in self.connection.list_extensions()
        if self.has_xfixes:
            try:
                # XFIXES cursor notifications are not usable until the
                # version handshake has completed.  Without this reply some
                # Xvfb builds accept SelectCursorInput but never deliver the
                # event, which would silently turn every run into marker-only
                # data.
                self.connection.xfixes_query_version().reply()
                self.connection.xfixes_select_cursor_input(
                    self.root, xfixes.XFixesDisplayCursorNotifyMask
                )
                self.connection.sync()
            except Exception:
                # The server can advertise XFIXES without accepting this
                # request (old bundled Xvfb builds).  Marker fallback stays
                # honest and is reported in the artifact.
                self.has_xfixes = False
        self.connection.sync()

    def close(self) -> None:
        self.connection.close()

    def window_is_viewable(self, window_id: int) -> bool:
        """Confirm that a mapped candidate survived setup on the server."""
        try:
            window = self.connection.create_resource_object("window", window_id)
            return window.get_attributes().map_state == self.X.IsViewable
        except Exception:
            return False

    def drain(self) -> None:
        while self.connection.pending_events():
            self.connection.next_event()

    def fake_drag(self, start: tuple[int, int] = (100, 100), end: tuple[int, int] = (300, 250)) -> tuple[int, int]:
        """Send the same early gesture used by the correctness smoke test."""
        if not hasattr(self.connection, "xtest_fake_input"):
            raise LatencyError("XTEST extension is required for latency input")
        started = time.perf_counter_ns()
        root_id = self.root.id
        for event_type, detail, x, y in (
            (self.X.MotionNotify, 0, start[0], start[1]),
            (self.X.ButtonPress, 1, start[0], start[1]),
            (self.X.MotionNotify, 0, end[0], end[1]),
            (self.X.ButtonRelease, 1, end[0], end[1]),
        ):
            self.connection.xtest_fake_input(
                event_type,
                detail=detail,
                root=root_id,
                x=x,
                y=y,
            )
        self.connection.sync()
        return started, time.perf_counter_ns()

    def read_pixel(self, window_id: int, point: tuple[int, int] = (100, 170)) -> bytes:
        """Read one server-side overlay pixel; this is not monitor presentation."""
        window = self.connection.create_resource_object("window", window_id)
        image = window.get_image(
            point[0], point[1], 1, 1, self.X.ZPixmap, 0xFFFFFFFF
        )
        return bytes(image.data)

    def begin_hold_drag(
        self,
        start: tuple[int, int] = (100, 100),
        end: tuple[int, int] = (300, 250),
    ) -> tuple[int, int]:
        """Press and move while holding button 1; the caller releases later."""
        if not hasattr(self.connection, "xtest_fake_input"):
            raise LatencyError("XTEST extension is required for render probing")
        started = time.perf_counter_ns()
        self.connection.xtest_fake_input(
            self.X.MotionNotify, detail=0, root=self.root.id, x=start[0], y=start[1]
        )
        self.connection.xtest_fake_input(
            self.X.ButtonPress, detail=1, root=self.root.id, x=start[0], y=start[1]
        )
        self.connection.xtest_fake_input(
            self.X.MotionNotify, detail=0, root=self.root.id, x=end[0], y=end[1]
        )
        self.connection.sync()
        return started, time.perf_counter_ns()

    def release_hold_drag(self, point: tuple[int, int] = (300, 250)) -> None:
        self.connection.xtest_fake_input(
            self.X.ButtonRelease,
            detail=1,
            root=self.root.id,
            x=point[0],
            y=point[1],
        )
        self.connection.sync()

    def poll_events(self, timeout: float = 0.0) -> list[dict[str, Any]]:
        """Return decoded events with observer receive timestamps."""
        if timeout > 0 and not self.connection.pending_events():
            readable, _, _ = select.select([self.connection.fileno()], [], [], timeout)
            if not readable:
                return []
        events: list[dict[str, Any]] = []
        while self.connection.pending_events():
            event = self.connection.next_event()
            now = time.perf_counter_ns()
            if isinstance(event, self.xfixes.DisplayCursorNotify):
                events.append(
                    {
                        "kind": "xfixes_cursor_notify",
                        "at_ns": now,
                        "x_server_time": int(getattr(event, "timestamp", 0)),
                        "cursor_serial": int(getattr(event, "cursor_serial", 0)),
                    }
                )
                continue
            if event.type not in (self.X.MapNotify, self.X.UnmapNotify):
                continue
            window = getattr(event, "window", None)
            if window is None:
                continue
            window_id = int(window.id)
            geometry: tuple[int, int] | None = None
            with contextlib.suppress(Exception):
                value = window.get_geometry()
                geometry = (int(value.width), int(value.height))
            if geometry != self.root_size:
                continue
            events.append(
                {
                    "kind": "overlay_map_notify"
                    if event.type == self.X.MapNotify
                    else "overlay_unmap_notify",
                    "at_ns": now,
                    "window_id": window_id,
                    "geometry": [geometry[0], geometry[1]],
                    "override_redirect": bool(getattr(event, "override", False)),
                }
            )
        return events

class KeyboardContention:
    """Hold a foreign X11 keyboard grab for a bounded, recorded interval."""

    def __init__(self, env: Mapping[str, str], hold_ms: int):
        try:
            from Xlib import X, display
        except ImportError as error:  # pragma: no cover
            raise LatencyError("python-xlib is required for contention fixture") from error
        self.X = X
        self.display = display.Display(env["DISPLAY"])
        self.root = self.display.screen().root
        self.hold_ms = hold_ms
        self.thread: threading.Thread | None = None
        self.grabbed = False
        self.started_ns: int | None = None
        self.released_ns: int | None = None
        self.closed = False

    def start(self) -> None:
        status = self.root.grab_keyboard(
            owner_events=False,
            keyboard_mode=self.X.GrabModeAsync,
            pointer_mode=self.X.GrabModeAsync,
            time=self.X.CurrentTime,
        )
        self.display.sync()
        if status != self.X.GrabSuccess:
            self.display.close()
            raise LatencyError(f"contention fixture could not grab keyboard: {status}")
        self.grabbed = True
        self.started_ns = time.perf_counter_ns()
        self.thread = threading.Thread(target=self._release_later, daemon=True)
        self.thread.start()

    def _release_later(self) -> None:
        time.sleep(self.hold_ms / 1000.0)
        if self.grabbed:
            with contextlib.suppress(Exception):
                self.display.ungrab_keyboard(self.X.CurrentTime)
                self.display.flush()
            self.released_ns = time.perf_counter_ns()
            self.grabbed = False

    def close(self) -> None:
        if self.closed:
            return
        self.closed = True
        if self.grabbed:
            with contextlib.suppress(Exception):
                self.display.ungrab_keyboard(self.X.CurrentTime)
                self.display.flush()
            self.released_ns = time.perf_counter_ns()
            self.grabbed = False
        if self.thread is not None:
            self.thread.join(timeout=1)
        self.display.close()


@dataclass
class Sample:
    launch_ns: int
    command_return_ns: int | None = None
    cursor_notify_ns: int | None = None
    ready_marker_ns: int | None = None
    overlay_map_ns: int | None = None
    overlay_viewable_ns: int | None = None
    overlay_unmap_ns: int | None = None
    gesture_start_ns: int | None = None
    gesture_flush_ns: int | None = None
    capture_complete_ns: int | None = None
    cursor_signal: str | None = None
    overlay_signal: str | None = None
    overlay_window_id: int | None = None
    capture_dimensions: list[int] | None = None
    gesture_outcome: str = "not_sent"
    output_lines: list[str] = field(default_factory=list)
    contention_start_ns: int | None = None
    contention_release_ns: int | None = None
    render_probe_ns: int | None = None
    render_probe_status: str | None = None
    render_pixel_before: str | None = None
    render_pixel_after: str | None = None

    def finish(self) -> dict[str, Any]:
        values: dict[str, Any] = {
            "launch_to_command_return_ms": _delta(self.launch_ns, self.command_return_ns),
            "launch_to_cursor_signal_ms": _delta(
                self.launch_ns, self.cursor_notify_ns or self.ready_marker_ns
            ),
            "launch_to_xfixes_cursor_notify_ms": _delta(self.launch_ns, self.cursor_notify_ns),
            "launch_to_ready_marker_ms": _delta(self.launch_ns, self.ready_marker_ns),
            "launch_to_overlay_map_ms": _delta(self.launch_ns, self.overlay_map_ns),
            "launch_to_overlay_viewable_ms": _delta(
                self.launch_ns, self.overlay_viewable_ns
            ),
            "launch_to_gesture_ms": _delta(self.launch_ns, self.gesture_start_ns),
            "launch_to_capture_complete_ms": _delta(self.launch_ns, self.capture_complete_ns),
            "overlay_map_to_gesture_ms": _delta(self.overlay_map_ns, self.gesture_start_ns),
            "gesture_to_capture_complete_ms": _delta(
                self.gesture_start_ns, self.capture_complete_ns
            ),
            "gesture_to_server_border_pixel_ms": _delta(
                self.gesture_start_ns, self.render_probe_ns
            ),
            "render_probe_status": self.render_probe_status,
            "render_pixel_before": self.render_pixel_before,
            "render_pixel_after": self.render_pixel_after,
            "cursor_signal": self.cursor_signal
            or ("selection_input_ready" if self.ready_marker_ns is not None else None),
            "overlay_signal": self.overlay_signal,
            "overlay_window_id": self.overlay_window_id,
            "capture_dimensions": self.capture_dimensions,
            "gesture_outcome": self.gesture_outcome,
            "output_lines": self.output_lines,
        }
        if self.contention_start_ns is not None:
            values["contention_hold_ms"] = _delta(
                self.contention_start_ns, self.contention_release_ns
            )
            values["launch_to_contention_release_ms"] = _delta(
                self.launch_ns, self.contention_release_ns
            )
        return values


def _delta(start: int | None, end: int | None) -> float | None:
    if start is None or end is None:
        return None
    return round((end - start) / 1_000_000.0, 3)


def _spawn(command: Sequence[str], env: Mapping[str, str]) -> Any:
    return smoke._spawn(command, env)


def _start_resident(binary: Path, env: Mapping[str, str]) -> tuple[Any, LinePump]:
    daemon = _spawn([str(binary), "--daemon"], env)
    pump = LinePump(daemon)
    smoke._wait_for_instance_owner(env, timeout=5)
    # Let the daemon return to poll() before the first command.  This is not
    # included in any sample and prevents startup scheduling from biasing the
    # first warm capture.
    time.sleep(0.02)
    return daemon, pump


def _reset_failed_capture(env: Mapping[str, str], observer: X11Observer) -> bool:
    """Cancel a timed-out selection before the next independent trial."""
    # Right-click is handled by the pointer path and therefore still reaches
    # the selector when a candidate has deferred its keyboard grab.  Escape
    # covers implementations that only expose keyboard cancellation.
    for command in (
        ["xdotool", "click", "3"],
        ["xdotool", "key", "--clearmodifiers", "Escape"],
    ):
        with contextlib.suppress(Exception):
            smoke._run(command, env, check=False, timeout=2)
    deadline = time.monotonic() + 0.75
    saw_unmap = False
    while time.monotonic() < deadline:
        for event in observer.poll_events(0.02):
            if event["kind"] == "overlay_unmap_notify":
                saw_unmap = True
        if saw_unmap:
            break
    observer.drain()
    return saw_unmap


def _consume_sample(
    binary: Path,
    env: Mapping[str, str],
    observer: X11Observer,
    pump: LinePump | None,
    *,
    resident: bool,
    contention_ms: int | None,
    early_gesture: bool,
    render_probe: bool,
    timeout_s: float = 3.0,
) -> dict[str, Any]:
    observer.drain()
    contention: KeyboardContention | None = None
    if contention_ms is not None:
        contention = KeyboardContention(env, contention_ms)
        contention.start()
    launch_ns = time.perf_counter_ns()
    sample = Sample(launch_ns=launch_ns)
    if contention is not None:
        sample.contention_start_ns = contention.started_ns
    process = _spawn([str(binary), "--region", "--clipboard"], env)
    process_pump = pump if resident else LinePump(process)
    if process_pump is None:
        raise LatencyError("warm condition has no resident output pump")
    sent_gesture = False
    hold_active = False
    render_probe_deadline_ns = 0
    viewable_check_ns = 0
    deadline = time.monotonic() + timeout_s
    try:
        if resident:
            process.wait(timeout=4)
            sample.command_return_ns = time.perf_counter_ns()
            if process.returncode != 0:
                raise LatencyError(f"resident command exited {process.returncode}")
        while time.monotonic() < deadline:
            for event_ns, line in process_pump.drain():
                sample.output_lines.append(line)
                if READY_RE.search(line) and sample.ready_marker_ns is None:
                    sample.ready_marker_ns = event_ns
                    if sample.cursor_signal is None and not observer.has_xfixes:
                        sample.cursor_signal = "selection_input_ready"
                capture = CAPTURE_RE.search(line)
                if capture and sample.capture_complete_ns is None:
                    sample.capture_complete_ns = event_ns
                    sample.capture_dimensions = [
                        int(capture["width"]),
                        int(capture["height"]),
                    ]
            # Never let a synchronous observer request delay the early burst.
            # The marker is emitted after the root grab and is the earliest
            # input signal available from older instrumented candidates.
            if early_gesture and sample.ready_marker_ns is not None and not sent_gesture:
                gesture_start, gesture_flush = observer.fake_drag()
                sample.gesture_start_ns = gesture_start
                sample.gesture_flush_ns = gesture_flush
                sample.gesture_outcome = "sent_waiting_for_capture"
                sent_gesture = True
            for event in observer.poll_events(0.002):
                if event["kind"] == "xfixes_cursor_notify" and sample.cursor_notify_ns is None:
                    sample.cursor_notify_ns = int(event["at_ns"])
                    sample.cursor_signal = "xfixes_cursor_notify"
                elif event["kind"] == "overlay_map_notify":
                    if sample.overlay_map_ns is None:
                        sample.overlay_map_ns = int(event["at_ns"])
                        sample.overlay_signal = "MapNotify"
                        sample.overlay_window_id = int(event["window_id"])
                        viewable_check_ns = sample.overlay_map_ns + 5_000_000
                elif event["kind"] == "overlay_unmap_notify":
                    sample.overlay_unmap_ns = int(event["at_ns"])
                    # A map followed by an unmap before the gesture is a
                    # failed/transient setup, not usable readiness.
            if (
                not early_gesture
                and sample.overlay_window_id is not None
                and sample.overlay_viewable_ns is None
                and time.perf_counter_ns() >= viewable_check_ns
                and viewable_check_ns != 0
            ):
                checked_at = time.perf_counter_ns()
                if observer.window_is_viewable(sample.overlay_window_id):
                    sample.overlay_viewable_ns = checked_at
                # Do not poll the same XID repeatedly.  A transient map that
                # vanished will be represented by overlay_unmap_notify.
                viewable_check_ns = 2**63 - 1
            if (
                render_probe
                and sample.overlay_viewable_ns is not None
                and not sent_gesture
            ):
                if sample.overlay_window_id is None:
                    raise LatencyError("render probe has no mapped overlay window")
                before = observer.read_pixel(sample.overlay_window_id)
                gesture_start, _ = observer.begin_hold_drag()
                sample.gesture_start_ns = gesture_start
                sample.render_pixel_before = before.hex()
                sample.gesture_outcome = "render_probe_waiting"
                sent_gesture = True
                hold_active = True
                render_probe_deadline_ns = time.perf_counter_ns() + 1_000_000_000
            if render_probe and hold_active and sample.render_probe_ns is None:
                if time.perf_counter_ns() >= render_probe_deadline_ns:
                    sample.render_probe_status = "timeout"
                    observer.release_hold_drag()
                    hold_active = False
                elif sample.overlay_window_id is not None:
                    after = observer.read_pixel(sample.overlay_window_id)
                    sample.render_pixel_after = after.hex()
                    if after != bytes.fromhex(sample.render_pixel_before or "") and any(
                        value >= 220 for value in after[:3]
                    ):
                        sample.render_probe_ns = time.perf_counter_ns()
                        sample.render_probe_status = "server_border_pixel_observed"
                        observer.release_hold_drag()
                        hold_active = False
            # Prefer the explicit internal marker when available: current
            # builds install the root grab before the expensive snapshot.  If
            # an older binary has no marker, wait for the first full-screen
            # MapNotify.  The result is accepted only if capture_complete is
            # later observed, so a transient map cannot create a false pass.
            gesture_trigger = (
                sample.ready_marker_ns is not None
                or (sample.ready_marker_ns is None and sample.overlay_map_ns is not None)
                if early_gesture
                else sample.overlay_viewable_ns is not None
            )
            if gesture_trigger and not sent_gesture:
                gesture_start, gesture_flush = observer.fake_drag()
                sample.gesture_start_ns = gesture_start
                sample.gesture_flush_ns = gesture_flush
                sample.gesture_outcome = "sent_waiting_for_capture"
                sent_gesture = True
            if sample.capture_complete_ns is not None:
                if sample.capture_dimensions != [200, 150]:
                    sample.gesture_outcome = "capture_wrong_dimensions"
                elif sample.overlay_unmap_ns is not None and sample.overlay_unmap_ns < sample.capture_complete_ns:
                    # A normal completed capture may unmap as part of cleanup;
                    # preserve that event but do not call it a setup failure.
                    sample.gesture_outcome = "capture_complete"
                else:
                    sample.gesture_outcome = "capture_complete"
                break
            if process.poll() is not None and not resident and process.returncode not in (0, None):
                raise LatencyError(
                    f"cold capture process exited {process.returncode}: {sample.output_lines[-5:]}"
                )
        else:
            sample.gesture_outcome = "capture_timeout"
        if contention is not None:
            contention.close()
            sample.contention_release_ns = contention.released_ns
        return sample.finish()
    finally:
        if hold_active:
            with contextlib.suppress(Exception):
                observer.release_hold_drag()
        if contention is not None:
            contention.close()
            sample.contention_release_ns = contention.released_ns
        if resident:
            smoke._terminate(process)
        else:
            smoke._terminate(process)
            process_pump.close()


def _run_condition(
    binary: Path,
    env: Mapping[str, str],
    size: tuple[int, int],
    condition: str,
    iterations: int,
    contention_ms: int,
) -> dict[str, Any]:
    resident = condition.startswith("warm")
    contested = "contention" in condition
    render_probe = condition.endswith("-render")
    early_gesture = not condition.endswith("-mapped") and not render_probe
    daemon = None
    pump = None
    observer = X11Observer(env, size)
    samples: list[dict[str, Any]] = []
    try:
        if resident:
            daemon, pump = _start_resident(binary, env)
        for _ in range(iterations):
            sample = _consume_sample(
                binary,
                env,
                observer,
                pump,
                resident=resident,
                contention_ms=contention_ms if contested else None,
                early_gesture=early_gesture,
                render_probe=render_probe,
            )
            samples.append(sample)
            if sample["gesture_outcome"] != "capture_complete":
                # Keep collecting independent trials.  Escape is sent only as
                # fixture cleanup after the failed timed gesture; it is never
                # part of a successful latency sample.  A candidate that
                # cannot recover remains visible through its failure count.
                sample["reset_after_failure"] = _reset_failed_capture(env, observer)
                if resident and not sample["reset_after_failure"]:
                    # A candidate that keeps the selector alive after both
                    # cancellation paths failed cannot provide an independent
                    # next trial. Restart only that private fixture daemon;
                    # the raw sample remains a failure and is never hidden.
                    if daemon is not None:
                        smoke._terminate(daemon)
                    if pump is not None:
                        pump.close()
                    daemon, pump = _start_resident(binary, env)
                    sample["daemon_restarted_after_failure"] = True
        metrics = (
            "launch_to_cursor_signal_ms",
            "launch_to_xfixes_cursor_notify_ms",
            "launch_to_ready_marker_ms",
            "launch_to_overlay_map_ms",
            "launch_to_overlay_viewable_ms",
            "launch_to_gesture_ms",
            "launch_to_capture_complete_ms",
            "gesture_to_capture_complete_ms",
            "gesture_to_server_border_pixel_ms",
        )
        return {
            "condition": condition,
            "gesture_policy": (
                "hold-until-server-border-pixel"
                if render_probe
                else "early-marker"
                if early_gesture
                else "after-map"
            ),
            "xfixes_selected": observer.has_xfixes,
            "xfixes_event_samples": sum(
                sample.get("launch_to_xfixes_cursor_notify_ms") is not None
                for sample in samples
            ),
            "iterations_requested": iterations,
            "iterations_completed": len(samples),
            "samples": samples,
            "success_count": sum(s["gesture_outcome"] == "capture_complete" for s in samples),
            "failure_count": sum(s["gesture_outcome"] != "capture_complete" for s in samples),
            "summary": {metric: _summary(samples, metric) for metric in metrics},
        }
    finally:
        observer.close()
        if daemon is not None:
            with contextlib.suppress(Exception):
                smoke._run([str(binary), "--quit"], env, timeout=3)
            smoke._terminate(daemon)
        if pump is not None:
            pump.close()


def _print_run(candidate: str, size: tuple[int, int], run: Mapping[str, Any]) -> None:
    print(
        f"{candidate} size={size[0]}x{size[1]} condition={run['condition']} "
        f"success={run['success_count']}/{run['iterations_requested']}"
    )
    for key, summary in run["summary"].items():
        if summary["n"]:
            print(
                f"  {key}: median={summary['median_ms']:.3f}ms "
                f"p95={summary['p95_ms']:.3f}ms "
                f"n={summary['n']} missing={summary['missing']}"
            )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", action="append", required=True, type=_parse_candidate, metavar="NAME=PATH")
    parser.add_argument("--size", action="append", type=_parse_size)
    parser.add_argument(
        "--condition",
        action="append",
        choices=(
            "warm",
            "warm-mapped",
            "cold",
            "cold-mapped",
            "warm-render",
            "warm-contention",
            "cold-contention",
        ),
        default=None,
    )
    parser.add_argument("--iterations", type=int, default=20)
    parser.add_argument("--keyboard-hold-ms", type=int, default=150)
    parser.add_argument("--json-out", type=Path)
    args = parser.parse_args(argv)
    if args.iterations < 5:
        parser.error("--iterations must be at least 5")
    if args.keyboard_hold_ms <= 0:
        parser.error("--keyboard-hold-ms must be positive")
    required = smoke._require_commands(("xdotool",))
    if required:
        print(f"FAIL: missing commands: {', '.join(required)}", file=sys.stderr)
        return 2

    requested_sizes = args.size or [(5560, 1920)]
    requested_conditions = args.condition or ["warm"]
    # Normalize duplicate values while preserving their requested order.
    sizes: list[tuple[int, int]] = []
    for size in requested_sizes:
        if size not in sizes:
            sizes.append(size)
    runs: list[dict[str, Any]] = []
    try:
        with tempfile_context(prefix="snipchord-latency-") as runtime:
            for candidate_name, binary in args.candidate:
                for size in sizes:
                    for condition in requested_conditions:
                        # One fresh X server per candidate/condition prevents a
                        # failed grab or a stale preview from contaminating the
                        # next measurement while still keeping every sample in
                        # a condition on one server.
                        with smoke.XvfbServer(runtime / f"{candidate_name}-{size[0]}x{size[1]}-{condition}", *size) as server:
                            env = dict(server.env)
                            env["SNIPCHORD_BENCHMARK_READY"] = "1"
                            run = _run_condition(
                                binary,
                                env,
                                size,
                                condition,
                                args.iterations,
                                args.keyboard_hold_ms,
                            )
                            record = {
                                "candidate": candidate_name,
                                "binary": str(binary),
                                "xvfb": str(server.xvfb),
                                "display": server.display,
                                "size": [size[0], size[1]],
                                **run,
                            }
                            runs.append(record)
                            _print_run(candidate_name, size, run)
    except (smoke.SmokeError, LatencyError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1

    artifact = {
        "schema": 1,
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "iterations_requested": args.iterations,
        "keyboard_hold_ms": args.keyboard_hold_ms,
        "python": sys.version.split()[0],
        "tools": {"xdotool": _tool_version(["xdotool", "--version"])},
        "candidates": {
            name: {
                "path": str(path),
                "sha256": _sha256(path),
                "size_bytes": path.stat().st_size,
            }
            for name, path in args.candidate
        },
        "measurement_limits": [
            "XFIXES and MapNotify are server-side event delivery times, not monitor presentation times.",
            "selection_input_ready is an internal fallback marker and is never reported as physical cursor visibility.",
            "The XTEST gesture starts after the internal marker when available, otherwise after a full-screen map signal.",
            "The contention condition holds a foreign keyboard grab; it is a deterministic fixture, not a claim about a particular desktop compositor.",
        ],
        "runs": runs,
    }
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(json.dumps(artifact, indent=2) + "\n")
        print(f"JSON: {args.json_out}")
    return 0


@contextlib.contextmanager
def tempfile_context(prefix: str):
    import tempfile

    with tempfile.TemporaryDirectory(prefix=prefix) as path:
        runtime = Path(path)
        yield runtime


if __name__ == "__main__":
    raise SystemExit(main())
