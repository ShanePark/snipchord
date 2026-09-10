#!/usr/bin/env python3
"""Exercise raw screenshot shortcuts while an X11 menu owns the input grabs.

The fixture is deliberately closer to a file manager context menu than to a
normal selection window: a foreign X11 client maps a coloured popup and holds
both the pointer and keyboard grabs.  A resident SnipChord process must still
see the configured raw shortcut, take the desktop snapshot before dismissing
the popup, and leave the pointer usable for the selection gesture.

All work runs on a private Xvfb display with temporary HOME/XDG directories.
The test never reads the live desktop, changes the user's shortcut settings,
or uses the live clipboard.

Example::

    python3 tests/menu_capture.py --binary target/release/snipchord
"""

from __future__ import annotations

import argparse
import contextlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
from typing import Mapping, Sequence


ROOT = Path(__file__).resolve().parents[1]
TESTS = ROOT / "tests"
if str(TESTS) not in sys.path:
    sys.path.insert(0, str(TESTS))

import rust_x11_smoke as smoke  # noqa: E402 (local harness import)


MENU_READY_RE = re.compile(r"menu_ready")
MENU_RELEASED_RE = re.compile(r"menu_released")


def _resolve_binary(value: str | None) -> Path:
    if value:
        binary = Path(value).expanduser().resolve()
    else:
        candidates = (
            ROOT / "target" / "release" / "snipchord",
            ROOT / "target" / "debug" / "snipchord",
        )
        binary = next((candidate for candidate in candidates if candidate.is_file()), candidates[0])
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise smoke.SmokeError(f"Rust binary is missing or not executable: {binary}")
    return binary


def _write_gsettings_stub(env: Mapping[str, str]) -> dict[str, str]:
    """Return an environment whose gsettings exposes the four managed bindings."""
    directory = Path(env["XDG_RUNTIME_DIR"]) / "menu-capture-gsettings"
    directory.mkdir(parents=True, exist_ok=True)
    executable = directory / "gsettings"
    # Keep the command executable's basename as ``snipchord``.  The shortcut
    # reader intentionally ignores unrelated custom commands by that name.
    executable.write_text(
        "#!/bin/sh\n"
        "schema=\"$2\"\n"
        "key=\"$3\"\n"
        "if [ \"$1\" != get ]; then exit 1; fi\n"
        "if [ \"$schema\" = org.gnome.settings-daemon.plugins.media-keys ] && [ \"$key\" = custom-keybindings ]; then\n"
        "  printf '%s\\n' \"['/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/','/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom1/','/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom2/','/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom3/']\"\n"
        "  exit 0\n"
        "fi\n"
        "case \"$schema\" in\n"
        "  *custom0/) command=\"/home/test/.local/bin/snipchord --region --clipboard\"; binding=\"<Control><Alt><Shift>4\" ;;\n"
        "  *custom1/) command=\"/home/test/.local/bin/snipchord --region --save\"; binding=\"<Alt><Shift>4\" ;;\n"
        "  *custom2/) command=\"/home/test/.local/bin/snipchord --fullscreen --clipboard\"; binding=\"<Control><Alt><Shift>3\" ;;\n"
        "  *custom3/) command=\"/home/test/.local/bin/snipchord --fullscreen --save\"; binding=\"<Alt><Shift>3\" ;;\n"
        "  *) exit 1 ;;\n"
        "esac\n"
        "case \"$key\" in\n"
        "  command) printf \"'%s'\\n\" \"$command\" ;;\n"
        "  binding) printf \"'%s'\\n\" \"$binding\" ;;\n"
        "  *) exit 1 ;;\n"
        "esac\n"
    )
    executable.chmod(0o755)
    child_env = dict(env)
    child_env["PATH"] = f"{directory}{os.pathsep}{env.get('PATH', '')}"
    return child_env


def _write_settings(env: Mapping[str, str]) -> Path:
    settings = Path(env["XDG_CONFIG_HOME"]) / "snipchord" / "settings.json"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        '{"save_automatically": false, "show_preview": false, '
        '"output_directory": "~/Downloads"}\n'
    )
    destination = Path(env["HOME"]) / "Downloads"
    destination.mkdir(parents=True, exist_ok=True)
    return destination


def _menu_worker(argv: Sequence[str]) -> int:
    """Map a coloured popup, grab input, and dismiss it when Escape arrives."""
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--menu-worker", action="store_true")
    parser.add_argument("--x", type=int, required=True)
    parser.add_argument("--y", type=int, required=True)
    parser.add_argument("--width", type=int, required=True)
    parser.add_argument("--height", type=int, required=True)
    parser.add_argument("--initial", type=lambda value: int(value, 16), required=True)
    parser.add_argument("--updated", type=lambda value: int(value, 16), required=True)
    args = parser.parse_args(argv)
    try:
        from Xlib import X, display
    except ImportError as error:
        print(f"menu worker requires python-xlib: {error}", file=sys.stderr, flush=True)
        return 2

    connection = display.Display(os.environ["DISPLAY"])
    window = None
    pointer_grabbed = False
    keyboard_grabbed = False
    initial = args.initial
    updated = args.updated
    try:
        root = connection.screen().root
        window = root.create_window(
            args.x,
            args.y,
            args.width,
            args.height,
            0,
            X.CopyFromParent,
            X.InputOutput,
            X.CopyFromParent,
            background_pixel=initial,
            event_mask=X.KeyPressMask | X.KeyReleaseMask | X.ExposureMask,
        )
        window.map()
        window.configure(stack_mode=X.Above)
        connection.sync()

        pointer_status = window.grab_pointer(
            False,
            X.ButtonPressMask | X.ButtonReleaseMask | X.PointerMotionMask,
            X.GrabModeAsync,
            X.GrabModeAsync,
            X.NONE,
            X.NONE,
            X.CurrentTime,
        )
        connection.sync()
        if pointer_status != X.GrabSuccess:
            print(f"menu pointer grab failed: {pointer_status}", file=sys.stderr, flush=True)
            return 2
        pointer_grabbed = True

        keyboard_status = window.grab_keyboard(
            False, X.GrabModeAsync, X.GrabModeAsync, X.CurrentTime
        )
        connection.sync()
        if keyboard_status != X.GrabSuccess:
            print(f"menu keyboard grab failed: {keyboard_status}", file=sys.stderr, flush=True)
            return 2
        keyboard_grabbed = True
        print("menu_ready", flush=True)

        while True:
            event = connection.next_event()
            if event.type != X.KeyPress:
                continue
            keysym = connection.keycode_to_keysym(event.detail, 0)
            if keysym != 0xFF1B:  # XK_Escape
                continue
            if event.state & (X.ShiftMask | X.ControlMask | X.Mod1Mask | X.Mod4Mask):
                # The application must wait until the screenshot shortcut's
                # modifiers are physically released before sending Escape.
                # A modified Escape would be a different menu action on a
                # real file manager and must not dismiss this fixture.
                continue

            # A real context menu can change or disappear as soon as Escape is
            # delivered.  The screenshot must already own the old pixels by
            # then, so make that ordering observable to the test.
            window.change_attributes(background_pixel=updated)
            window.clear_area(0, 0, args.width, args.height, exposures=False)
            connection.sync()
            if pointer_grabbed:
                connection.ungrab_pointer(X.CurrentTime)
                pointer_grabbed = False
            if keyboard_grabbed:
                connection.ungrab_keyboard(X.CurrentTime)
                keyboard_grabbed = False
            window.unmap()
            connection.flush()
            print("menu_released", flush=True)
            return 0
    except Exception as error:
        print(f"menu worker failed: {error}", file=sys.stderr, flush=True)
        return 2
    finally:
        if window is not None:
            with contextlib.suppress(Exception):
                if pointer_grabbed:
                    connection.ungrab_pointer(X.CurrentTime)
                if keyboard_grabbed:
                    connection.ungrab_keyboard(X.CurrentTime)
                window.destroy()
                connection.flush()
        connection.close()


def _start_menu(
    env: Mapping[str, str],
    x: int,
    y: int,
    width: int,
    height: int,
    initial: tuple[int, int, int],
    updated: tuple[int, int, int],
) -> subprocess.Popen[bytes]:
    def packed(rgb: tuple[int, int, int]) -> str:
        return f"{(rgb[0] << 16) | (rgb[1] << 8) | rgb[2]:06x}"

    process = smoke._spawn(
        [
            sys.executable,
            str(Path(__file__).resolve()),
            "--menu-worker",
            "--x",
            str(x),
            "--y",
            str(y),
            "--width",
            str(width),
            "--height",
            str(height),
            "--initial",
            packed(initial),
            "--updated",
            packed(updated),
        ],
        env,
    )
    smoke._read_until(process, MENU_READY_RE, 4)
    return process


def _full_screen_windows(env: Mapping[str, str], size: tuple[int, int]) -> set[str]:
    return {
        str(window["id"])
        for window in smoke._window_geometries(env)
        if (int(window["width"]), int(window["height"])) == size
    }


def _wait_for_new_full_screen_window(
    env: Mapping[str, str], size: tuple[int, int], existing: set[str], timeout: float = 6
) -> dict[str, int | str]:
    return smoke._wait_for_window_geometry(
        env,
        lambda window: str(window["id"]) not in existing
        and (int(window["width"]), int(window["height"])) == size,
        timeout,
    )


def _wait_for_new_png(destination: Path, before: set[Path], timeout: float = 6) -> set[Path]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        current = set(destination.glob("*.png")) - before
        if current:
            return current
        time.sleep(0.03)
    raise smoke.SmokeError(
        f"screenshot file did not appear in {destination}; before={sorted(before)}"
    )


def _image(path: Path):
    try:
        from PIL import Image
    except ImportError as error:
        raise smoke.SmokeError("Pillow is required for menu screenshot verification") from error
    try:
        image = Image.open(path).convert("RGB")
        image.load()
        return image
    except Exception as error:
        raise smoke.SmokeError(f"could not decode screenshot {path}: {error}") from error


def _assert_near(observed: tuple[int, int, int], expected: tuple[int, int, int], label: str) -> None:
    if sum(abs(observed[index] - expected[index]) for index in range(3)) > 12:
        raise smoke.SmokeError(f"{label} pixel was {observed}, expected {expected}")


def _trigger_key(env: Mapping[str, str], key: str, point: tuple[int, int]) -> None:
    smoke._run(["xdotool", "mousemove", str(point[0]), str(point[1])], env, timeout=3)
    smoke._run(["xdotool", "key", key], env, timeout=3)


def _wait_menu_release(menu: subprocess.Popen[bytes]) -> None:
    smoke._read_until(menu, MENU_RELEASED_RE, 6)


def _region_shortcut_under_menu(
    binary: Path,
    env: Mapping[str, str],
    daemon: subprocess.Popen[bytes],
    destination: Path,
    root_size: tuple[int, int],
) -> dict[str, object]:
    popup_x, popup_y = 250, 170
    popup_width, popup_height = 180, 120
    initial = (0xD0, 0x40, 0x70)
    updated = (0x28, 0xB0, 0xD8)
    menu = _start_menu(env, popup_x, popup_y, popup_width, popup_height, initial, updated)
    before_files = set(destination.glob("*.png"))
    existing_windows = _full_screen_windows(env, root_size)
    try:
        # This key is delivered to the foreign menu's normal keyboard grab.
        # The daemon must receive the same physical key through its raw X11
        # listener and dismiss the menu once the trigger snapshot is safe.
        _trigger_key(env, "alt+shift+4", (popup_x + 20, popup_y + 20))
        try:
            _wait_menu_release(menu)
        except smoke.SmokeError as error:
            daemon_output = _read_available(daemon, timeout=0.2)
            raise smoke.SmokeError(f"{error}; daemon output={daemon_output[-2000:]}") from error
        selection = _wait_for_new_full_screen_window(env, root_size, existing_windows)

        # GNOME's ordinary shortcut launcher can arrive after the raw event's
        # XTEST Escape has already dismissed the menu.  This is a second
        # delivery of the same physical shortcut, not a new user action; the
        # resident must suppress it while retaining the pending selection.
        duplicate = smoke._spawn([str(binary), "--region", "--save"], env)
        duplicate.wait(timeout=4)
        if duplicate.returncode != 0:
            duplicate_output = (
                duplicate.stdout.read().decode(errors="replace") if duplicate.stdout else ""
            )
            raise smoke.SmokeError(
                f"post-XTEST duplicate region command failed: {duplicate_output[-1000:]}"
            )

        # The popup was dismissed by Escape.  The accepted crop must still be
        # the old menu pixels from the snapshot taken at shortcut time.
        smoke._run(["xdotool", "mousemove", str(popup_x), str(popup_y)], env)
        smoke._run(["xdotool", "mousedown", "1"], env)
        smoke._run(
            [
                "xdotool",
                "mousemove",
                str(popup_x + popup_width),
                str(popup_y + popup_height),
            ],
            env,
        )
        smoke._run(["xdotool", "mouseup", "1"], env)
        match, output = smoke._read_until(daemon, smoke.CAPTURE_RE, 8)
        dimensions = (int(match["width"]), int(match["height"]))
        expected_dimensions = (popup_width, popup_height)
        if dimensions != expected_dimensions:
            raise smoke.SmokeError(
                f"raw region shortcut reported {dimensions}, expected {expected_dimensions}; "
                f"output={output[-1000:]}"
            )
        new_files = _wait_for_new_png(destination, before_files)
        if len(new_files) != 1:
            raise smoke.SmokeError(f"raw region shortcut wrote {len(new_files)} files: {sorted(new_files)}")
        path = next(iter(new_files))
        with _image(path) as image:
            if image.size != expected_dimensions:
                raise smoke.SmokeError(f"raw region screenshot has size {image.size}, expected {expected_dimensions}")
            center = tuple(image.getpixel((popup_width // 2, popup_height // 2)))
            _assert_near(center, initial, "raw region trigger-time")
            if sum(abs(center[index] - updated[index]) for index in range(3)) < 120:
                raise smoke.SmokeError(
                    f"raw region used post-dismissal pixels: center={center}, updated={updated}"
                )
        return {
            "dimensions": dimensions,
            "center": center,
            "menu": "pointer+keyboard grab",
            "snapshot": "trigger-time",
            "command_after_xtest": "suppressed",
            "selection_window": str(selection["id"]),
        }
    finally:
        smoke._terminate(menu)


def _read_available(process: subprocess.Popen[bytes], timeout: float = 0.8) -> str:
    """Collect already queued daemon output without blocking for another marker."""
    if process.stdout is None:
        return ""
    import selectors

    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    chunks: list[bytes] = []
    deadline = time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            events = selector.select(max(0.01, min(0.1, deadline - time.monotonic())))
            if not events:
                continue
            chunk = process.stdout.read1(4096)
            if not chunk:
                break
            chunks.append(chunk)
    finally:
        selector.close()
    return b"".join(chunks).decode(errors="replace")


def _fullscreen_shortcut_dedup(
    binary: Path,
    env: Mapping[str, str],
    daemon: subprocess.Popen[bytes],
    destination: Path,
    root_size: tuple[int, int],
) -> dict[str, object]:
    popup_x, popup_y = 500, 300
    popup_width, popup_height = 160, 100
    initial = (0x36, 0xC4, 0x82)
    updated = (0xD0, 0x68, 0x24)
    menu = _start_menu(env, popup_x, popup_y, popup_width, popup_height, initial, updated)
    before_files = set(destination.glob("*.png"))
    duplicate: subprocess.Popen[bytes] | None = None
    try:
        # First let the raw shortcut reach the resident and complete its
        # capture.  Only then issue the GNOME process-launch command.  This
        # makes the ordering explicit while preserving the real duplicate
        # race: both deliveries belong to one physical keypress, so the
        # second command must be discarded inside the duplicate window.
        _trigger_key(env, "alt+shift+3", (popup_x + 20, popup_y + 20))
        first, output = smoke._read_until(daemon, smoke.CAPTURE_RE, 10)
        dimensions = (int(first["width"]), int(first["height"]))
        if dimensions != root_size:
            raise smoke.SmokeError(f"raw fullscreen shortcut reported {dimensions}, expected {root_size}")

        duplicate = smoke._spawn([str(binary), "--fullscreen", "--save"], env)
        duplicate.wait(timeout=4)
        if duplicate.returncode != 0:
            output = duplicate.stdout.read().decode(errors="replace") if duplicate.stdout else ""
            raise smoke.SmokeError(f"duplicate fullscreen command failed: {output[-1000:]}")
        extra = _read_available(daemon)
        capture_count = 1 + len(smoke.CAPTURE_RE.findall(extra))
        if capture_count != 1:
            raise smoke.SmokeError(
                f"one physical fullscreen shortcut produced {capture_count} captures; "
                f"extra daemon output={extra[-1000:]}"
            )
        new_files = _wait_for_new_png(destination, before_files)
        time.sleep(0.35)
        all_new_files = set(destination.glob("*.png")) - before_files
        if len(all_new_files) != 1 or new_files != all_new_files:
            raise smoke.SmokeError(
                f"one physical fullscreen shortcut wrote {len(all_new_files)} files: "
                f"{sorted(all_new_files)}"
            )
        path = next(iter(all_new_files))
        with _image(path) as image:
            if image.size != root_size:
                raise smoke.SmokeError(f"fullscreen screenshot has size {image.size}, expected {root_size}")
            center = tuple(image.getpixel((popup_x + popup_width // 2, popup_y + popup_height // 2)))
            _assert_near(center, initial, "raw fullscreen trigger-time")
        return {
            "dimensions": dimensions,
            "captures": capture_count,
            "files": len(all_new_files),
            "popup_center": center,
            "snapshot": "trigger-time",
        }
    finally:
        smoke._terminate(menu)
        smoke._terminate(duplicate)


def _normal_region(
    binary: Path,
    env: Mapping[str, str],
    daemon: subprocess.Popen[bytes],
    destination: Path,
    root_size: tuple[int, int],
) -> dict[str, object]:
    start_x, start_y = 40, 40
    width, height = 140, 90
    before_files = set(destination.glob("*.png"))
    existing_windows = _full_screen_windows(env, root_size)
    command = smoke._spawn([str(binary), "--region", "--save"], env)
    try:
        # The resident daemon owns the selection window; the short-lived
        # command process may exit as soon as it delivers the IPC request.
        _wait_for_new_full_screen_window(env, root_size, existing_windows)
        smoke._run(["xdotool", "mousemove", str(start_x), str(start_y)], env)
        smoke._run(["xdotool", "mousedown", "1"], env)
        smoke._run(["xdotool", "mousemove", str(start_x + width), str(start_y + height)], env)
        smoke._run(["xdotool", "mouseup", "1"], env)
        match, output = smoke._read_until(daemon, smoke.CAPTURE_RE, 8)
        dimensions = (int(match["width"]), int(match["height"]))
        if dimensions != (width, height):
            raise smoke.SmokeError(f"normal region reported {dimensions}; output={output[-1000:]}")
        files = _wait_for_new_png(destination, before_files)
        if len(files) != 1:
            raise smoke.SmokeError(f"normal region wrote {len(files)} files: {sorted(files)}")
        with _image(next(iter(files))) as image:
            if image.size != dimensions:
                raise smoke.SmokeError(f"normal region screenshot has size {image.size}")
            center = tuple(image.getpixel((width // 2, height // 2)))
        expected = smoke._fixture_pixel(start_x + width // 2, start_y + height // 2)
        _assert_near(center, expected, "normal region")
        return {"dimensions": dimensions, "center": center, "expected": expected}
    finally:
        smoke._terminate(command)


def _run_suite(binary: Path, width: int, height: int, display: int | None) -> dict[str, object]:
    with tempfile.TemporaryDirectory(prefix="snipchord-menu-capture-") as temporary:
        runtime = Path(temporary)
        with smoke.XvfbServer(runtime, width, height, display) as server:
            env = _write_gsettings_stub(server.env)
            destination = _write_settings(env)
            fixture = smoke._publish_fixture(env, width, height)
            daemon = smoke._spawn([str(binary), "--daemon"], env)
            try:
                smoke._wait_for_instance_owner(env)
                region = _region_shortcut_under_menu(
                    binary, env, daemon, destination, (width, height)
                )
                fullscreen = _fullscreen_shortcut_dedup(
                    binary, env, daemon, destination, (width, height)
                )
                normal = _normal_region(binary, env, daemon, destination, (width, height))
                return {"region": region, "fullscreen": fullscreen, "normal": normal}
            finally:
                with contextlib.suppress(Exception):
                    smoke._run([str(binary), "--quit"], env, timeout=4)
                smoke._terminate(daemon)
                fixture.close()


def main(argv: Sequence[str] | None = None) -> int:
    if argv and argv[0] == "--menu-worker":
        return _menu_worker(argv)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", help="Rust snipchord executable")
    parser.add_argument("--display", type=int, help="private X display number")
    parser.add_argument("--width", type=int, default=1024)
    parser.add_argument("--height", type=int, default=768)
    args = parser.parse_args(argv)
    if args.width <= 0 or args.height <= 0:
        parser.error("--width and --height must be positive")
    missing = smoke._require_commands(("xdotool", "xdpyinfo", "xwininfo"))
    if missing:
        print(f"SKIP: missing commands: {', '.join(missing)}", file=sys.stderr)
        return 2
    try:
        binary = _resolve_binary(args.binary)
        # XvfbServer owns the temporary environment and cleans its display
        # socket even when an assertion below fails.
        results = _run_suite(binary, args.width, args.height, args.display)
        print(f"PASS menu-grab raw region {results['region']}")
        print(f"PASS menu-grab fullscreen dedup {results['fullscreen']}")
        print(f"PASS normal region {results['normal']}")
        return 0
    except smoke.SmokeError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    except (OSError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
