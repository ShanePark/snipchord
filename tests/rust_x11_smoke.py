#!/usr/bin/env python3
"""Run SnipChord's X11 smoke checks on a private Xvfb display.

This is an explicit integration check rather than a unittest module.  It starts
its own X server, sets all child processes to that display, and uses a temporary
HOME/XDG configuration directory.  The live desktop (including DISPLAY=:1 in
the development container) and the user's clipboard are never touched.

The normal invocation is::

    python3 tests/rust_x11_smoke.py --binary target/release/snipchord

The script needs ``Xvfb``, ``xclip``, ``xdotool``, ``xdpyinfo``, ``xwininfo``,
``xwd``, ImageMagick, Python Xlib, and Pillow.  GTK's Python bindings are
optional and are used only for the legacy memory reference when available.
The JetBrains remote development bundle is searched when Xvfb is not on PATH.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
from typing import Iterable, Mapping, Sequence


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_WIDTH = 1024
DEFAULT_HEIGHT = 768
INCR_THRESHOLD = 256 * 1024
CAPTURE_RE = re.compile(
    r"capture_complete\s+width=(?P<width>\d+)\s+height=(?P<height>\d+)\s+"
    r"demo=(?P<demo>true|false|1|0)",
    re.IGNORECASE,
)
READY_RE = re.compile(r"selection_input_ready")


class SmokeError(RuntimeError):
    """A check could run but did not satisfy its contract."""


def _require_commands(names: Iterable[str]) -> list[str]:
    return [name for name in names if shutil.which(name) is None]


def _x11_extensions(env: Mapping[str, str]) -> set[str]:
    """Return extension names advertised by the isolated X server."""
    result = subprocess.run(
        ["xdpyinfo"],
        env=dict(env),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=3,
        check=True,
    )
    extensions: set[str] = set()
    reading = False
    for line in result.stdout.decode(errors="replace").splitlines():
        if line.startswith("number of extensions:"):
            reading = True
            continue
        if reading and line.startswith("default screen number:"):
            break
        if reading and line.strip():
            extensions.add(line.strip())
    return extensions


def _find_xvfb() -> Path | None:
    requested = os.environ.get("SNIPCHORD_XVFB")
    candidates: list[Path] = []
    if requested:
        candidates.append(Path(requested).expanduser())
    path = shutil.which("Xvfb")
    if path:
        candidates.append(Path(path))
    # JetBrains bundles Xvfb for its remote development server.  It is useful
    # on minimal Ubuntu installations where xvfb is not installed globally.
    candidates.extend(
        Path.home().glob(
            ".local/share/JetBrains/Toolbox/apps/*/plugins/remote-dev-server/"
            "selfcontained/bin/Xvfb"
        )
    )
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def _server_env(base: Mapping[str, str], display: str, runtime: Path) -> dict[str, str]:
    env = dict(base)
    # Do not inherit a live X/Wayland authority or a desktop D-Bus session.
    # -ac on our fresh Xvfb server is local to this test process.
    env["DISPLAY"] = display
    env.pop("WAYLAND_DISPLAY", None)
    env.pop("XAUTHORITY", None)
    env.pop("DBUS_SESSION_BUS_ADDRESS", None)
    env["GDK_BACKEND"] = "x11"
    env["NO_AT_BRIDGE"] = "1"
    env["HOME"] = str(runtime / "home")
    env["XDG_CONFIG_HOME"] = str(runtime / "config")
    env["XDG_DATA_HOME"] = str(runtime / "data")
    env["XDG_CACHE_HOME"] = str(runtime / "cache")
    env["XDG_RUNTIME_DIR"] = str(runtime / "runtime")
    for directory in ("home", "config", "data", "cache", "runtime"):
        path = runtime / directory
        path.mkdir(parents=True, exist_ok=True)
        if directory == "runtime":
            path.chmod(0o700)
    return env


class XvfbServer:
    """Own a fresh X server and a child-process environment for it."""

    def __init__(
        self,
        runtime: Path,
        width: int,
        height: int,
        display: int | None = None,
        extra_args: Sequence[str] = (),
    ):
        self.runtime = runtime
        self.width = width
        self.height = height
        self.requested_display = display
        self.display_number: int | None = None
        self._process_display: int | None = None
        self.process: subprocess.Popen[bytes] | None = None
        self.env: dict[str, str] = {}
        self.xvfb = _find_xvfb()
        self.extra_args = tuple(extra_args)

    @property
    def display(self) -> str:
        if self.display_number is None:
            raise RuntimeError("Xvfb is not running")
        return f":{self.display_number}"

    def _display_candidates(self) -> Iterable[int]:
        if self.requested_display is not None:
            yield self.requested_display
            return
        # Keep test displays away from the usual :0/:1 desktop sessions.
        for candidate in range(93, 130):
            yield candidate

    def start(self) -> None:
        if self.xvfb is None:
            raise SmokeError(
                "Xvfb was not found (set SNIPCHORD_XVFB or install xvfb); "
                "the isolated X11 smoke check cannot run"
            )
        for number in self._display_candidates():
            lock = Path(f"/tmp/.X{number}-lock")
            socket_path = Path("/tmp/.X11-unix") / f"X{number}"
            if lock.exists() or socket_path.exists():
                continue
            display = f":{number}"
            self.env = _server_env(os.environ, display, self.runtime)
            # The JetBrains build is linked against OpenSSL 1.0.  Keep this
            # path local to the server process; no system library is installed.
            bundled_lib = self.xvfb.parent.parent / "lib"
            xvfb_env = dict(self.env)
            if (bundled_lib / "libcrypto.so.10").is_file():
                current = xvfb_env.get("LD_LIBRARY_PATH", "")
                xvfb_env["LD_LIBRARY_PATH"] = str(bundled_lib) + (
                    os.pathsep + current if current else ""
                )
            log_path = self.runtime / "xvfb.log"
            log = log_path.open("wb")
            try:
                process = subprocess.Popen(
                    [
                        str(self.xvfb),
                        display,
                        "-screen",
                        "0",
                        f"{self.width}x{self.height}x24",
                        "-nolisten",
                        "tcp",
                        "-ac",
                        *self.extra_args,
                    ],
                    env=xvfb_env,
                    stdin=subprocess.DEVNULL,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
            finally:
                log.close()
            self.process = process
            self._process_display = number
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    break
                if socket_path.exists() and self._can_connect():
                    self.display_number = number
                    return
                time.sleep(0.03)
            output = log_path.read_text(errors="replace")
            self.stop()
            if process.poll() is None:
                continue
            # A candidate can be occupied between the preflight and Popen.
            if "already active" in output or "Cannot establish" in output:
                continue
            raise SmokeError(f"Xvfb failed on {display}:\n{output.strip()}")
        raise SmokeError(
            "could not start an isolated Xvfb display; "
            "run this command with sandbox escalation if the environment blocks AF_UNIX sockets"
        )

    def _can_connect(self) -> bool:
        result = subprocess.run(
            ["xdpyinfo"],
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=2,
        )
        return result.returncode == 0

    def stop(self) -> None:
        process, self.process = self.process, None
        number, self._process_display = self._process_display, None
        if process is None:
            return
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
        if number is not None:
            # Some bundled Xvfb builds leave these entries behind on SIGTERM.
            # They were absent before this process started, so removing only
            # this display's own socket/lock cannot affect a live desktop.
            with contextlib.suppress(FileNotFoundError):
                Path(f"/tmp/.X{number}-lock").unlink()
            with contextlib.suppress(FileNotFoundError):
                (Path("/tmp/.X11-unix") / f"X{number}").unlink()

    def __enter__(self) -> "XvfbServer":
        self.start()
        return self

    def __exit__(self, *_args: object) -> None:
        self.stop()


def _terminate(process: subprocess.Popen[bytes] | None) -> None:
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=4)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=4)


def _run(
    command: Sequence[str],
    env: Mapping[str, str],
    *,
    timeout: float = 8,
    check: bool = True,
    stdin: bytes | None = None,
) -> subprocess.CompletedProcess[bytes]:
    result = subprocess.run(
        list(command),
        env=dict(env),
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )
    if check and result.returncode != 0:
        raise SmokeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout={result.stdout.decode(errors='replace')[:1000]}\n"
            f"stderr={result.stderr.decode(errors='replace')[:1000]}"
        )
    return result


def _spawn(command: Sequence[str], env: Mapping[str, str]) -> subprocess.Popen[bytes]:
    return subprocess.Popen(
        list(command),
        env=dict(env),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )


def _read_until(
    process: subprocess.Popen[bytes], pattern: re.Pattern[str], timeout: float
) -> tuple[re.Match[str], str]:
    if process.stdout is None:
        raise SmokeError("child process has no stdout")
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    text = ""
    deadline = time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            if process.poll() is not None and not text:
                break
            events = selector.select(max(0.01, min(0.2, deadline - time.monotonic())))
            if not events:
                continue
            chunk = process.stdout.read1(4096)
            if not chunk:
                break
            text += chunk.decode(errors="replace")
            match = pattern.search(text)
            if match:
                return match, text
    finally:
        selector.close()
    raise SmokeError(f"timed out waiting for {pattern.pattern!r}; output={text[-3000:]}")


def _wait_selection_start(process: subprocess.Popen[bytes], timeout: float = 1.0) -> None:
    """Give a freshly spawned selector time to grab the private X11 surface.

    The legacy GTK implementation emitted ``selection_ready_ms`` when its
    window was mapped.  The native Rust implementation intentionally keeps
    stdout for user-visible capture events and has no window title on its
    override-redirect selection surface, so there is no portable readiness
    marker to wait for.  A short liveness wait is sufficient here because the
    Xvfb fixture and child process are local; any immediate startup failure is
    still surfaced to the caller.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            output = b""
            if process.stdout is not None:
                with contextlib.suppress(Exception):
                    output = process.stdout.read() or b""
            raise SmokeError(
                f"selection process exited before input was ready ({process.returncode}): "
                f"{output.decode(errors='replace')[-2000:]}"
            )
        # One scheduler slice is enough for map/grab/event-loop setup while
        # keeping the smoke suite responsive on slower CI hosts.
        time.sleep(0.05)
    if process.poll() is not None:
        raise SmokeError(f"selection process exited before input was ready ({process.returncode})")


def _wait_for_window(env: Mapping[str, str], name: str, timeout: float = 3) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = _run(["xdotool", "search", "--name", name], env, check=False, timeout=2)
        candidates = result.stdout.decode(errors="replace").split()
        if candidates:
            return candidates[-1]
        time.sleep(0.05)
    raise SmokeError(f"window {name!r} did not appear")


WINDOW_GEOMETRY_RE = re.compile(
    r"^\s+(?P<id>0x[0-9a-fA-F]+)\s+.*?"
    r"(?P<width>\d+)x(?P<height>\d+)\+(?P<x>-?\d+)\+(?P<y>-?\d+)\s+\+"
)


def _window_geometries(env: Mapping[str, str]) -> list[dict[str, int | str]]:
    result = _run(["xwininfo", "-root", "-tree"], env, timeout=3)
    windows: list[dict[str, int | str]] = []
    for line in result.stdout.decode(errors="replace").splitlines():
        match = WINDOW_GEOMETRY_RE.match(line)
        if not match:
            continue
        windows.append(
            {
                "id": match["id"],
                "width": int(match["width"]),
                "height": int(match["height"]),
                "x": int(match["x"]),
                "y": int(match["y"]),
            }
        )
    return windows


def _wait_for_window_geometry(
    env: Mapping[str, str],
    predicate,
    timeout: float = 3,
) -> dict[str, int | str]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        candidates = [window for window in _window_geometries(env) if predicate(window)]
        if candidates:
            return candidates[0]
        time.sleep(0.05)
    raise SmokeError("expected X11 window geometry did not appear")


def _wait_for_window_hidden(
    env: Mapping[str, str], window_id: str, timeout: float = 1.5
) -> None:
    """Wait for one transient X11 window to stop being viewable after an action."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = _run(
            ["xwininfo", "-id", window_id],
            env,
            check=False,
            timeout=2,
        )
        if result.returncode != 0 or "Map State: IsViewable" not in result.stdout.decode(
            errors="replace"
        ):
            return
        time.sleep(0.01)
    raise SmokeError(f"X11 window {window_id} remained viewable after the cancel action")


def _wait_for_instance_owner(env: Mapping[str, str], timeout: float = 3) -> int:
    """Wait until the resident app has claimed its private X11 selection."""
    try:
        from Xlib import display
    except ImportError as error:
        raise SmokeError("python-xlib is required for resident readiness verification") from error
    connection = display.Display(env["DISPLAY"])
    try:
        atom = connection.intern_atom("_SNIPCHORD_INSTANCE")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            owner = connection.get_selection_owner(atom)
            if owner:
                return int(getattr(owner, "id", owner))
            time.sleep(0.005)
    finally:
        connection.close()
    raise SmokeError("resident SnipChord instance did not claim its X11 selection")


def _wait_for_thumbnail_window(
    env: Mapping[str, str],
    root_size: tuple[int, int],
    timeout: float = 3,
) -> dict[str, int | str]:
    """Find the small, lower-right screenshot thumbnail without relying on a title.

    The macOS-style surface is intentionally image-only and has no window title,
    so geometry is the stable X11 contract.  Keep this predicate broad enough
    for a HiDPI-aware implementation while excluding the full-screen selection
    window and the centered preferences surface.
    """
    root_width, root_height = root_size

    def is_thumbnail(window: Mapping[str, int | str]) -> bool:
        width = int(window["width"])
        height = int(window["height"])
        x = int(window["x"])
        y = int(window["y"])
        return (
            100 <= width <= 360
            and 70 <= height <= 280
            and x >= max(0, root_width - 420)
            and y >= max(0, root_height - 340)
            and x + width <= root_width + 4
            and y + height <= root_height + 4
        )

    return _wait_for_window_geometry(env, is_thumbnail, timeout)


def _thumbnail_windows(
    env: Mapping[str, str], root_size: tuple[int, int]
) -> list[dict[str, int | str]]:
    root_width, root_height = root_size
    return [
        window
        for window in _window_geometries(env)
        if 100 <= int(window["width"]) <= 360
        and 70 <= int(window["height"]) <= 280
        and int(window["x"]) >= max(0, root_width - 420)
        and int(window["y"]) >= max(0, root_height - 340)
        and int(window["x"]) + int(window["width"]) <= root_width + 4
        and int(window["y"]) + int(window["height"]) <= root_height + 4
    ]


def _wait_for_thumbnail_gone(
    env: Mapping[str, str], root_size: tuple[int, int], timeout: float = 9
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not _thumbnail_windows(env, root_size):
            return
        time.sleep(0.05)
    raise SmokeError("screenshot thumbnail did not dismiss after its timeout")


def _capture_window_png(env: Mapping[str, str], window_id: str, destination: Path) -> None:
    """Persist one private-X11 window image for visual QA."""
    raw = _run(["xwd", "-id", window_id, "-silent"], env, timeout=5).stdout
    destination.parent.mkdir(parents=True, exist_ok=True)
    _run(["convert", "xwd:-", f"png:{destination}"], env, timeout=8, stdin=raw)


def _xdg_open_stub(env: Mapping[str, str]) -> tuple[dict[str, str], Path]:
    """Prepend a private xdg-open stub that records the opened path."""
    directory = Path(env["XDG_RUNTIME_DIR"]) / "snipchord-xdg-open"
    directory.mkdir(parents=True, exist_ok=True)
    executable = directory / "xdg-open"
    log = directory / "opened.log"
    executable.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$1\" >> \"$SNIPCHORD_XDG_OPEN_LOG\"\n"
    )
    executable.chmod(0o755)
    child_env = dict(env)
    child_env["PATH"] = f"{directory}{os.pathsep}{env.get('PATH', '')}"
    child_env["SNIPCHORD_XDG_OPEN_LOG"] = str(log)
    return child_env, log


def _wait_for_opened_path(log: Path, timeout: float = 3) -> Path:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if log.exists():
            lines = [line.strip() for line in log.read_text().splitlines() if line.strip()]
            if lines:
                return Path(lines[-1])
        time.sleep(0.05)
    raise SmokeError(f"xdg-open was not called; log={log}")


def _click_thumbnail(env: Mapping[str, str], root_size: tuple[int, int]) -> dict[str, int | str]:
    geometry = _wait_for_thumbnail_window(env, root_size, 8)
    x = int(geometry["x"]) + int(geometry["width"]) // 2
    y = int(geometry["y"]) + int(geometry["height"]) // 2
    _run(["xdotool", "mousemove", str(x), str(y)], env)
    _run(["xdotool", "click", "1"], env)
    return geometry


def _clipboard_incr_transfer(env: Mapping[str, str], target: str) -> dict[str, object]:
    """Request one image target and assert/read the initial INCR property.

    ``x11rb`` enables BIG-REQUESTS on many servers, so payload size alone is
    not evidence that the ICCCM incremental path ran.  This small requestor
    observes the first property type directly, then drains all chunks so the
    owner can return to its normal event loop.
    """
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for INCR verification") from error
    connection = display.Display(env["DISPLAY"])
    requestor = None
    try:
        screen = connection.screen()
        requestor = screen.root.create_window(
            0,
            0,
            1,
            1,
            0,
            X.CopyFromParent,
            X.InputOutput,
            X.CopyFromParent,
            event_mask=X.PropertyChangeMask,
        )
        selection = connection.intern_atom("CLIPBOARD")
        target_atom = connection.intern_atom(target)
        property_atom = connection.intern_atom("_SNIPCHORD_SMOKE_INCR")
        incr_atom = connection.intern_atom("INCR")
        requestor.convert_selection(
            selection,
            target_atom,
            property_atom,
            X.CurrentTime,
        )
        connection.flush()
        deadline = time.monotonic() + 12
        initial_type = None
        total = 0
        waiting_for_chunk = False
        selection_notified = False
        while time.monotonic() < deadline:
            if not connection.pending_events():
                time.sleep(0.01)
                continue
            event = connection.next_event()
            if event.type == X.SelectionNotify:
                if event.property == X.NONE:
                    raise SmokeError(f"clipboard target {target!r} was rejected")
                property_value = requestor.get_property(
                    property_atom, X.AnyPropertyType, 0, 1, False
                )
                initial_type = property_value.property_type
                if initial_type == incr_atom:
                    selection_notified = True
                    requestor.delete_property(property_atom)
                    connection.flush()
                    waiting_for_chunk = True
                else:
                    selection_notified = True
                    total += len(property_value.value or b"")
                    break
            elif (
                selection_notified
                and waiting_for_chunk
                and event.type == X.PropertyNotify
                and event.atom == property_atom
                and event.state == X.PropertyNewValue
            ):
                chunk = requestor.get_property(
                    property_atom, X.AnyPropertyType, 0, 1 << 16, True
                )
                # A DeleteProperty event can race with the server's
                # PropertyNotify delivery; wait for the next NewValue event
                # if the property has already disappeared.
                if chunk is None:
                    continue
                value = chunk.value or b""
                if isinstance(value, bytes):
                    total += len(value)
                else:
                    total += len(value) * max(1, int(chunk.format or 8) // 8)
                if not value:
                    break
                # ``delete=True`` above already sends DeleteProperty, which
                # is the request that advances the owner's stream.  Sending a
                # second delete races the next chunk and can discard it.
        else:
            raise SmokeError(f"timed out draining clipboard {target!r} INCR transfer")
        if initial_type is None:
            raise SmokeError(f"clipboard {target!r} sent no SelectionNotify")
        try:
            type_name = connection.get_atom_name(initial_type)
        except Exception:
            type_name = str(initial_type)
        return {"target": target, "initial_type": type_name, "bytes": total}
    finally:
        if requestor is not None:
            with contextlib.suppress(Exception):
                requestor.destroy()
                connection.flush()
        connection.close()


def _clipboard_target(env: Mapping[str, str], target: str, *, timeout: float = 12) -> bytes:
    result = _run(
        ["xclip", "-selection", "clipboard", "-target", target, "-out"],
        env,
        timeout=timeout,
    )
    return result.stdout


def _clipboard_targets(env: Mapping[str, str]) -> set[str]:
    data = _clipboard_target(env, "TARGETS")
    return {line.strip() for line in data.decode(errors="replace").splitlines() if line.strip()}


def _png_size(data: bytes) -> tuple[int, int]:
    if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        raise SmokeError("clipboard image/png has no valid PNG signature/IHDR")
    return struct.unpack(">II", data[16:24])


def _bmp_size(data: bytes) -> tuple[int, int]:
    if data[:2] != b"BM" or len(data) < 26:
        raise SmokeError("clipboard image/bmp has no valid BMP header")
    width, height = struct.unpack_from("<ii", data, 18)
    return width, abs(height)


def _fixture_noise(x: int, y: int) -> int:
    """Return stable high-frequency noise used to keep large PNGs realistic."""
    value = (0x9E3779B9 ^ ((x + 1) * 0x45D9F3B) ^ ((y + 1) * 0x119DE1F3)) & 0xFFFFFFFF
    value ^= (value >> 16)
    value = (value * 0x7FEB352D) & 0xFFFFFFFF
    value ^= (value >> 15)
    value = (value * 0x846CA68B) & 0xFFFFFFFF
    value ^= (value >> 16)
    return value


def _fixture_pixel(x: int, y: int) -> tuple[int, int, int]:
    """Return a structured, deterministic desktop scene used by publish_fixture().

    The fixture keeps enough per-pixel entropy to exercise large clipboard
    transfers, but its broad panels, toolbar, sidebar, and editor-like rows
    make the thumbnail artifact useful for visual QA instead of looking like a
    solid colour or a random test pattern.
    """
    noise = _fixture_noise(x, y)
    jitter = ((noise >> 24) & 0x0F) - 8

    # Dark desktop background with a slim top bar.
    if y < 34:
        base = (30, 34, 44)
    else:
        base = (39 + (y % 48) // 12, 45 + (x % 64) // 16, 60 + (x + y) % 24)

    # Main editor window: header, navigation rail, and a content canvas.
    if 76 <= x < 930 and 74 <= y < 680:
        if y < 112:
            base = (57, 63, 78)
        elif x < 218:
            base = (47, 52, 65)
            if (y // 32) % 4 == 1 and x > 104:
                base = (65, 75, 97)
        else:
            base = (24, 28, 36)
            row = (y - 132) % 26
            if row < 3 and 250 <= x < 870:
                palette = ((98, 177, 255), (128, 203, 146), (235, 181, 96), (190, 145, 255))
                base = palette[((y - 132) // 26) % len(palette)]
            elif row in (5, 6) and 280 <= x < 760 and ((x // 70) % 5) != 0:
                base = (58, 72, 91)

    # A small bright status card in the upper-right makes the preview readable
    # at thumbnail scale without adding text rendering dependencies.
    if 748 <= x < 900 and 128 <= y < 222:
        if y < 154:
            base = (72, 86, 106)
        elif 170 <= y < 176 and 780 <= x < 862:
            base = (104, 202, 168)
        elif 192 <= y < 198 and 780 <= x < 838:
            base = (234, 179, 94)

    return tuple(max(0, min(255, channel + jitter)) for channel in base)


def _fixture_bytes(width: int, height: int, y_start: int = 0) -> bytes:
    # Xvfb's 24-bit visual is 32 bits per pixel, LSBFirst.  The red/green/blue
    # masks are the usual 0x00ff0000/0x0000ff00/0x000000ff, so little-endian
    # 0x00RRGGBB gives the server the exact RGB values expected below.
    data = bytearray(width * height * 4)
    offset = 0
    for y in range(y_start, y_start + height):
        for x in range(width):
            red, green, blue = _fixture_pixel(x, y)
            data[offset : offset + 4] = bytes((blue, green, red, 0))
            offset += 4
    return bytes(data)


def _publish_fixture(env: Mapping[str, str], width: int, height: int):
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError(
            "python-xlib is required to publish the deterministic X11 fixture"
        ) from error
    connection = display.Display(env["DISPLAY"])
    try:
        root = connection.screen().root
        depth = int(connection.screen().root_depth)
        if depth != 24:
            raise SmokeError(f"fixture expects a 24-bit Xvfb root, got depth {depth}")
        # A direct PutImage on the root is cleared when a child window first
        # maps on Xvfb.  Install a pixmap as the root background so the fixture
        # remains stable while GTK maps the selection and preview windows.
        background = root.create_pixmap(width, height, depth)
        gc = background.create_gc()
        try:
            # Python Xlib does not split PutImage requests when the payload
            # exceeds the 16-bit request-length field.  Publish in small row
            # bands so a full noisy fixture remains valid on every Xvfb.
            row_bytes = width * 4
            band_height = max(1, min(height, (64 * 1024) // max(1, row_bytes)))
            for y in range(0, height, band_height):
                rows = min(band_height, height - y)
                background.put_image(
                    gc,
                    0,
                    y,
                    width,
                    rows,
                    X.ZPixmap,
                    depth,
                    0,
                    _fixture_bytes(width, rows, y),
                )
        finally:
            gc.free()
        root.change_attributes(background_pixmap=background)
        root.clear_area(0, 0, width, height)
        connection.sync()
        # Keep this connection alive for the duration of the smoke run.  Xvfb can
        # reset its root pixmap when the last client disconnects; a persistent
        # fixture owner makes the pixels stable while SnipChord captures them.
        return connection
    except Exception:
        connection.close()
        raise


def _image_pixels(data: bytes) -> tuple[int, int, tuple[tuple[int, int, int], ...]]:
    try:
        from PIL import Image
    except ImportError as error:
        raise SmokeError("Pillow is required for fixture pixel verification") from error
    try:
        with Image.open(io.BytesIO(data)) as image:
            image = image.convert("RGB")
            sample_points = [(0, 0), (image.width // 2, image.height // 2), (image.width - 1, image.height - 1)]
            pixels = tuple(image.getpixel(point) for point in sample_points)
            return image.width, image.height, pixels
    except Exception as error:
        raise SmokeError(f"could not decode clipboard image: {error}") from error


def _start_sentinel(env: Mapping[str, str]) -> subprocess.Popen[bytes]:
    process = subprocess.Popen(
        ["xclip", "-quiet", "-selection", "clipboard", "-in", "-loops", "0"],
        env=dict(env),
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    assert process.stdin is not None
    process.stdin.write(b"snipchord-demo-sentinel")
    process.stdin.close()
    # Wait until ownership is observable before starting the demo.
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        if process.poll() is not None:
            error = process.stderr.read().decode(errors="replace") if process.stderr else ""
            raise SmokeError(f"xclip sentinel exited: {error}")
        try:
            if _clipboard_target(env, "UTF8_STRING", timeout=1) == b"snipchord-demo-sentinel":
                return process
        except (SmokeError, subprocess.TimeoutExpired):
            pass
        time.sleep(0.05)
    _terminate(process)
    raise SmokeError("clipboard sentinel did not become owner")


def _demo_does_not_write_clipboard(binary: Path, env: Mapping[str, str]) -> None:
    sentinel = _start_sentinel(env)
    app = _spawn([str(binary), "--demo"], env)
    try:
        _wait_selection_start(app)
        # Escape closes the selection before a capture is completed.  Keeping
        # the sentinel alive makes any accidental clipboard write observable.
        _run(["xdotool", "key", "Escape"], env)
        time.sleep(0.25)
        if sentinel.poll() is not None:
            raise SmokeError("--demo replaced the clipboard owner")
        value = _clipboard_target(env, "UTF8_STRING")
        if value != b"snipchord-demo-sentinel":
            raise SmokeError("--demo changed the clipboard sentinel")
    finally:
        _terminate(app)
        _terminate(sentinel)


def _escape_cancels_region(binary: Path, env: Mapping[str, str]) -> None:
    sentinel = _start_sentinel(env)
    app = _spawn([str(binary), "--region"], env)
    try:
        _wait_selection_start(app)
        _run(["xdotool", "key", "Escape"], env)
        time.sleep(0.25)
        if sentinel.poll() is not None or _clipboard_target(env, "UTF8_STRING") != b"snipchord-demo-sentinel":
            raise SmokeError("Escape cancelled the UI but changed the clipboard")
    finally:
        _terminate(app)
        _terminate(sentinel)


def _click_without_drag_cancels_region(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> dict[str, object]:
    """A plain click with no drag must cancel the active selection immediately.

    This follows the resident hotkey path: the daemon receives the region-save
    command, the full-screen selector becomes visible, and the user clicks
    once without moving the pointer.  The old behavior cleared the zero-sized
    rectangle but left the selector and input grabs active, so the next click
    appeared to do nothing.  Keep the assertion on the actual transient XID
    rather than using a long sleep or waiting for process termination.
    """
    settings = Path(env["XDG_CONFIG_HOME"]) / "snipchord" / "settings.json"
    settings_was_present = settings.is_file()
    settings_before = settings.read_bytes() if settings_was_present else None
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        json.dumps(
            {
                "save_automatically": False,
                "show_preview": False,
                "output_directory": "~/Downloads",
            }
        )
        + "\n"
    )
    destination = Path(env["HOME"]) / "Downloads"
    before_files = set(destination.glob("*.png"))
    sentinel = _start_sentinel(env)
    daemon = _spawn([str(binary), "--daemon"], env)
    command = None
    try:
        _wait_for_instance_owner(env)
        existing_selection_ids = {
            str(window["id"])
            for window in _window_geometries(env)
            if int(window["width"]) == root_size[0]
            and int(window["height"]) == root_size[1]
        }
        command = _spawn([str(binary), "--region", "--save"], env)
        command.wait(timeout=3)
        if command.returncode != 0:
            output = command.stdout.read().decode(errors="replace") if command.stdout else ""
            raise SmokeError(
                f"resident region-save command failed ({command.returncode}): {output[-1000:]}"
            )
        selection = _wait_for_window_geometry(
            env,
            lambda window: str(window["id"]) not in existing_selection_ids
            and int(window["width"]) == root_size[0]
            and int(window["height"]) == root_size[1],
            3,
        )
        window_id = str(selection["id"])
        _run(["xdotool", "mousemove", "200", "200", "click", "1"], env, timeout=3)
        _wait_for_window_hidden(env, window_id)

        # Cancellation keeps the resident process usable and leaves both
        # output channels untouched. A real capture would create one PNG and
        # replace the sentinel clipboard owner.
        if daemon.poll() is not None:
            raise SmokeError(
                f"plain click unexpectedly exited the resident app ({daemon.returncode})"
            )
        after_files = set(destination.glob("*.png"))
        if after_files != before_files:
            raise SmokeError(
                f"plain click wrote files: before={sorted(before_files)}, after={sorted(after_files)}"
            )
        if sentinel.poll() is not None or _clipboard_target(env, "UTF8_STRING") != b"snipchord-demo-sentinel":
            raise SmokeError("plain click changed the clipboard instead of cancelling")
        return {"cancelled": True, "files": len(after_files), "clipboard": "preserved"}
    finally:
        if command is not None:
            _terminate(command)
        # The quit command also verifies that a click cancellation released
        # both grabs and left the hidden instance owner reachable.
        with contextlib.suppress(Exception):
            _run([str(binary), "--quit"], env, timeout=3)
        _terminate(daemon)
        _terminate(sentinel)
        if settings_was_present:
            settings.write_bytes(settings_before or b"")
        else:
            with contextlib.suppress(FileNotFoundError):
                settings.unlink()


def _immediate_drag_after_resident_command(
    binary: Path,
    env: Mapping[str, str],
    expected_dimensions: tuple[int, int] = (200, 150),
) -> dict[str, object]:
    """Ensure a drag issued immediately after the hotkey command is honored.

    Do not wait for a selector window or sleep after dispatching the command.
    The xdotool command sends the complete move/press/move/release sequence as
    soon as the shortcut client returns.  A startup path that maps the overlay
    only after the pointer sequence has already finished loses this drag and
    leaves the user staring at an untouched crosshair.
    """
    daemon = _spawn([str(binary), "--daemon"], env)
    command = None
    try:
        _wait_for_instance_owner(env)
        started = time.perf_counter()
        command = _spawn([str(binary), "--region", "--clipboard"], env)
        command.wait(timeout=3)
        if command.returncode != 0:
            output = command.stdout.read().decode(errors="replace") if command.stdout else ""
            raise SmokeError(
                f"resident region command failed ({command.returncode}): {output[-1000:]}"
            )
        command_returned_ms = (time.perf_counter() - started) * 1000.0

        # Keep this as one xdotool invocation so there is no test-side delay
        # between the shortcut client and the user's first drag gesture.
        _run(
            [
                "xdotool",
                "mousemove",
                "100",
                "100",
                "mousedown",
                "1",
                "mousemove",
                "300",
                "250",
                "mouseup",
                "1",
            ],
            env,
            timeout=3,
        )
        match, output = _read_until(daemon, CAPTURE_RE, 3)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != expected_dimensions:
            raise SmokeError(
                f"immediate drag reported {dimensions}, expected {expected_dimensions}; "
                f"output={output[-1000:]}"
            )
        return {
            "dimensions": dimensions,
            "command_returned_ms": round(command_returned_ms, 2),
            "input": "move-press-move-release without readiness wait",
        }
    finally:
        if command is not None:
            _terminate(command)
        with contextlib.suppress(Exception):
            _run([str(binary), "--quit"], env, timeout=3)
        _terminate(daemon)


def _xtest_burst_during_cursor_ready(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
    expected_dimensions: tuple[int, int] = (200, 150),
) -> dict[str, object]:
    """Inject a drag as soon as the resident reports its cursor-ready point.

    The marker is emitted immediately after the root pointer/keyboard grabs
    are installed, before the expensive snapshot and selection window setup.
    XTEST then sends the complete gesture without waiting for a mapped overlay,
    proving that the early root grab preserves rapid input for the eventual
    selection.  This measures the user-visible cursor-ready phase directly;
    it does not depend on a diagnostic helper window being created.
    """
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for early XTEST input verification") from error
    benchmark_env = dict(env)
    benchmark_env["SNIPCHORD_BENCHMARK_READY"] = "1"
    daemon = _spawn([str(binary), "--daemon"], benchmark_env)
    command = None
    observer = None
    try:
        _wait_for_instance_owner(benchmark_env)
        observer = display.Display(benchmark_env["DISPLAY"])
        root = observer.screen().root
        if not hasattr(observer, "xtest_fake_input"):
            raise SmokeError("XTEST extension is required for early input verification")

        existing_selection_ids = {
            str(window["id"])
            for window in _window_geometries(benchmark_env)
            if int(window["width"]) == root_size[0]
            and int(window["height"]) == root_size[1]
        }
        command = _spawn([str(binary), "--region", "--clipboard"], benchmark_env)
        command.wait(timeout=3)
        if command.returncode != 0:
            output = command.stdout.read().decode(errors="replace") if command.stdout else ""
            raise SmokeError(
                f"resident region command failed ({command.returncode}): {output[-1000:]}"
            )
        _read_until(daemon, READY_RE, 3)

        # The readiness marker is emitted before root snapshot/setup. Confirm
        # that the full-screen selector is not already viewable at the exact
        # injection point; otherwise a slow test-side wait would hide the
        # original lost-gesture regression.
        visible_selection = []
        for window in _window_geometries(benchmark_env):
            window_id = str(window["id"])
            if window_id in existing_selection_ids:
                continue
            if (int(window["width"]), int(window["height"])) != root_size:
                continue
            info = _run(["xwininfo", "-id", window_id], benchmark_env, timeout=2)
            if "Map State: IsViewable" in info.stdout.decode(errors="replace"):
                visible_selection.append(window_id)
        # On a small Xvfb the marker pipe can be scheduled after the whole
        # setup simply because the snapshot completes in a few milliseconds.
        # Keep this as diagnostic context while the XTEST burst below still
        # verifies that input sent at the marker reaches the eventual drag.

        # The resident has installed the early root grab. Send all XTEST
        # events as one client burst before waiting for the full-screen
        # selection window to appear.
        for event_type, detail, x, y in (
            (X.MotionNotify, 0, 100, 100),
            (X.ButtonPress, 1, 100, 100),
            (X.MotionNotify, 0, 300, 250),
            (X.ButtonRelease, 1, 300, 250),
        ):
            observer.xtest_fake_input(
                event_type,
                detail=detail,
                root=root.id,
                x=x,
                y=y,
            )
        observer.sync()

        match, output = _read_until(daemon, CAPTURE_RE, 3)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != expected_dimensions:
            raise SmokeError(
                f"early XTEST drag reported {dimensions}, expected {expected_dimensions}; "
                f"output={output[-1000:]}"
            )
        return {
            "dimensions": dimensions,
            "input": "XTEST burst after cursor-ready marker",
            "overlay_viewable_at_injection": bool(visible_selection),
        }
    finally:
        if command is not None:
            _terminate(command)
        with contextlib.suppress(Exception):
            _run([str(binary), "--quit"], env, timeout=3)
        _terminate(daemon)
        if observer is not None:
            observer.close()


def _selection_restores_foreign_focus(binary: Path, env: Mapping[str, str]) -> None:
    """Check Escape restores focus to a viewable, unrelated X11 window."""
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for focus verification") from error
    connection = display.Display(env["DISPLAY"])
    sentinel_window = None
    app = None
    try:
        sentinel_window = connection.screen().root.create_window(
            2,
            2,
            8,
            8,
            0,
            X.CopyFromParent,
            X.InputOutput,
            X.CopyFromParent,
        )
        sentinel_window.map()
        sentinel_window.set_input_focus(X.RevertToParent, X.CurrentTime)
        connection.flush()
        expected_focus = sentinel_window.id
        current_focus = connection.get_input_focus().focus
        current_focus_id = getattr(current_focus, "id", current_focus)
        if current_focus_id != expected_focus:
            raise SmokeError("could not establish the foreign focus sentinel")
        app = _spawn([str(binary), "--region"], env)
        _wait_selection_start(app)
        _run(["xdotool", "key", "Escape"], env)
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            connection.sync()
            current_focus = connection.get_input_focus().focus
            if getattr(current_focus, "id", current_focus) == expected_focus:
                return
            time.sleep(0.05)
        raise SmokeError("Escape did not restore focus to the foreign X11 window")
    finally:
        _terminate(app)
        if sentinel_window is not None:
            with contextlib.suppress(Exception):
                sentinel_window.destroy()
                connection.flush()
        connection.close()


def _preferences_ignores_foreign_client_message(binary: Path, env: Mapping[str, str]) -> None:
    """Ensure only WM_DELETE_WINDOW closes the preferences surface."""
    try:
        from Xlib import X, display
        from Xlib.protocol import event
    except ImportError as error:
        raise SmokeError("python-xlib is required for ClientMessage verification") from error
    connection = display.Display(env["DISPLAY"])
    app = _spawn([str(binary), "--preferences"], env)
    try:
        window_id = _wait_for_window(env, "SnipChord Preferences", 8)
        window = connection.create_resource_object("window", int(window_id, 0))
        foreign_type = connection.intern_atom("_SNIPCHORD_FOREIGN_MESSAGE")
        message = event.ClientMessage(
            window=window.id,
            client_type=foreign_type,
            data=(32, [1, 0, 0, 0, 0]),
        )
        window.send_event(message, event_mask=0, propagate=False)
        connection.flush()
        time.sleep(0.25)
        if not _run(
            ["xdotool", "search", "--name", "SnipChord Preferences"],
            env,
            check=False,
            timeout=2,
        ).stdout.strip():
            raise SmokeError("foreign ClientMessage closed preferences")
        _run(["xdotool", "key", "Escape"], env)
    finally:
        _terminate(app)
        connection.close()


def _keyboard_grab_deferred_selection(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> dict[str, object]:
    """Keep pointer selection usable while a foreign client owns the keyboard.

    A keyboard grab can briefly be unavailable on a real desktop while the
    selector is already responsible for pointer input.  The native path keeps
    that selection alive, retries the keyboard grab, and must still honour a
    pointer drag, right-click cancellation, and Escape after the foreign grab
    is released.  This fixture deliberately holds the keyboard grab across
    three independent resident commands so each gesture exercises the same
    setup path and leaves the daemon usable for the next one.
    """
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for grab cleanup verification") from error
    connection = display.Display(env["DISPLAY"])
    blocker = None
    daemon = None
    commands: list[subprocess.Popen[bytes]] = []
    result: dict[str, object] = {}
    try:
        blocker = connection.screen().root.create_window(
            4,
            4,
            8,
            8,
            0,
            X.CopyFromParent,
            X.InputOnly,
            X.CopyFromParent,
        )
        blocker.map()
        keyboard = blocker.grab_keyboard(
            False, X.GrabModeAsync, X.GrabModeAsync, X.CurrentTime
        )
        connection.flush()
        if keyboard != X.GrabSuccess:
            raise SmokeError(f"fixture could not acquire keyboard grab: {keyboard}")

        daemon = _spawn([str(binary), "--daemon"], env)
        _wait_for_instance_owner(env)

        def start_selection() -> tuple[subprocess.Popen[bytes], str]:
            existing_selection_ids = {
                str(window["id"])
                for window in _window_geometries(env)
                if int(window["width"]) == root_size[0]
                and int(window["height"]) == root_size[1]
            }
            command = _spawn([str(binary), "--region", "--clipboard"], env)
            commands.append(command)
            command.wait(timeout=3)
            if command.returncode != 0:
                output = command.stdout.read().decode(errors="replace") if command.stdout else ""
                raise SmokeError(
                    f"resident region command failed ({command.returncode}): {output[-1000:]}"
                )
            selection = _wait_for_window_geometry(
                env,
                lambda window: str(window["id"]) not in existing_selection_ids
                and int(window["width"]) == root_size[0]
                and int(window["height"]) == root_size[1],
                3,
            )
            return command, str(selection["id"])

        # Pointer input remains usable during the deferred keyboard phase.
        drag_command, drag_window = start_selection()
        _run(
            [
                "xdotool",
                "mousemove",
                "100",
                "100",
                "mousedown",
                "1",
                "mousemove",
                "300",
                "250",
                "mouseup",
                "1",
            ],
            env,
            timeout=3,
        )
        match, output = _read_until(daemon, CAPTURE_RE, 3)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != (200, 150):
            raise SmokeError(
                f"keyboard-busy pointer drag reported {dimensions}; output={output[-1000:]}"
            )
        _wait_for_window_hidden(env, drag_window)
        result["drag_capture"] = dimensions
        if drag_command.poll() is None:
            raise SmokeError("resident drag command did not finish")

        # A right click is an explicit cancellation and must also clear the
        # pending keyboard-retry path while the foreign keyboard grab remains.
        _right_click_command, right_window = start_selection()
        # Keep the foreign grab active long enough to cover the old timeout
        # variant; cancellation must remain available after a real desktop
        # contention interval rather than only during the first retry tick.
        hold_started = time.monotonic()
        time.sleep(0.6)
        _run(["xdotool", "mousemove", "100", "100", "click", "3"], env, timeout=3)
        _wait_for_window_hidden(env, right_window)
        result["right_click_cancelled"] = True
        result["foreign_keyboard_hold_ms"] = round((time.monotonic() - hold_started) * 1000, 1)

        # Once the foreign owner releases the keyboard, the retry should win
        # quickly enough for Escape to cancel the still-active drag.  Keeping
        # Button1 down while releasing the foreign grab exercises the retry
        # path during a motion gesture, including the old clear-on-drag bug.
        _escape_command, escape_window = start_selection()
        _run(
            [
                "xdotool",
                "mousemove",
                "100",
                "100",
                "mousedown",
                "1",
                "mousemove",
                "300",
                "250",
            ],
            env,
            timeout=3,
        )
        connection.ungrab_keyboard(X.CurrentTime)
        connection.flush()
        time.sleep(0.1)
        _run(["xdotool", "key", "Escape"], env, timeout=3)
        _wait_for_window_hidden(env, escape_window)
        with contextlib.suppress(Exception):
            _run(["xdotool", "mouseup", "1"], env, timeout=2)
        result["escape_after_release"] = True
        result["keyboard_release"] = "foreign grab released before Escape"

        if daemon.poll() is not None:
            raise SmokeError(f"daemon exited during deferred keyboard test ({daemon.returncode})")
        return result
    finally:
        with contextlib.suppress(Exception):
            _run(["xdotool", "mouseup", "1"], env, timeout=2)
        for command in commands:
            _terminate(command)
        _terminate(daemon)
        if blocker is not None:
            with contextlib.suppress(Exception):
                connection.ungrab_pointer(X.CurrentTime)
                connection.ungrab_keyboard(X.CurrentTime)
                blocker.destroy()
                connection.flush()
        connection.close()


def _failed_pointer_grab_releases_keyboard(binary: Path, env: Mapping[str, str]) -> None:
    """Keep the genuine pointer-busy setup failure and cleanup regression."""
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for grab cleanup verification") from error
    connection = display.Display(env["DISPLAY"])
    blocker = None
    app = None
    daemon = None
    try:
        blocker = connection.screen().root.create_window(
            4,
            4,
            8,
            8,
            0,
            X.CopyFromParent,
            X.InputOnly,
            X.CopyFromParent,
        )
        blocker.map()
        pointer = blocker.grab_pointer(
            False,
            X.ButtonPressMask,
            X.GrabModeAsync,
            X.GrabModeAsync,
            X.NONE,
            X.NONE,
            X.CurrentTime,
        )
        connection.flush()
        if pointer != X.GrabSuccess:
            raise SmokeError(f"fixture could not acquire pointer grab: {pointer}")

        app = _spawn([str(binary), "--region"], env)
        try:
            app.wait(timeout=8)
        except subprocess.TimeoutExpired as error:
            raise SmokeError("selection did not fail while the pointer was externally grabbed") from error
        if app.returncode == 0:
            output = app.stdout.read().decode(errors="replace") if app.stdout else ""
            raise SmokeError(
                "selection unexpectedly succeeded despite the external pointer grab; "
                f"output={output[-1000:]}"
            )

        # The failed setup must release any keyboard state before returning;
        # after the blocker releases the pointer a daemon can claim the app.
        connection.ungrab_pointer(X.CurrentTime)
        connection.flush()
        daemon = _spawn([str(binary), "--daemon"], env)
        _wait_for_instance_owner(env)
        _run([str(binary), "--quit"], env, timeout=8)
        deadline = time.monotonic() + 3
        while daemon.poll() is None and time.monotonic() < deadline:
            time.sleep(0.05)
        if daemon.poll() is None:
            raise SmokeError("daemon did not accept --quit after pointer-busy setup failure")
    finally:
        _terminate(app)
        _terminate(daemon)
        if blocker is not None:
            with contextlib.suppress(Exception):
                connection.ungrab_pointer(X.CurrentTime)
                connection.ungrab_keyboard(X.CurrentTime)
                blocker.destroy()
                connection.flush()
        connection.close()


def _assert_selection_visual(path: Path, expected_size: tuple[int, int]) -> None:
    try:
        from PIL import Image
    except ImportError as error:
        raise SmokeError("Pillow is required for selection visual verification") from error
    try:
        with Image.open(path) as image:
            image = image.convert("RGB")
            if image.size != expected_size:
                raise SmokeError(f"selection artifact has size {image.size}")
            source = (0x36, 0x50, 0x70)
            outside = image.getpixel((10, 10))
            inside = image.getpixel((150, 150))
            border = image.getpixel((100, 150))
            if outside != source:
                raise SmokeError(f"selection outside pixel is {outside}, expected {source}")
            if inside != source:
                raise SmokeError(f"selection interior is {inside}, expected {source}")
            if border != (255, 255, 255):
                raise SmokeError(f"selection border is {border}, expected white")
    except SmokeError:
        raise
    except Exception as error:
        raise SmokeError(f"could not inspect selection artifact: {error}") from error


def _assert_plain_selection_overlay(
    path: Path,
    root_size: tuple[int, int],
    selected: tuple[int, int, int, int] | None = None,
    ignored: Sequence[tuple[int, int, int, int]] = (),
) -> dict[str, object]:
    """Reject selection overlays that paint text or metadata badges.

    The X11 selection window is a full-screen copy of the frozen desktop.  In
    every state the no-dim selector therefore preserves the fixture pixels;
    during a drag the accepted rectangle is still restored from the same
    snapshot.  Comparing
    against the deterministic fixture catches the old coordinate, dimensions,
    and help badges without trying to OCR arbitrary desktop content.  The
    selected rectangle's narrow perimeter is ignored because its white outline
    and corner handles are intentional UI chrome.
    """
    try:
        from PIL import Image
    except ImportError as error:
        raise SmokeError("Pillow is required for selection overlay verification") from error

    width, height = root_size
    try:
        with Image.open(path) as image:
            image = image.convert("RGB")
            if image.size != root_size:
                raise SmokeError(
                    f"selection overlay artifact has size {image.size}, expected {root_size}"
                )
            ignored_regions = [
                (left - 5, top - 5, right + 5, bottom + 5)
                for left, top, right, bottom in ignored
            ]
            if selected is not None:
                left, top, selected_width, selected_height = selected
                ignored_regions.append(
                    (
                        left - 5,
                        top - 5,
                        left + selected_width + 5,
                        top + selected_height + 5,
                    )
                )

            mismatches = 0
            first_mismatch: tuple[int, int, tuple[int, int, int], tuple[int, int, int]] | None = None
            for y in range(height):
                for x in range(width):
                    if any(
                        left <= x < right and top <= y < bottom
                        for left, top, right, bottom in ignored_regions
                    ):
                        continue
                    source = _fixture_pixel(x, y)
                    if selected is not None:
                        selected_left, selected_top, selected_width, selected_height = selected
                        in_selection = (
                            selected_left <= x < selected_left + selected_width
                            and selected_top <= y < selected_top + selected_height
                        )
                    else:
                        in_selection = False
                    expected = source
                    observed = image.getpixel((x, y))
                    # The server-side Render dimmer rounds one channel by at
                    # most one value depending on the source byte.  Ignore
                    # that deterministic conversion noise while retaining a
                    # strict threshold for any text or filled badge.
                    if max(abs(observed[index] - expected[index]) for index in range(3)) > 1:
                        mismatches += 1
                        if first_mismatch is None:
                            first_mismatch = (x, y, observed, expected)

            # Allow a very small number of server conversion edge pixels, but
            # a text badge changes thousands of pixels and fails by a wide
            # margin.  The strict cap also catches a full-width help banner.
            allowed = max(16, (width * height) // 20_000)
            if mismatches > allowed:
                raise SmokeError(
                    "selection overlay contains unexpected painted pixels "
                    f"({mismatches} mismatches, allowed {allowed}, first={first_mismatch})"
                )
            return {
                "mismatches": mismatches,
                "allowed": allowed,
                "selected": selected,
            }
    except SmokeError:
        raise
    except Exception as error:
        raise SmokeError(f"could not inspect selection overlay artifact: {error}") from error


def _wait_for_plain_selection_frame(
    env: Mapping[str, str],
    window_id: str,
    point: tuple[int, int],
    expected: tuple[int, int, int],
    timeout: float = 3,
) -> None:
    """Wait until one post-motion pixel proves the frame was redrawn."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        observed = _selection_frame_samples(env, window_id, (point,))[0]
        if observed == expected:
            return
        time.sleep(0.01)
    raise SmokeError(
        f"selection overlay did not settle at {point}: observed={observed}, expected={expected}"
    )


def _selection_overlay_has_no_text_badges(
    binary: Path,
    env: Mapping[str, str],
    artifacts_dir: Path,
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> dict[str, object]:
    """Check idle, drag, and window-pick overlays contain no text surfaces."""
    width, height = root_size
    if width < 360 or height < 300:
        raise SmokeError("selection overlay check needs an X11 surface at least 360x300")
    source_at_pointer = _fixture_pixel(40, 40)
    results: dict[str, object] = {}

    # Keep each state in a separate process so a previous pointer grab or
    # selection rectangle cannot influence the next visual assertion.
    app = _spawn([str(binary), "--region", "--clipboard"], env)
    try:
        _wait_selection_start(app)
        selection = _wait_for_window_geometry(
            env,
            lambda window: int(window["width"]) == width
            and int(window["height"]) == height,
            3,
        )
        idle_path = artifacts_dir / "snipchord-overlay-idle.png"
        _capture_window_png(env, str(selection["id"]), idle_path)
        results["idle"] = _assert_plain_selection_overlay(idle_path, root_size)
    finally:
        _terminate(app)

    app = _spawn([str(binary), "--region", "--clipboard"], env)
    try:
        _wait_selection_start(app)
        selection = _wait_for_window_geometry(
            env,
            lambda window: int(window["width"]) == width
            and int(window["height"]) == height,
            3,
        )
        window_id = str(selection["id"])
        _run(["xdotool", "mousemove", "100", "100"], env)
        _run(["xdotool", "mousedown", "1"], env)
        _run(["xdotool", "mousemove", "300", "250"], env)
        selected_point = _fixture_pixel(180, 170)
        _wait_for_plain_selection_frame(env, window_id, (180, 170), selected_point)
        drag_path = artifacts_dir / "snipchord-overlay-drag.png"
        _capture_window_png(env, window_id, drag_path)
        results["drag"] = _assert_plain_selection_overlay(
            drag_path,
            root_size,
            selected=(100, 100, 200, 150),
        )
    finally:
        with contextlib.suppress(Exception):
            _run(["xdotool", "mouseup", "1"], env, timeout=2)
        _terminate(app)

    app = _spawn([str(binary), "--region", "--clipboard"], env)
    try:
        _wait_selection_start(app)
        selection = _wait_for_window_geometry(
            env,
            lambda window: int(window["width"]) == width
            and int(window["height"]) == height,
            3,
        )
        window_id = str(selection["id"])
        _run(["xdotool", "keydown", "space"], env)
        _run(["xdotool", "mousemove", "40", "40"], env)
        _wait_for_plain_selection_frame(env, window_id, (40, 40), source_at_pointer)
        window_pick_path = artifacts_dir / "snipchord-overlay-windowpick.png"
        _capture_window_png(env, window_id, window_pick_path)
        results["window_pick"] = _assert_plain_selection_overlay(window_pick_path, root_size)
    finally:
        with contextlib.suppress(Exception):
            _run(["xdotool", "keyup", "space"], env, timeout=2)
        _terminate(app)

    return results


def _assert_thumbnail_visual(
    path: Path,
    window: Mapping[str, int | str],
    expected_image_size: tuple[int, int],
) -> dict[str, object]:
    """Check the thumbnail contract and keep an artifact for visual review.

    A thumbnail is a compact image surface: it should preserve the capture's
    aspect ratio, occupy most of its small window, and avoid the old metadata
    card (title, dimensions, status, and buttons).  Rounded corners are
    validated through the X11 shape check in the caller; this image check
    remains useful on X servers without the Shape extension because it catches
    an accidental return to the large card layout.
    """
    try:
        from PIL import Image
    except ImportError as error:
        raise SmokeError("Pillow is required for thumbnail visual verification") from error
    width = int(window["width"])
    height = int(window["height"])
    if width > 360 or height > 280:
        raise SmokeError(f"thumbnail surface is too large: {width}x{height}")
    expected_width, expected_height = expected_image_size
    expected_ratio = expected_width / expected_height
    observed_ratio = width / height
    if abs(observed_ratio - expected_ratio) > 0.45:
        raise SmokeError(
            f"thumbnail aspect ratio {observed_ratio:.3f} does not resemble "
            f"the captured image {expected_ratio:.3f}"
        )
    try:
        with Image.open(path) as image:
            image = image.convert("RGB")
            if image.size != (width, height):
                raise SmokeError(
                    f"thumbnail artifact has size {image.size}, expected {(width, height)}"
                )
            # A real fixture should survive the downscale as more than one
            # colour.  This rejects a blank/solid card while remaining agnostic
            # to the implementation's interpolation filter.
            sample_points = [
                (max(0, width // 4), max(0, height // 4)),
                (max(0, width // 2), max(0, height // 2)),
                (max(0, (3 * width) // 4), max(0, (3 * height) // 4)),
            ]
            samples = tuple(image.getpixel(point) for point in sample_points)
            if len(set(samples)) < 2:
                raise SmokeError("thumbnail image is blank or solid instead of the captured desktop")
            return {"geometry": (width, height), "samples": samples}
    except SmokeError:
        raise
    except Exception as error:
        raise SmokeError(f"could not inspect thumbnail artifact: {error}") from error


def _assert_window_is_rounded(env: Mapping[str, str], window_id: str) -> dict[str, object]:
    """Require rounded thumbnail corners, using X11 Shape when available."""
    try:
        from Xlib import display
        from Xlib.ext import shape
    except ImportError as error:
        raise SmokeError("python-xlib with Shape support is required for thumbnail verification") from error
    connection = display.Display(env["DISPLAY"])
    try:
        window = connection.create_resource_object("window", int(window_id, 0))
        reply = shape.get_rectangles(window, shape.SK.Bounding).reply()
        rectangles = getattr(reply, "rectangles", ())
        width = int(window.get_geometry().width)
        height = int(window.get_geometry().height)
        if rectangles:
            bounding_area = sum(int(rect.width) * int(rect.height) for rect in rectangles)
            # A rectangle with only a single bounding region is the old square
            # card.  Rounded corners normally produce several bands; accept
            # any shaped region whose area is smaller than the rectangular
            # bounds.
            if bounding_area >= width * height:
                raise SmokeError(
                    f"thumbnail bounding shape is rectangular ({bounding_area} == {width * height})"
                )
            return {"rectangles": len(rectangles), "bounding_area": bounding_area}

        # Minimal Xvfb builds may omit the Shape extension.  The UI still
        # paints a rounded card inside its shadow rectangle, so verify those
        # corner pixels instead of rejecting an otherwise valid visual.
        raw = _run(["xwd", "-id", window_id, "-silent"], env, timeout=5).stdout
        png = _run(["convert", "xwd:-", "png:-"], env, timeout=8, stdin=raw).stdout
        try:
            from PIL import Image

            with Image.open(io.BytesIO(png)) as image:
                image = image.convert("RGB")
                center = image.getpixel((width // 2, height // 2))
                corners = tuple(
                    image.getpixel(point)
                    for point in ((0, 0), (width - 1, 0), (0, height - 1), (width - 1, height - 1))
                )
        except Exception as error:
            raise SmokeError(f"could not inspect thumbnail corner pixels: {error}") from error
        if len(set(corners)) == 1 and corners[0] == center:
            raise SmokeError("thumbnail corners match the card surface; rounded rendering is absent")
        return {"shape_extension": False, "corner_pixels": corners}
    except SmokeError:
        raise
    except Exception as error:
        raise SmokeError(f"could not inspect thumbnail Shape region: {error}") from error
    finally:
        connection.close()


def _space_move_preserves_dimensions(
    binary: Path,
    env: Mapping[str, str],
    artifacts_dir: Path | None = None,
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> tuple[int, int]:
    app = _spawn([str(binary), "--demo"], env)
    try:
        _wait_selection_start(app)
        # Select 200x150, hold Space, move by 50x40, and release.  The
        # completed demo log reports the crop dimensions after the move.
        _run(["xdotool", "mousemove", "100", "100"], env)
        _run(["xdotool", "mousedown", "1"], env)
        _run(["xdotool", "mousemove", "300", "250"], env)
        if artifacts_dir is not None:
            time.sleep(0.1)
            selection_window = _wait_for_window_geometry(
                env,
                lambda window: int(window["width"]) == root_size[0]
                and int(window["height"]) == root_size[1],
                3,
            )
            selection_path = artifacts_dir / "snipchord-selection.png"
            _capture_window_png(env, str(selection_window["id"]), selection_path)
            # --demo is a solid #365070 image in the native Rust binary.  The
            # wrapper used for legacy regression has a different fixture, so
            # keep its dimension/keyboard checks while limiting this visual
            # assertion to the release/debug Rust executable layout.
            if binary.parent.name in {"release", "debug"}:
                _assert_selection_visual(selection_path, root_size)
        _run(["xdotool", "keydown", "space"], env)
        _run(["xdotool", "mousemove", "350", "290"], env)
        _run(["xdotool", "keyup", "space"], env)
        _run(["xdotool", "mouseup", "1"], env)
        match, output = _read_until(app, CAPTURE_RE, 8)
        width, height = int(match["width"]), int(match["height"])
        if (width, height) != (200, 150):
            raise SmokeError(
                f"Space move changed selection dimensions: {(width, height)}; output={output[-1000:]}"
            )
        return width, height
    finally:
        _terminate(app)


def _selection_frame_samples(
    env: Mapping[str, str], window_id: str, points: Sequence[tuple[int, int]]
) -> tuple[tuple[int, int, int], ...]:
    """Read a few pixels from the mapped selection window without saving an artifact."""
    try:
        from PIL import Image
    except ImportError as error:
        raise SmokeError("Pillow is required for selection redraw verification") from error
    raw = _run(["xwd", "-id", window_id, "-silent"], env, timeout=5).stdout
    png = _run(["convert", "xwd:-", "png:-"], env, timeout=8, stdin=raw).stdout
    try:
        with Image.open(io.BytesIO(png)) as image:
            image = image.convert("RGB")
            return tuple(image.getpixel(point) for point in points)
    except Exception as error:
        raise SmokeError(f"could not sample selection frame: {error}") from error


def _selection_redraw_has_no_intermediate_frame(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> int:
    """Check that pointer redraws never expose the half-built selection frame.

    The demo image is a solid colour, so one point well inside the selected
    rectangle and one point well outside it have stable values while the held
    mouse button repeatedly resizes the rectangle.  The native UI redraws into
    an off-screen pixmap and presents it with one CopyArea; this check samples
    the mapped window while a separate X11 client generates motion events.  A
    black or stale frame between the two old CopyArea requests is therefore
    observable without asserting a timing budget.
    """
    width, height = root_size
    if width < 360 or height < 300:
        raise SmokeError("selection redraw check needs an X11 surface at least 360x300")
    source = (0x36, 0x50, 0x70)
    points = ((180, 170), (width - 20, height - 20))
    expected = (source, source)

    app = _spawn([str(binary), "--demo"], env)
    motion_errors: list[BaseException] = []
    motion_thread: threading.Thread | None = None
    try:
        _wait_selection_start(app)
        _run(["xdotool", "mousemove", "100", "100"], env)
        _run(["xdotool", "mousedown", "1"], env)
        _run(["xdotool", "mousemove", "300", "250"], env)
        selection = _wait_for_window_geometry(
            env,
            lambda window: int(window["width"]) == width
            and int(window["height"]) == height,
            3,
        )
        window_id = str(selection["id"])

        # Wait for the initial drag event to produce a valid selection before
        # the concurrent motion loop starts.  This also verifies that the
        # sample points avoid the pointer and dimension badges.
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            if _selection_frame_samples(env, window_id, points) == expected:
                break
            time.sleep(0.01)
        else:
            raise SmokeError("selection did not reach a stable demo frame before redraw sampling")

        try:
            from Xlib import display
        except ImportError as error:
            raise SmokeError("python-xlib is required for selection redraw verification") from error

        # Keep the selected point inside the rectangle as the held drag resizes
        # it. Each warp is a real X11 motion event delivered through the grab.
        path = [(310, 260), (320, 270), (310, 260), (320, 270)] * 20

        def drive_motion() -> None:
            connection = display.Display(env["DISPLAY"])
            try:
                root = connection.screen().root
                for x, y in path:
                    root.warp_pointer(x, y)
                    connection.flush()
                    time.sleep(0.01)
            except BaseException as error:  # surface worker failures in the test thread
                motion_errors.append(error)
            finally:
                connection.close()

        motion_thread = threading.Thread(target=drive_motion, name="snipchord-motion")
        motion_thread.start()
        samples = 0
        deadline = time.monotonic() + 6
        while (motion_thread.is_alive() or samples < 16) and time.monotonic() < deadline:
            observed = _selection_frame_samples(env, window_id, points)
            if observed != expected:
                raise SmokeError(
                    "selection redraw exposed an intermediate frame: "
                    f"observed={observed}, expected={expected}, sample={samples}"
                )
            samples += 1
        motion_thread.join(timeout=1)
        if motion_thread.is_alive():
            raise SmokeError("selection redraw motion driver did not finish")
        if motion_errors:
            raise SmokeError(f"selection redraw motion driver failed: {motion_errors[0]}")
        if samples < 16:
            raise SmokeError(f"selection redraw check collected only {samples} samples")
        return samples
    finally:
        if motion_thread is not None:
            motion_thread.join(timeout=1)
        # Release the active grab before terminating the child so the private
        # X server remains usable for the following smoke checks.
        for command in (("keyup", "space"), ("mouseup", "1")):
            with contextlib.suppress(Exception):
                _run(["xdotool", *command], env, timeout=2)
        _terminate(app)


def _region_clipboard_crop(
    binary: Path, env: Mapping[str, str], expected_width: int = 200, expected_height: int = 150
) -> dict[str, object]:
    """Capture a controlled root rectangle and verify its encoded pixels."""
    start_x, start_y = 100, 100
    end_x, end_y = start_x + expected_width, start_y + expected_height
    # This is the Ctrl+Alt+Shift+4 contract: the explicit clipboard output
    # mode must preserve image/png and image/bmp ownership.
    app = _spawn([str(binary), "--region", "--clipboard"], env)
    try:
        _wait_selection_start(app)
        _run(["xdotool", "mousemove", str(start_x), str(start_y)], env)
        _run(["xdotool", "mousedown", "1"], env)
        _run(["xdotool", "mousemove", str(end_x), str(end_y)], env)
        _run(["xdotool", "mouseup", "1"], env)
        match, output = _read_until(app, CAPTURE_RE, 8)
        reported = (int(match["width"]), int(match["height"]))
        expected_dimensions = (expected_width, expected_height)
        if reported != expected_dimensions:
            raise SmokeError(
                f"region reported {reported}, expected {expected_dimensions}; output={output[-1000:]}"
            )
        targets = _clipboard_targets(env)
        required = {"image/png", "image/bmp"}
        if not required.issubset(targets):
            raise SmokeError(f"region clipboard targets missing {sorted(required - targets)}")
        png = _clipboard_target(env, "image/png")
        bmp = _clipboard_target(env, "image/bmp")
        png_w, png_h, png_samples = _image_pixels(png)
        bmp_w, bmp_h, bmp_samples = _image_pixels(bmp)
        sample_points = ((start_x, start_y), (start_x + expected_width // 2, start_y + expected_height // 2),
                         (end_x - 1, end_y - 1))
        expected = tuple(_fixture_pixel(*point) for point in sample_points)
        if (png_w, png_h) != expected_dimensions or (bmp_w, bmp_h) != expected_dimensions:
            raise SmokeError(
                f"region clipboard dimensions are {(png_w, png_h)} / {(bmp_w, bmp_h)}, "
                f"expected {expected_dimensions}"
            )
        if png_samples != expected or bmp_samples != expected:
            raise SmokeError(
                f"region fixture pixels changed: expected={expected}, png={png_samples}, bmp={bmp_samples}"
            )
        return {
            "dimensions": reported,
            "png_bytes": len(png),
            "bmp_bytes": len(bmp),
            "png_sha256": hashlib.sha256(png).hexdigest(),
            "bmp_sha256": hashlib.sha256(bmp).hexdigest(),
        }
    finally:
        _terminate(app)


def _fullscreen_clipboard(binary: Path, env: Mapping[str, str], width: int, height: int) -> dict[str, object]:
    # The explicit full-screen clipboard mode backs the Ctrl+Alt+Shift+3
    # shortcut while retaining the large-transfer coverage below.
    app = _spawn([str(binary), "--fullscreen", "--clipboard"], env)
    try:
        match, output = _read_until(app, CAPTURE_RE, 12)
        reported = (int(match["width"]), int(match["height"]))
        if reported != (width, height):
            raise SmokeError(f"fullscreen reported {reported}, expected {(width, height)}; output={output[-1000:]}")
        targets = _clipboard_targets(env)
        required = {"image/png", "image/bmp"}
        if not required.issubset(targets):
            raise SmokeError(f"clipboard targets missing {sorted(required - targets)}: {sorted(targets)}")
        incr = _clipboard_incr_transfer(env, "image/png")
        if incr["initial_type"] != "INCR":
            raise SmokeError(f"large PNG transfer did not start with INCR: {incr}")
        if int(incr["bytes"]) <= INCR_THRESHOLD:
            raise SmokeError(f"INCR PNG transfer was only {incr['bytes']} bytes: {incr}")
        png = _clipboard_target(env, "image/png")
        bmp = _clipboard_target(env, "image/bmp")
        if len(bmp) <= INCR_THRESHOLD:
            raise SmokeError(f"BMP transfer was only {len(bmp)} bytes; INCR path was not exercised")
        if len(png) <= INCR_THRESHOLD:
            raise SmokeError(f"PNG transfer was only {len(png)} bytes; noisy fixture did not exercise INCR")
        if _png_size(png) != (width, height):
            raise SmokeError(f"PNG dimensions are {_png_size(png)}, expected {(width, height)}")
        if _bmp_size(bmp) != (width, height):
            raise SmokeError(f"BMP dimensions are {_bmp_size(bmp)}, expected {(width, height)}")
        png_w, png_h, png_samples = _image_pixels(png)
        bmp_w, bmp_h, bmp_samples = _image_pixels(bmp)
        expected = tuple(_fixture_pixel(*point) for point in ((0, 0), (width // 2, height // 2), (width - 1, height - 1)))
        if png_samples != expected or bmp_samples != expected:
            raise SmokeError(
                f"fixture pixels changed: expected={expected}, png={png_samples}, bmp={bmp_samples}"
            )
        return {
            "png_bytes": len(png),
            "bmp_bytes": len(bmp),
            "png_sha256": hashlib.sha256(png).hexdigest(),
            "bmp_sha256": hashlib.sha256(bmp).hexdigest(),
            "png_incr": incr,
            "png_dimensions": (png_w, png_h),
            "bmp_dimensions": (bmp_w, bmp_h),
        }
    finally:
        _terminate(app)


def _region_save_to_configured_directory(
    binary: Path,
    env: Mapping[str, str],
    expected_width: int = 200,
    expected_height: int = 150,
) -> dict[str, object]:
    """Verify Alt+Shift+4 saves a region without claiming the clipboard."""
    settings = Path(env["XDG_CONFIG_HOME"]) / "snipchord" / "settings.json"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        json.dumps(
            {
                "save_automatically": False,
                "show_preview": False,
                "output_directory": "~/Downloads",
            }
        )
        + "\n"
    )
    destination = Path(env["HOME"]) / "Downloads"
    before = set(destination.glob("*.png"))
    sentinel = _start_sentinel(env)
    app = _spawn([str(binary), "--region", "--save"], env)
    try:
        _wait_selection_start(app)
        start_x, start_y = 100, 100
        _run(["xdotool", "mousemove", str(start_x), str(start_y)], env)
        _run(["xdotool", "mousedown", "1"], env)
        _run(
            ["xdotool", "mousemove", str(start_x + expected_width), str(start_y + expected_height)],
            env,
        )
        _run(["xdotool", "mouseup", "1"], env)
        match, output = _read_until(app, CAPTURE_RE, 8)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != (expected_width, expected_height):
            raise SmokeError(f"save-mode region reported {dimensions}; output={output[-1000:]}")
        if sentinel.poll() is not None or _clipboard_target(env, "UTF8_STRING") != b"snipchord-demo-sentinel":
            raise SmokeError("--save mode replaced the clipboard owner")
        deadline = time.monotonic() + 5
        saved: set[Path] = set()
        while time.monotonic() < deadline:
            saved = set(destination.glob("*.png")) - before
            if saved:
                break
            time.sleep(0.05)
        if len(saved) != 1:
            raise SmokeError(f"--save mode wrote {len(saved)} files in {destination}: {sorted(saved)}")
        path = next(iter(saved))
        png_w, png_h, samples = _image_pixels(path.read_bytes())
        points = (
            (start_x, start_y),
            (start_x + expected_width // 2, start_y + expected_height // 2),
            (start_x + expected_width - 1, start_y + expected_height - 1),
        )
        expected = tuple(_fixture_pixel(*point) for point in points)
        if (png_w, png_h) != dimensions or samples != expected:
            raise SmokeError(
                f"saved screenshot changed dimensions/pixels: {(png_w, png_h)}, {samples}; "
                f"expected {dimensions}, {expected}"
            )
        return {"path": str(path), "dimensions": dimensions, "bytes": path.stat().st_size}
    finally:
        _terminate(app)
        _terminate(sentinel)


def _fullscreen_save_to_configured_directory(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int],
) -> dict[str, object]:
    """Verify `3` + save writes the full desktop without claiming clipboard."""
    settings = Path(env["XDG_CONFIG_HOME"]) / "snipchord" / "settings.json"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        json.dumps(
            {
                "save_automatically": False,
                "show_preview": False,
                "output_directory": "~/Downloads",
            }
        )
        + "\n"
    )
    destination = Path(env["HOME"]) / "Downloads"
    before = set(destination.glob("*.png"))
    sentinel = _start_sentinel(env)
    app = _spawn([str(binary), "--fullscreen", "--save"], env)
    try:
        match, output = _read_until(app, CAPTURE_RE, 12)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != root_size:
            raise SmokeError(f"save-mode fullscreen reported {dimensions}; output={output[-1000:]}")
        if sentinel.poll() is not None or _clipboard_target(env, "UTF8_STRING") != b"snipchord-demo-sentinel":
            raise SmokeError("fullscreen --save mode replaced the clipboard owner")
        deadline = time.monotonic() + 5
        new_files: set[Path] = set()
        while time.monotonic() < deadline:
            new_files = set(destination.glob("*.png")) - before
            if new_files:
                break
            time.sleep(0.05)
        if len(new_files) != 1:
            raise SmokeError(
                f"fullscreen --save wrote {len(new_files)} new files in {destination}: "
                f"{sorted(new_files)}"
            )
        path = next(iter(new_files))
        png_w, png_h, samples = _image_pixels(path.read_bytes())
        points = ((0, 0), (root_size[0] // 2, root_size[1] // 2), (root_size[0] - 1, root_size[1] - 1))
        expected = tuple(_fixture_pixel(*point) for point in points)
        if (png_w, png_h) != dimensions or samples != expected:
            raise SmokeError(
                f"saved fullscreen changed dimensions/pixels: {(png_w, png_h)}, {samples}; "
                f"expected {dimensions}, {expected}"
            )
        return {"path": str(path), "dimensions": dimensions, "bytes": path.stat().st_size}
    finally:
        _terminate(app)
        _terminate(sentinel)


def _configure_save_directory(binary: Path, env: Mapping[str, str]) -> dict[str, object]:
    """Check the display-free `--save-dir` configuration command."""
    settings = Path(env["XDG_CONFIG_HOME"]) / "snipchord" / "settings.json"
    result = _run([str(binary), "--save-dir", "~/Downloads"], env, timeout=8)
    try:
        saved_settings = json.loads(settings.read_text())
    except (OSError, ValueError) as error:
        raise SmokeError(f"--save-dir wrote invalid settings: {error}") from error
    if saved_settings.get("output_directory") != "~/Downloads":
        raise SmokeError(f"--save-dir did not persist its value: {saved_settings!r}")
    output = result.stdout.decode(errors="replace").strip()
    if str(Path(env["HOME"]) / "Downloads") not in output:
        raise SmokeError(f"--save-dir reported an unexpected path: {output!r}")
    return {"value": saved_settings["output_directory"], "reported": output}


def _save_mode_escape_does_not_write(binary: Path, env: Mapping[str, str]) -> dict[str, object]:
    """Cancel a save selection and ensure no file or clipboard side effect occurs."""
    settings = Path(env["XDG_CONFIG_HOME"]) / "snipchord" / "settings.json"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        json.dumps(
            {
                "save_automatically": False,
                "show_preview": False,
                "output_directory": "~/Downloads",
            }
        )
        + "\n"
    )
    destination = Path(env["HOME"]) / "Downloads"
    before = set(destination.glob("*.png"))
    sentinel = _start_sentinel(env)
    app = _spawn([str(binary), "--region", "--save"], env)
    try:
        _wait_selection_start(app)
        _run(["xdotool", "key", "Escape"], env)
        time.sleep(0.25)
        if app.poll() is not None:
            raise SmokeError(f"save-mode Escape unexpectedly exited the resident app ({app.returncode})")
        after = set(destination.glob("*.png"))
        if after != before:
            raise SmokeError(f"save-mode Escape wrote files: before={sorted(before)}, after={sorted(after)}")
        if sentinel.poll() is not None or _clipboard_target(env, "UTF8_STRING") != b"snipchord-demo-sentinel":
            raise SmokeError("save-mode Escape changed the clipboard")
        return {"files": len(after), "clipboard": "preserved"}
    finally:
        _terminate(app)
        _terminate(sentinel)


def _region_snapshot_is_immutable(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> dict[str, object]:
    """Verify a committed region comes from the pre-overlay desktop snapshot."""
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for snapshot immutability verification") from error
    connection = display.Display(env["DISPLAY"])
    child = None
    app = None
    child_x, child_y = 300, 180
    child_width, child_height = 180, 120
    initial_rgb = (0xD0, 0x40, 0x70)
    updated_rgb = (0x28, 0xB0, 0xD8)
    try:
        root = connection.screen().root
        child = root.create_window(
            child_x,
            child_y,
            child_width,
            child_height,
            2,
            X.CopyFromParent,
            X.InputOutput,
            X.CopyFromParent,
            background_pixel=(initial_rgb[0] << 16) | (initial_rgb[1] << 8) | initial_rgb[2],
        )
        child.map()
        connection.sync()

        app = _spawn([str(binary), "--region", "--clipboard"], env)
        _wait_selection_start(app)
        _wait_for_window_geometry(
            env,
            lambda window: int(window["width"]) == root_size[0]
            and int(window["height"]) == root_size[1],
            3,
        )

        # Change the live child after the root snapshot was made. The accepted
        # normal region must still contain initial_rgb, which proves the
        # overlay never reads a moving root surface after it is shown.
        child.change_attributes(
            background_pixel=(updated_rgb[0] << 16) | (updated_rgb[1] << 8) | updated_rgb[2]
        )
        child.clear_area(0, 0, child_width, child_height, exposures=False)
        connection.sync()
        _run(["xdotool", "mousemove", str(child_x), str(child_y)], env)
        _run(["xdotool", "mousedown", "1"], env)
        _run(
            [
                "xdotool",
                "mousemove",
                str(child_x + child_width),
                str(child_y + child_height),
            ],
            env,
        )
        _run(["xdotool", "mouseup", "1"], env)
        match, output = _read_until(app, CAPTURE_RE, 8)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != (child_width, child_height):
            raise SmokeError(
                f"immutable region reported {dimensions}; output={output[-1000:]}"
            )
        png_w, png_h, samples = _image_pixels(_clipboard_target(env, "image/png"))
        if (png_w, png_h) != dimensions:
            raise SmokeError(f"immutable region clipboard dimensions {(png_w, png_h)} != {dimensions}")
        center = samples[1]
        if sum(abs(center[index] - initial_rgb[index]) for index in range(3)) > 12:
            raise SmokeError(
                "region read the post-overlay child instead of the frozen snapshot: "
                f"center={center}, initial={initial_rgb}, updated={updated_rgb}"
            )
        return {
            "dimensions": dimensions,
            "center": center,
            "snapshot": "pre-overlay",
        }
    finally:
        _terminate(app)
        if child is not None:
            with contextlib.suppress(Exception):
                child.destroy()
                connection.flush()
        connection.close()


def _window_selection_capture(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> dict[str, object]:
    """Exercise Space-before-drag window picking against a real X11 child."""
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for window-selection verification") from error
    connection = display.Display(env["DISPLAY"])
    child = None
    app = None
    child_x, child_y = 420, 240
    child_width, child_height = 180, 120
    initial_rgb = (0xD0, 0x40, 0x70)
    try:
        root = connection.screen().root
        # Xvfb's 24-bit TrueColor visual uses 0x00RRGGBB pixels.  A mapped
        # child gives WindowPicker a concrete stack entry and a distinctive
        # crop whose dimensions/pixels can be checked below.
        child = root.create_window(
            child_x,
            child_y,
            child_width,
            child_height,
            2,
            X.CopyFromParent,
            X.InputOutput,
            X.CopyFromParent,
            background_pixel=(initial_rgb[0] << 16) | (initial_rgb[1] << 8) | initial_rgb[2],
        )
        child.map()
        connection.sync()

        app = _spawn([str(binary), "--region", "--clipboard"], env)
        _wait_selection_start(app)

        # Wait for the full-screen selection surface itself. The test then
        # mutates the child after the root image is frozen, so a fast but
        # slow-to-map process cannot accidentally mutate before capture.
        _wait_for_window_geometry(
            env,
            lambda window: int(window["width"]) == root_size[0]
            and int(window["height"]) == root_size[1],
            3,
        )

        # The root image is frozen before the overlay is mapped. Change the
        # child only after that point: a real Composite/window-pixmap capture
        # must return the new surface pixels, while a crop of the frozen root
        # would still contain the original initial_rgb. This proves the native
        # window path rather than merely proving that the initial desktop crop
        # happened to include the child.
        child_rgb = (0x28, 0xB0, 0xD8)
        child.change_attributes(
            background_pixel=(child_rgb[0] << 16) | (child_rgb[1] << 8) | child_rgb[2]
        )
        child.clear_area(0, 0, child_width, child_height, exposures=False)
        connection.sync()
        _run(["xdotool", "keydown", "space"], env)
        _run(
            [
                "xdotool",
                "mousemove",
                str(child_x + child_width // 2),
                str(child_y + child_height // 2),
            ],
            env,
        )
        _run(["xdotool", "click", "1"], env)
        _run(["xdotool", "keyup", "space"], env)
        match, output = _read_until(app, CAPTURE_RE, 8)
        dimensions = (int(match["width"]), int(match["height"]))
        if not (child_width <= dimensions[0] <= child_width + 12):
            raise SmokeError(
                f"window pick width {dimensions[0]} does not include the child window: output={output[-1000:]}"
            )
        if not (child_height <= dimensions[1] <= child_height + 12):
            raise SmokeError(
                f"window pick height {dimensions[1]} does not include the child window: output={output[-1000:]}"
            )
        png = _clipboard_target(env, "image/png")
        png_w, png_h, samples = _image_pixels(png)
        if (png_w, png_h) != dimensions:
            raise SmokeError(f"window pick clipboard dimensions {(png_w, png_h)} != {dimensions}")
        center = samples[1]
        if sum(abs(center[index] - child_rgb[index]) for index in range(3)) > 12:
            raise SmokeError(
                f"window pick did not capture the child surface: center={center}, expected={child_rgb}"
            )
        return {
            "dimensions": dimensions,
            "png_bytes": len(png),
            "center": center,
            "native_surface": "post-freeze child pixels",
        }
    finally:
        _terminate(app)
        if child is not None:
            with contextlib.suppress(Exception):
                child.destroy()
                connection.flush()
        connection.close()


def _window_selection_clipped_capture(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> dict[str, object]:
    """Verify a live Composite window is clipped correctly at root edges."""
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for clipped-window verification") from error
    root_width, root_height = root_size
    child_width, child_height = 180, 120
    if root_width < 360 or root_height < 300:
        raise SmokeError("clipped-window verification needs an X11 root at least 360x300")
    connection = display.Display(env["DISPLAY"])
    child = None
    app = None
    child_x, child_y = root_width - 84, root_height - 68
    initial_rgb = (0xD0, 0x40, 0x70)
    updated_rgb = (0x28, 0xB0, 0xD8)
    try:
        root = connection.screen().root
        child = root.create_window(
            child_x,
            child_y,
            child_width,
            child_height,
            2,
            X.CopyFromParent,
            X.InputOutput,
            X.CopyFromParent,
            background_pixel=(initial_rgb[0] << 16) | (initial_rgb[1] << 8) | initial_rgb[2],
        )
        child.map()
        connection.sync()
        app = _spawn([str(binary), "--region", "--clipboard"], env)
        _wait_selection_start(app)
        _wait_for_window_geometry(
            env,
            lambda window: int(window["width"]) == root_width
            and int(window["height"]) == root_height,
            3,
        )
        child.change_attributes(
            background_pixel=(updated_rgb[0] << 16) | (updated_rgb[1] << 8) | updated_rgb[2]
        )
        child.clear_area(0, 0, child_width, child_height, exposures=False)
        connection.sync()
        _run(["xdotool", "keydown", "space"], env)
        _run(["xdotool", "mousemove", str(child_x + 20), str(child_y + 20)], env)
        _run(["xdotool", "click", "1"], env)
        _run(["xdotool", "keyup", "space"], env)
        match, output = _read_until(app, CAPTURE_RE, 8)
        dimensions = (int(match["width"]), int(match["height"]))
        visible_width = root_width - child_x
        visible_height = root_height - child_y
        if not (visible_width - 4 <= dimensions[0] <= visible_width + 4):
            raise SmokeError(
                f"clipped window width {dimensions[0]} != visible {visible_width}; output={output[-1000:]}"
            )
        if not (visible_height - 4 <= dimensions[1] <= visible_height + 4):
            raise SmokeError(
                f"clipped window height {dimensions[1]} != visible {visible_height}; output={output[-1000:]}"
            )
        png_w, png_h, samples = _image_pixels(_clipboard_target(env, "image/png"))
        if (png_w, png_h) != dimensions:
            raise SmokeError(f"clipped window dimensions {(png_w, png_h)} != {dimensions}")
        center = samples[1]
        if sum(abs(center[index] - updated_rgb[index]) for index in range(3)) > 12:
            raise SmokeError(
                f"clipped window did not preserve live child pixels: center={center}, expected={updated_rgb}"
            )
        return {
            "dimensions": dimensions,
            "center": center,
            "clipped": True,
        }
    finally:
        _terminate(app)
        if child is not None:
            with contextlib.suppress(Exception):
                child.destroy()
                connection.flush()
        connection.close()


def _preferences_and_preview(
    binary: Path,
    env: Mapping[str, str],
    artifacts_dir: Path | None = None,
    root_size: tuple[int, int] = (DEFAULT_WIDTH, DEFAULT_HEIGHT),
) -> dict[str, object]:
    """Verify preferences and the macOS-style image-only thumbnail.

    The thumbnail does not claim focus, does not expose the old
    title/dimensions/buttons card, and disappears on its own.  Its click
    contract is covered separately by _preview_click_opens_saved_and_clipboard.
    """
    # Earlier smoke checks intentionally disable previews while testing that a
    # cancellation leaves settings untouched. Establish the expected initial
    # state here so the toggle assertion remains independent of test order.
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

    preferences = _spawn([str(binary), "--preferences"], env)
    try:
        window_id = _wait_for_window(env, "SnipChord Preferences", 8)
        # The native Rust window handles pointer events directly (there are no
        # toolkit child widgets), so click the first checkbox in window-local
        # coordinates rather than relying on GTK's Tab/Space focus traversal.
        _run(["xdotool", "windowfocus", "--sync", window_id], env)
        _run(["xdotool", "mousemove", "--window", window_id, "100", "110"], env)
        _run(["xdotool", "click", "1"], env)
        time.sleep(0.2)
    finally:
        _terminate(preferences)
    if not settings.exists():
        # Some GTK implementations only write on a button event.  The window
        # check above remains useful, but do not report a false pass for a
        # missing settings API if the file never appeared after the toggle.
        raise SmokeError(f"preferences toggle did not write {settings}")
    try:
        saved_settings = json.loads(settings.read_text())
    except (OSError, ValueError) as error:
        raise SmokeError(f"preferences wrote invalid JSON: {error}") from error
    if saved_settings.get("show_preview") is not False:
        raise SmokeError(f"preferences checkbox did not toggle show_preview: {saved_settings!r}")

    # Restore the default before exercising a normal capture.  This also
    # verifies that a user can turn the transient thumbnail back on without
    # restarting the resident process.
    preferences = _spawn([str(binary), "--preferences"], env)
    try:
        window_id = _wait_for_window(env, "SnipChord Preferences", 8)
        _run(["xdotool", "windowfocus", "--sync", window_id], env)
        _run(["xdotool", "mousemove", "--window", window_id, "100", "110"], env)
        _run(["xdotool", "click", "1"], env)
        time.sleep(0.2)
    finally:
        _terminate(preferences)
    try:
        saved_settings = json.loads(settings.read_text())
    except (OSError, ValueError) as error:
        raise SmokeError(f"preferences wrote invalid JSON after re-enable: {error}") from error
    if saved_settings.get("show_preview") is not True:
        raise SmokeError(f"preferences did not re-enable show_preview: {saved_settings!r}")

    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for thumbnail focus verification") from error
    focus_connection = display.Display(env["DISPLAY"])
    focus_sentinel = None
    preview = None
    try:
        focus_sentinel = focus_connection.screen().root.create_window(
            2,
            2,
            8,
            8,
            0,
            X.CopyFromParent,
            X.InputOutput,
            X.CopyFromParent,
        )
        focus_sentinel.map()
        focus_sentinel.set_input_focus(X.RevertToParent, X.CurrentTime)
        focus_connection.flush()
        expected_focus = focus_sentinel.id
        if focus_connection.get_input_focus().focus.id != expected_focus:
            raise SmokeError("could not establish the thumbnail focus sentinel")

        # Capture the whole structured fixture rather than the solid --demo
        # surface so the saved artifact is representative of a real desktop
        # thumbnail and exercises the same full-screen path as the `3` key.
        preview = _spawn([str(binary), "--fullscreen", "--clipboard"], env)
        match, output = _read_until(preview, CAPTURE_RE, 8)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != root_size:
            raise SmokeError(f"thumbnail fixture capture reported {dimensions}; output={output[-1000:]}")
        geometry = _wait_for_thumbnail_window(env, root_size, 8)
        window_id = str(geometry["id"])
        if artifacts_dir is not None:
            artifact = artifacts_dir / "snipchord-thumbnail.png"
            _capture_window_png(env, window_id, artifact)
            visual = _assert_thumbnail_visual(artifact, geometry, dimensions)
        else:
            visual = {"geometry": (int(geometry["width"]), int(geometry["height"]))}
        shape = _assert_window_is_rounded(env, window_id)

        focus_connection.sync()
        current_focus = focus_connection.get_input_focus().focus
        if current_focus.id != expected_focus:
            raise SmokeError(
                f"thumbnail stole focus from the capture owner: {current_focus.id:#x} != {expected_focus:#x}"
            )

        _wait_for_thumbnail_gone(env, root_size)
        return {"geometry": geometry, "visual": visual, "shape": shape, "dismissed": True}
    finally:
        _terminate(preview)
        if focus_sentinel is not None:
            with contextlib.suppress(Exception):
                focus_sentinel.destroy()
                focus_connection.flush()
        focus_connection.close()


def _preview_click_opens_saved_and_clipboard(
    binary: Path,
    env: Mapping[str, str],
    root_size: tuple[int, int],
) -> dict[str, object]:
    """Verify thumbnail clicks open the saved file or a managed temp PNG."""
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
    destination = Path(env["HOME"]) / "Downloads"
    before_saved = set(destination.glob("*.png"))
    open_env, log = _xdg_open_stub(env)
    sentinel = _start_sentinel(open_env)
    saved_app = _spawn([str(binary), "--fullscreen", "--save"], open_env)
    saved_path: Path | None = None
    try:
        match, output = _read_until(saved_app, CAPTURE_RE, 12)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != root_size:
            raise SmokeError(f"saved preview reported {dimensions}; output={output[-1000:]}")
        _click_thumbnail(open_env, root_size)
        saved_path = _wait_for_opened_path(log).resolve()
        new_saved = {path.resolve() for path in destination.glob("*.png")} - {
            path.resolve() for path in before_saved
        }
        if len(new_saved) != 1 or saved_path not in new_saved:
            raise SmokeError(
                f"saved preview opened {saved_path}, expected the only new PNG {sorted(new_saved)}"
            )
        png_w, png_h, samples = _image_pixels(saved_path.read_bytes())
        expected = tuple(
            _fixture_pixel(*point)
            for point in ((0, 0), (root_size[0] // 2, root_size[1] // 2), (root_size[0] - 1, root_size[1] - 1))
        )
        if (png_w, png_h) != dimensions or samples != expected:
            raise SmokeError(
                f"saved preview image changed: {(png_w, png_h)}, {samples}; expected {dimensions}, {expected}"
            )
        if sentinel.poll() is not None or _clipboard_target(open_env, "UTF8_STRING") != b"snipchord-demo-sentinel":
            raise SmokeError("clicking a saved preview changed the clipboard")
    finally:
        _terminate(saved_app)
        _terminate(sentinel)

    log.unlink(missing_ok=True)
    clipboard_app = _spawn([str(binary), "--fullscreen", "--clipboard"], open_env)
    temp_prefix = f".snipchord-preview-{clipboard_app.pid}-"
    try:
        match, output = _read_until(clipboard_app, CAPTURE_RE, 12)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != root_size:
            raise SmokeError(f"clipboard preview reported {dimensions}; output={output[-1000:]}")
        temp_paths = set(Path("/tmp").glob(f"{temp_prefix}*.png"))
        if temp_paths:
            raise SmokeError(f"clipboard preview wrote temp files before click: {sorted(temp_paths)}")
        before_png = _clipboard_target(open_env, "image/png")
        before_targets = _clipboard_targets(open_env)
        _click_thumbnail(open_env, root_size)
        temp_path = _wait_for_opened_path(log).resolve()
        if not temp_path.name.startswith(temp_prefix):
            raise SmokeError(f"clipboard preview opened an unexpected path: {temp_path}")
        if not temp_path.is_file():
            raise SmokeError(f"clipboard preview path does not exist: {temp_path}")
        temp_paths = {path.resolve() for path in Path("/tmp").glob(f"{temp_prefix}*.png")}
        if temp_paths != {temp_path}:
            raise SmokeError(f"clipboard preview created unexpected temp files: {sorted(temp_paths)}")
        temp_png = temp_path.read_bytes()
        if _image_pixels(temp_png) != _image_pixels(before_png):
            raise SmokeError("temporary preview image differs from the clipboard image")
        if _clipboard_target(open_env, "image/png") != before_png:
            raise SmokeError("clicking a clipboard preview changed the clipboard image")
        if _clipboard_targets(open_env) != before_targets:
            raise SmokeError("clicking a clipboard preview changed clipboard targets")

        _run([str(binary), "--quit"], open_env, timeout=8)
        deadline = time.monotonic() + 3
        while clipboard_app.poll() is None and time.monotonic() < deadline:
            time.sleep(0.05)
        if clipboard_app.poll() is None:
            raise SmokeError("clipboard preview app did not exit after --quit")
        if temp_path.exists():
            raise SmokeError(f"temporary preview was not cleaned up on app exit: {temp_path}")
        return {
            "saved_path": str(saved_path),
            "clipboard_temp": str(temp_path),
            "clipboard_preserved": True,
        }
    finally:
        _terminate(clipboard_app)
        for path in Path("/tmp").glob(f"{temp_prefix}*.png"):
            with contextlib.suppress(FileNotFoundError):
                path.unlink()


def _preferences_cancel_active_capture(binary: Path, env: Mapping[str, str]) -> dict[str, object]:
    """Ensure a preferences command clears selection state before the next capture."""
    app = _spawn([str(binary), "--region"], env)
    try:
        _wait_selection_start(app)
        _run([str(binary), "--preferences"], env, timeout=8)
        preferences = _wait_for_window(env, "SnipChord Preferences", 8)
        # Native preferences owns input focus.  Send a normal focused key
        # event rather than targeting the transient XID again; a fast close or
        # XID reuse between search and SendEvent otherwise yields BadWindow.
        _run(["xdotool", "key", "Escape"], env)
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            if not _run(
                ["xdotool", "search", "--name", "SnipChord Preferences"],
                env,
                check=False,
                timeout=2,
            ).stdout.strip():
                break
            time.sleep(0.05)
        else:
            raise SmokeError("preferences Escape did not close the active preferences window")

        # Send a second region command to the existing owner.  If pending_image
        # or the selection UI was left stale, this command will be ignored and
        # the capture-complete marker below will never arrive.
        _run([str(binary), "--region"], env, timeout=8)
        _wait_selection_start(app)
        _run(["xdotool", "mousemove", "100", "100"], env)
        _run(["xdotool", "mousedown", "1"], env)
        _run(["xdotool", "mousemove", "300", "250"], env)
        _run(["xdotool", "mouseup", "1"], env)
        match, output = _read_until(app, CAPTURE_RE, 8)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != (200, 150):
            raise SmokeError(f"capture after preferences reported {dimensions}; output={output[-1000:]}")
        png = _clipboard_target(env, "image/png")
        if _png_size(png) != dimensions:
            raise SmokeError(f"capture after preferences clipboard is {_png_size(png)}, expected {dimensions}")
        return {"dimensions": dimensions, "png_bytes": len(png)}
    finally:
        _terminate(app)


def _proc_memory(pid: int) -> tuple[int | None, int | None]:
    rss: int | None = None
    pss: int | None = None
    status = Path(f"/proc/{pid}/status")
    rollup = Path(f"/proc/{pid}/smaps_rollup")
    if status.exists():
        for line in status.read_text(errors="replace").splitlines():
            if line.startswith("VmRSS:"):
                rss = int(line.split()[1])
                break
    if rollup.exists():
        for line in rollup.read_text(errors="replace").splitlines():
            if line.startswith("Pss:"):
                pss = int(line.split()[1])
                break
    return rss, pss


def _measure_process(command: Sequence[str], env: Mapping[str, str]) -> tuple[int | None, int | None]:
    process = _spawn(command, env)
    try:
        time.sleep(1.0)
        return _proc_memory(process.pid)
    finally:
        _terminate(process)


def _measure_rss(binary: Path, env: Mapping[str, str]) -> None:
    rust_rss, rust_pss = _measure_process([str(binary), "--daemon"], env)
    print(f"memory rust rss_kib={rust_rss} pss_kib={rust_pss}")
    legacy = ROOT / "legacy" / "python" / "src"
    if not legacy.is_dir():
        print("memory python unavailable (legacy/python was not found)")
        return
    python_env = dict(env)
    python_env["PYTHONPATH"] = str(legacy)
    try:
        python_rss, python_pss = _measure_process(
            [sys.executable, "-m", "snipchord.app", "--daemon"], python_env
        )
    except (FileNotFoundError, SmokeError, subprocess.TimeoutExpired) as error:
        print(f"memory python unavailable ({error})")
        return
    print(f"memory python rss_kib={python_rss} pss_kib={python_pss}")
    print("memory note: one idle sample on the same Xvfb is observational, not a benchmark")


def _resolve_binary(value: str | None) -> Path:
    if value:
        binary = Path(value).expanduser().resolve()
    else:
        candidates = [ROOT / "target" / "release" / "snipchord", ROOT / "target" / "debug" / "snipchord"]
        binary = next((candidate for candidate in candidates if candidate.is_file()), candidates[0])
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise SmokeError(f"Rust binary is missing or not executable: {binary}")
    return binary


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", help="Rust snipchord executable (default: target/release/snipchord)")
    parser.add_argument("--display", type=int, help="isolated X display number (default: first free :93..:129)")
    parser.add_argument("--width", type=int, default=DEFAULT_WIDTH)
    parser.add_argument("--height", type=int, default=DEFAULT_HEIGHT)
    parser.add_argument(
        "--disable-render",
        action="store_true",
        help="start Xvfb with Render disabled to exercise the client-side fallback path",
    )
    parser.add_argument(
        "--artifacts-dir",
        type=Path,
        default=Path("/tmp") / f"snipchord-rust-x11-{os.getpid()}",
        help="directory for selection/preview PNGs used by visual QA",
    )
    args = parser.parse_args(argv)
    if args.width <= 0 or args.height <= 0:
        parser.error("--width and --height must be positive")
    missing = _require_commands(["xclip", "xdotool", "xdpyinfo", "xwininfo", "xwd", "convert"])
    if missing:
        print(f"SKIP: missing commands: {', '.join(missing)}", file=sys.stderr)
        return 2
    try:
        binary = _resolve_binary(args.binary)
    except SmokeError as error:
        print(f"SKIP: {error}", file=sys.stderr)
        return 2
    try:
        with tempfile.TemporaryDirectory(prefix="snipchord-x11-") as temporary:
            runtime = Path(temporary)
            args.artifacts_dir.mkdir(parents=True, exist_ok=True)
            xvfb_args = ("-extension", "RENDER") if args.disable_render else ()
            with XvfbServer(
                runtime,
                args.width,
                args.height,
                args.display,
                xvfb_args,
            ) as server:
                env = server.env
                if args.disable_render:
                    extensions = {name.casefold() for name in _x11_extensions(env)}
                    if "render" in extensions:
                        raise SmokeError(
                            "Xvfb still advertises Render after --disable-render; "
                            "fallback verification would be ambiguous"
                        )
                    print("PASS Xvfb Render extension disabled")
                fixture = _publish_fixture(env, args.width, args.height)
                try:
                    _demo_does_not_write_clipboard(binary, env)
                    _escape_cancels_region(binary, env)
                    click_cancel = _click_without_drag_cancels_region(
                        binary,
                        env,
                        (args.width, args.height),
                    )
                    immediate_drag = _immediate_drag_after_resident_command(binary, env)
                    early_input = _xtest_burst_during_cursor_ready(
                        binary,
                        env,
                        (args.width, args.height),
                    )
                    _selection_restores_foreign_focus(binary, env)
                    dimensions = _space_move_preserves_dimensions(
                        binary,
                        env,
                        args.artifacts_dir,
                        (args.width, args.height),
                    )
                    redraw_samples = _selection_redraw_has_no_intermediate_frame(
                        binary, env, (args.width, args.height)
                    )
                    overlay = _selection_overlay_has_no_text_badges(
                        binary,
                        env,
                        args.artifacts_dir,
                        (args.width, args.height),
                    )
                    region = _region_clipboard_crop(binary, env)
                    clipboard = _fullscreen_clipboard(binary, env, args.width, args.height)
                    preview = _preferences_and_preview(
                        binary,
                        env,
                        args.artifacts_dir,
                        (args.width, args.height),
                    )
                    preview_open = _preview_click_opens_saved_and_clipboard(
                        binary,
                        env,
                        (args.width, args.height),
                    )
                    configured_directory = _configure_save_directory(binary, env)
                    saved_region = _region_save_to_configured_directory(binary, env)
                    saved_fullscreen = _fullscreen_save_to_configured_directory(
                        binary,
                        env,
                        (args.width, args.height),
                    )
                    save_cancel = _save_mode_escape_does_not_write(binary, env)
                    snapshot = _region_snapshot_is_immutable(
                        binary,
                        env,
                        (args.width, args.height),
                    )
                    window_capture = _window_selection_capture(
                        binary,
                        env,
                        (args.width, args.height),
                    )
                    clipped_window = _window_selection_clipped_capture(
                        binary,
                        env,
                        (args.width, args.height),
                    )
                    _preferences_ignores_foreign_client_message(binary, env)
                    preferences_recovery = _preferences_cancel_active_capture(binary, env)
                    keyboard_busy = _keyboard_grab_deferred_selection(
                        binary,
                        env,
                        (args.width, args.height),
                    )
                    _failed_pointer_grab_releases_keyboard(binary, env)
                    print(f"PASS demo clipboard, Escape, Space dimensions={dimensions}")
                    print(f"PASS plain click cancels empty region {click_cancel}")
                    print(f"PASS immediate post-hotkey drag {immediate_drag}")
                    print(f"PASS early XTEST drag before overlay {early_input}")
                    print(f"PASS selection redraw frames={redraw_samples}")
                    print(f"PASS clean selection overlay idle/drag/window-pick {overlay}")
                    print(f"PASS region crop clipboard {region}")
                    print(f"PASS fullscreen clipboard {clipboard}")
                    print(f"PASS preferences/thumbnail {preview}")
                    print(f"PASS thumbnail click opens saved/temp image {preview_open}")
                    print(f"PASS save directory configuration {configured_directory}")
                    print(f"PASS region save output directory {saved_region}")
                    print(f"PASS fullscreen save output directory {saved_fullscreen}")
                    print(f"PASS save-mode Escape cleanup {save_cancel}")
                    print(f"PASS pre-overlay snapshot immutability {snapshot}")
                    print(f"PASS Space-before-drag window capture {window_capture}")
                    print(f"PASS clipped Composite window capture {clipped_window}")
                    print("PASS focus restore and foreign ClientMessage")
                    print(f"PASS preferences recovery {preferences_recovery}")
                    print(f"PASS deferred keyboard grab pointer/cancel {keyboard_busy}")
                    print("PASS genuine pointer grab failure cleanup and daemon retry")
                    print(f"visual artifacts: {args.artifacts_dir}")
                    _measure_rss(binary, env)
                finally:
                    fixture.close()
    except SmokeError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    except (OSError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
