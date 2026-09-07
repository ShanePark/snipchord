#!/usr/bin/env python3
"""Measure thumbnail-click latency against an isolated X11 server.

The capture itself is outside the timed interval.  Each sample waits until the
thumbnail is visible (and, for clipboard mode, until the prepared PNG exists),
then measures the synthetic click through the controlled ``xdg-open`` stub.
This separates the viewer hand-off from root capture and PNG preparation.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import statistics
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Mapping


ROOT = Path(__file__).resolve().parents[1]
SMOKE_PATH = ROOT / "tests" / "rust_x11_smoke.py"
SPEC = importlib.util.spec_from_file_location("snipchord_rust_x11_smoke", SMOKE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"could not load {SMOKE_PATH}")
SMOKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)


def _trial_env(base: Mapping[str, str], root: Path, label: str) -> dict[str, str]:
    env = dict(base)
    trial = root / label
    for name in ("home", "config", "data", "cache", "runtime"):
        directory = trial / name
        directory.mkdir(parents=True, exist_ok=True)
        if name == "runtime":
            directory.chmod(0o700)
    env.update(
        {
            "HOME": str(trial / "home"),
            "XDG_CONFIG_HOME": str(trial / "config"),
            "XDG_DATA_HOME": str(trial / "data"),
            "XDG_CACHE_HOME": str(trial / "cache"),
            "XDG_RUNTIME_DIR": str(trial / "runtime"),
        }
    )
    settings = Path(env["XDG_CONFIG_HOME"]) / "snipchord" / "settings.json"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        json.dumps(
            {
                "save_automatically": False,
                "show_preview": True,
                "output_directory": "~/Downloads",
            }
        )
        + "\n"
    )
    return env


def _digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _wait_for_exit(process: subprocess.Popen[bytes], timeout: float = 3) -> None:
    deadline = time.monotonic() + timeout
    while process.poll() is None and time.monotonic() < deadline:
        time.sleep(0.02)
    if process.poll() is None:
        raise SMOKE.SmokeError("preview latency daemon did not exit after --quit")


def _timed_xdg_open_stub(env: Mapping[str, str]) -> tuple[dict[str, str], Path]:
    """Record the stub's wall-clock entry before the viewer path is handled."""
    directory = Path(env["XDG_RUNTIME_DIR"]) / "snipchord-timed-xdg-open"
    directory.mkdir(parents=True, exist_ok=True)
    executable = directory / "xdg-open"
    log = directory / "opened.log"
    executable.write_text(
        "#!/bin/sh\n"
        "timestamp=$(date +%s%N)\n"
        "printf '%s\\t%s\\n' \"$timestamp\" \"$1\" >> \"$SNIPCHORD_XDG_OPEN_LOG\"\n"
    )
    executable.chmod(0o755)
    child_env = dict(env)
    child_env["PATH"] = f"{directory}{os.pathsep}{env.get('PATH', '')}"
    child_env["SNIPCHORD_XDG_OPEN_LOG"] = str(log)
    return child_env, log


def _wait_for_timed_open(log: Path, timeout: float = 10) -> tuple[int, Path]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if log.exists():
            lines = [line.strip() for line in log.read_text().splitlines() if line.strip()]
            if lines:
                timestamp, path = lines[-1].split("\t", 1)
                return int(timestamp), Path(path)
        time.sleep(0.01)
    raise SMOKE.SmokeError(f"timed xdg-open was not called; log={log}")


def _measure_mode(
    binary: Path,
    mode: str,
    prepared_cache: bool,
    samples: int,
    server_env: Mapping[str, str],
    runtime: Path,
    root_size: tuple[int, int],
) -> dict[str, object]:
    values: list[float] = []
    failures: list[str] = []
    prepared_waits: list[float] = []
    for index in range(samples):
        env = _trial_env(server_env, runtime, f"{mode}-{index}")
        open_env, log = _timed_xdg_open_stub(env)
        before_cache = SMOKE._temporary_preview_paths(open_env)
        flag = "--save" if mode == "save" else "--clipboard"
        app = SMOKE._spawn([str(binary), "--fullscreen", flag], open_env)
        try:
            match, output = SMOKE._read_until(app, SMOKE.CAPTURE_RE, 15)
            dimensions = (int(match["width"]), int(match["height"]))
            if dimensions != root_size:
                raise SMOKE.SmokeError(
                    f"{mode} capture reported {dimensions}; output={output[-1000:]}"
                )
            prepared: set[Path] = set()
            if mode == "clipboard" and prepared_cache:
                started = time.perf_counter()
                prepared = SMOKE._wait_for_new_preview_paths(open_env, before_cache, 5)
                prepared_waits.append((time.perf_counter() - started) * 1000)
            geometry = SMOKE._wait_for_thumbnail_window(open_env, root_size, 8)
            x = int(geometry["x"]) + int(geometry["width"]) // 2
            y = int(geometry["y"]) + int(geometry["height"]) // 2
            SMOKE._run(["xdotool", "mousemove", str(x), str(y)], open_env)
            log.unlink(missing_ok=True)
            started_ns = time.time_ns()
            SMOKE._run(["xdotool", "click", "1"], open_env)
            opened_timestamp, opened = _wait_for_timed_open(log, 10)
            opened = opened.resolve()
            if not opened.is_file():
                raise SMOKE.SmokeError(f"xdg-open path does not exist: {opened}")
            if mode == "clipboard" and prepared_cache and opened not in prepared:
                raise SMOKE.SmokeError(
                    f"clipboard click opened an unprepared path: {opened}; prepared={sorted(prepared)}"
                )
            if mode == "save":
                destination = Path(open_env["HOME"]) / "Downloads"
                if opened.parent != destination.resolve():
                    raise SMOKE.SmokeError(
                        f"save click opened outside configured output: {opened} != {destination}"
                    )
            values.append((opened_timestamp - started_ns) / 1_000_000)
        except Exception as error:
            failures.append(f"sample {index + 1}: {error}")
        finally:
            with SMOKE.contextlib.suppress(Exception):
                SMOKE._run([str(binary), "--quit"], open_env, timeout=8)
            with SMOKE.contextlib.suppress(Exception):
                _wait_for_exit(app)
            SMOKE._terminate(app)
    return {
        "binary_sha256": _digest(binary),
        "mode": mode,
        "prepared_cache_expected": prepared_cache,
        "requested": samples,
        "completed": len(values),
        "failures": failures,
        "click_to_xdg_open_ms": {
            "p50": statistics.median(values) if values else None,
            "p95": _percentile(values, 0.95) if values else None,
            "min": min(values) if values else None,
            "max": max(values) if values else None,
            "samples": values,
        },
        "clipboard_prepare_wait_ms": {
            "p50": statistics.median(prepared_waits) if prepared_waits else None,
            "p95": _percentile(prepared_waits, 0.95) if prepared_waits else None,
            "samples": prepared_waits,
        },
    }


def _percentile(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * quantile
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = position - lower
    return ordered[lower] + (ordered[upper] - ordered[lower]) * fraction


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--width", type=int, default=5560)
    parser.add_argument("--height", type=int, default=1920)
    args = parser.parse_args()
    binaries = {
        "baseline": args.baseline.expanduser().resolve(),
        "candidate": args.candidate.expanduser().resolve(),
    }
    for name, binary in binaries.items():
        if not binary.is_file():
            parser.error(f"{name} binary does not exist: {binary}")
    root_size = (args.width, args.height)
    with tempfile.TemporaryDirectory(prefix="snipchord-preview-latency-") as temporary:
        runtime = Path(temporary)
        server = SMOKE.XvfbServer(runtime, *root_size)
        server.start()
        fixture = SMOKE._publish_fixture(server.env, *root_size)
        try:
            results = []
            for name, binary in binaries.items():
                for mode in ("save", "clipboard"):
                    result = _measure_mode(
                        binary,
                        mode,
                        name == "candidate" and mode == "clipboard",
                        args.samples,
                        server.env,
                        runtime / name,
                        root_size,
                    )
                    result["label"] = name
                    results.append(result)
                    print(json.dumps(result, sort_keys=True), flush=True)
        finally:
            fixture.close()
            server.stop()
    report = {
        "schema": "snipchord.preview-latency.v1",
        "metric": "thumbnail click to controlled xdg-open invocation",
        "xvfb": os.environ.get("SNIPCHORD_XVFB"),
        "display": {"width": args.width, "height": args.height},
        "samples_per_condition": args.samples,
        "conditions": results,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    return 1 if any(condition["failures"] for condition in results) else 0


if __name__ == "__main__":
    raise SystemExit(main())
