#!/usr/bin/env python3
"""Measure resident ``--region`` latency on an isolated X11 server.

This benchmark keeps one Xvfb server alive while it runs every binary passed
with ``--binary``.  Each binary gets its own resident daemon, but the server
size, fixture environment, pointer path, and iteration count are identical.
The default metric runs from shortcut client launch until a new full-screen
selection window becomes viewable. Use --metric cursor to measure the early
cursor-ready marker instead; this requires a binary with marker support and
is a separate metric, not directly comparable to selection-ready timings.
Each measured capture starts while the previous capture's preview is visible.

Example (the baseline path is intentionally explicit)::

    python3 tests/benchmark_capture.py \
        --binary /tmp/snipchord-baseline-c7b2 \
        --binary target/release/snipchord

The display is private Xvfb; no live desktop or clipboard is used.  The
script reuses the existing smoke-test harness for Xvfb discovery and process
cleanup, so it also works on hosts where Xvfb is supplied by JetBrains.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import statistics
import sys
import tempfile
import time
from collections.abc import Mapping, Sequence


ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))

import rust_x11_smoke as smoke  # noqa: E402  (local harness import)


READY_RE = re.compile(r"selection_input_ready")


def _viewable_selection(
    env: Mapping[str, str],
    size: tuple[int, int],
    seen: set[str],
    timeout: float,
) -> str:
    deadline = time.perf_counter() + timeout
    while time.perf_counter() < deadline:
        # A just-completed capture can destroy its selection window between
        # xwininfo's root-tree request and its follow-up query. Treat that
        # short BadWindow race as an ordinary poll miss.
        try:
            windows = smoke._window_geometries(env)
        except smoke.SmokeError:
            time.sleep(0.005)
            continue
        for window in windows:
            window_id = str(window["id"])
            if window_id in seen:
                continue
            if (int(window["width"]), int(window["height"])) != size:
                continue
            info = smoke._run(
                ["xwininfo", "-id", window_id], env, timeout=4
            ).stdout.decode(errors="replace")
            if "Map State: IsViewable" in info:
                return window_id
        time.sleep(0.005)
    raise smoke.SmokeError(
        f"selection window {size[0]}x{size[1]} did not become viewable "
        f"within {timeout:.0f}s"
    )


def _invoke_region(binary: pathlib.Path, env: Mapping[str, str]) -> None:
    command = smoke._spawn([str(binary), "--region"], env)
    try:
        command.wait(timeout=15)
        if command.returncode != 0:
            output = b""
            if command.stdout is not None:
                output = command.stdout.read() or b""
            raise smoke.SmokeError(
                f"{binary} --region command exited {command.returncode}: "
                f"{output.decode(errors='replace')[-1000:]}"
            )
    finally:
        smoke._terminate(command)


def _wait_input_ready(daemon, timeout: float = 8) -> None:
    """Wait for the resident app's early pointer/keyboard grab marker."""
    smoke._read_until(daemon, READY_RE, timeout)


def _complete_small_capture(
    daemon, env: Mapping[str, str], timeout: float = 120
) -> None:
    # Keep this rectangle away from the cursor badge and the desktop edges.
    smoke._run(["xdotool", "mousemove", "100", "100"], env)
    smoke._run(["xdotool", "mousedown", "1"], env)
    try:
        smoke._run(["xdotool", "mousemove", "300", "250"], env)
    finally:
        smoke._run(["xdotool", "mouseup", "1"], env)
    smoke._read_until(daemon, smoke.CAPTURE_RE, timeout)


def _measure_binary(
    binary: pathlib.Path,
    env: Mapping[str, str],
    size: tuple[int, int],
    iterations: int,
    metric: str,
) -> list[float]:
    daemon = smoke._spawn([str(binary), "--daemon"], env)
    try:
        # Give the daemon time to claim its X11 selection before priming it.
        deadline = time.perf_counter() + 5
        while time.perf_counter() < deadline:
            if daemon.poll() is not None:
                output = daemon.stdout.read() if daemon.stdout else b""
                raise smoke.SmokeError(
                    f"{binary} daemon exited {daemon.returncode}: "
                    f"{output.decode(errors='replace')[-1000:]}"
                )
            time.sleep(0.01)

        # Prime the resident process with a real capture. Every measured
        # command below then starts while its preview is visible.
        _invoke_region(binary, env)
        if metric == "cursor":
            _wait_input_ready(daemon)
        seen: set[str] = set()
        _viewable_selection(env, size, seen, timeout=120)
        _complete_small_capture(daemon, env)

        durations: list[float] = []
        for _ in range(iterations):
            started = time.perf_counter()
            _invoke_region(binary, env)
            if metric == "cursor":
                _wait_input_ready(daemon)
                durations.append((time.perf_counter() - started) * 1000.0)
            selection = _viewable_selection(env, size, seen, timeout=120)
            if metric == "selection":
                durations.append((time.perf_counter() - started) * 1000.0)
            seen.add(selection)
            _complete_small_capture(daemon, env)
        return durations
    finally:
        smoke._terminate(daemon)


def _format(values: Sequence[float]) -> str:
    ordered = sorted(values)
    return (
        f"n={len(values)} median_ms={statistics.median(values):.2f} "
        f"min_ms={ordered[0]:.2f} max_ms={ordered[-1]:.2f} "
        f"samples_ms={[round(value, 2) for value in values]}"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        action="append",
        required=True,
        type=pathlib.Path,
        help="binary to measure; repeat for baseline/current comparison",
    )
    parser.add_argument("--metric", choices=("selection", "cursor"), default="selection")
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--width", type=int, default=5560)
    parser.add_argument("--height", type=int, default=1920)
    args = parser.parse_args()
    if args.iterations < 5:
        parser.error("--iterations must be at least 5")
    size = (args.width, args.height)
    if args.width <= 0 or args.height <= 0:
        parser.error("--width and --height must be positive")
    for binary in args.binary:
        if not binary.is_file() or not binary.stat().st_mode & 0o111:
            parser.error(f"binary is not executable: {binary}")

    required = smoke._require_commands(("xdotool", "xwininfo"))
    if required:
        print(f"FAIL: missing commands: {', '.join(required)}", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="snipchord-benchmark-") as temporary:
        runtime = pathlib.Path(temporary)
        try:
            with smoke.XvfbServer(runtime, args.width, args.height) as server:
                if args.metric == "cursor":
                    server.env["SNIPCHORD_BENCHMARK_READY"] = "1"
                for binary in args.binary:
                    durations = _measure_binary(binary, server.env, size, args.iterations, args.metric)
                    print(f"{binary}: metric={args.metric} {_format(durations)}", flush=True)
        except smoke.SmokeError as error:
            print(f"FAIL: {error}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
