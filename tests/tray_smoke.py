#!/usr/bin/env python3
"""Exercise SnipChord's StatusNotifierItem on a private X11/D-Bus session.

The check deliberately starts the application before a watcher exists.  The
tray service must still export its SNI and menu, and it must continue working
when a watcher appears later.  All child processes use the temporary Xvfb and
the D-Bus session created by ``dbus-run-session``; the user's desktop session,
clipboard, and panel are never touched.

Run with::

    python3 tests/tray_smoke.py --binary target/release/snipchord

The test needs ``dbus-run-session``, ``Xvfb``, ``xdotool``, ``xdpyinfo``, and
Python's ``dbus`` and ``gi`` bindings.  It reuses the private Xvfb helpers from
``rust_x11_smoke.py``.
"""

from __future__ import annotations

import argparse
import contextlib
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
from typing import Any, Mapping, Sequence


ROOT = Path(__file__).resolve().parents[1]
TESTS = ROOT / "tests"
if str(TESTS) not in sys.path:
    sys.path.insert(0, str(TESTS))

from rust_x11_smoke import (  # noqa: E402  (test helper import after path setup)
    DEFAULT_HEIGHT,
    DEFAULT_WIDTH,
    SmokeError,
    XvfbServer,
    _require_commands,
    _read_until,
    _resolve_binary,
    _run,
    _spawn,
    _terminate,
    _capture_window_png,
    _xdg_open_stub,
    _wait_for_opened_path,
    _gsettings_stub,
    _wait_for_instance_owner,
    _wait_for_window,
    _wait_for_window_geometry,
    _wait_for_window_hidden,
    _window_geometries,
    _assert_window_on_screen,
    _assert_window_centered,
)


SNI_NAME_PREFIX = "org.kde.StatusNotifierItem-"
SNI_PATH = "/StatusNotifierItem"
SNI_INTERFACE = "org.kde.StatusNotifierItem"
MENU_PATH = "/MenuBar"
MENU_INTERFACE = "com.canonical.dbusmenu"
DBUS_INTERFACE = "org.freedesktop.DBus"
DBUS_PROPERTIES = "org.freedesktop.DBus.Properties"
DBUS_PATH = "/org/freedesktop/DBus"
WATCHER_NAME = "org.kde.StatusNotifierWatcher"
WATCHER_PATH = "/StatusNotifierWatcher"
WATCHER_INTERFACE = "org.kde.StatusNotifierWatcher"

MENU_LABELS = {
    1: "Preferences",
    2: "About SnipChord",
    3: "Quit SnipChord",
}


def _require_python_dbus() -> None:
    try:
        import dbus  # noqa: F401
        import gi  # noqa: F401
        from dbus.mainloop.glib import DBusGMainLoop  # noqa: F401
    except ImportError as error:
        raise SmokeError(
            "Python dbus and GLib bindings are required for the isolated tray check"
        ) from error


def _dbus_bus() -> Any:
    import dbus

    return dbus.SessionBus()


def _dbus_daemon(bus: Any) -> Any:
    import dbus

    return dbus.Interface(bus.get_object(DBUS_INTERFACE, DBUS_PATH), DBUS_INTERFACE)


def _bus_names(bus: Any) -> set[str]:
    return {str(name) for name in _dbus_daemon(bus).ListNames()}


def _wait_for_tray(bus: Any, timeout: float = 8.0) -> tuple[str, Any]:
    """Find our SNI name and ensure its canonical object is already exported."""
    deadline = time.monotonic() + timeout
    last_error = ""
    while time.monotonic() < deadline:
        for name in sorted(_bus_names(bus)):
            if not name.startswith(SNI_NAME_PREFIX):
                continue
            try:
                item = bus.get_object(name, SNI_PATH)
                props = __import__("dbus").Interface(item, DBUS_PROPERTIES)
                # Querying the properties also waits for zbus to finish serving
                # the object after it claims the generated well-known name.
                icon_name = str(props.Get(SNI_INTERFACE, "IconName"))
                menu_path = str(props.Get(SNI_INTERFACE, "Menu"))
                if icon_name == "snipchord" and menu_path == MENU_PATH:
                    return name, item
            except Exception as error:  # object server may be one event behind name ownership
                last_error = str(error)
        time.sleep(0.05)
    suffix = f"; last error={last_error}" if last_error else ""
    raise SmokeError(f"SnipChord StatusNotifierItem did not appear{suffix}")


def _unwrap_layout_value(value: Any) -> Any:
    # dbus-python exposes a variant as its contained scalar in most cases, but
    # keeps a Variant wrapper for some signatures.  The string conversion is
    # intentionally limited to labels below so a malformed tree still fails
    # the exact ID/layout assertions.
    return value


def _layout_nodes(layout: Any) -> list[tuple[int, dict[str, Any], list[Any]]]:
    """Flatten a dbusmenu GetLayout response while retaining child order."""
    result: list[tuple[int, dict[str, Any], list[Any]]] = []

    def visit(node: Any) -> None:
        try:
            node_id, properties, children = node
        except (TypeError, ValueError) as error:
            raise SmokeError(f"invalid dbusmenu layout node: {node!r}") from error
        props = {str(key): _unwrap_layout_value(value) for key, value in properties.items()}
        children_list = list(children)
        result.append((int(node_id), props, children_list))
        for child in children_list:
            visit(child)

    visit(layout)
    return result


def _tray_menu(bus: Any, name: str) -> tuple[Any, dict[int, str]]:
    import dbus

    item = bus.get_object(name, SNI_PATH)
    props = dbus.Interface(item, DBUS_PROPERTIES)
    if str(props.Get(SNI_INTERFACE, "Menu")) != MENU_PATH:
        raise SmokeError("StatusNotifierItem.Menu did not point to /MenuBar")
    if not bool(props.Get(SNI_INTERFACE, "ItemIsMenu")):
        raise SmokeError("StatusNotifierItem.ItemIsMenu is false")

    menu = bus.get_object(name, MENU_PATH)
    menu_iface = dbus.Interface(menu, MENU_INTERFACE)
    _, layout = menu_iface.GetLayout(
        dbus.Int32(0),
        dbus.Int32(-1),
        dbus.Array([], signature="s"),
    )
    nodes = _layout_nodes(layout)
    if not nodes or nodes[0][0] != 0:
        raise SmokeError(f"dbusmenu root ID was not 0: {nodes!r}")

    root_children = list(layout[2])
    root_ids = [int(child[0]) for child in root_children]
    if root_ids != [1, 2, 3]:
        raise SmokeError(f"unexpected SnipChord menu IDs: {root_ids!r}")

    labels: dict[int, str] = {}
    for node_id, properties, children in nodes:
        if "label" in properties:
            labels[node_id] = str(properties["label"])
        if node_id in MENU_LABELS and children:
            raise SmokeError(f"action menu ID {node_id} unexpectedly had children")
    for node_id, expected in MENU_LABELS.items():
        observed = labels.get(node_id)
        if observed != expected:
            raise SmokeError(
                f"menu ID {node_id} label was {observed!r}, expected {expected!r}"
            )
    return menu_iface, labels


def _click_menu(menu_iface: Any, menu_id: int, *, allow_disconnect: bool = False) -> None:
    import dbus

    # com.canonical.dbusmenu.Event has signature (isvu): ID, event, variant,
    # timestamp.  An empty string variant matches ksni's ignored data value.
    try:
        menu_iface.Event(
            dbus.Int32(menu_id),
            dbus.String("clicked"),
            dbus.String(""),
            dbus.UInt32(0),
        )
    except dbus.exceptions.DBusException as error:
        # Quit asks the resident to close its own ksni connection.  Depending
        # on scheduling, zbus can drop that connection before the synchronous
        # Event reply reaches this requestor; process and name cleanup below
        # remain the authoritative assertions for that action.
        if not allow_disconnect or "NoReply" not in str(error):
            raise


def _wait_for_watcher_log(path: Path, expected_service: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            entries = [line.strip() for line in path.read_text(errors="replace").splitlines()]
            if expected_service in entries:
                return
        time.sleep(0.05)
    observed = path.read_text(errors="replace") if path.exists() else "<no log>"
    raise SmokeError(
        f"late StatusNotifierWatcher did not receive {expected_service!r}; log={observed!r}"
    )


def _watcher_process(env: Mapping[str, str], log_path: Path) -> subprocess.Popen[bytes]:
    watcher_env = dict(env)
    watcher_env["SNIPCHORD_TRAY_WATCHER_LOG"] = str(log_path)
    return _spawn([sys.executable, str(Path(__file__).resolve()), "--watcher"], watcher_env)


def _run_watcher() -> int:
    """Own a tiny late watcher so ksni's NameOwnerChanged retry is observable."""
    _require_python_dbus()
    from dbus.mainloop.glib import DBusGMainLoop

    DBusGMainLoop(set_as_default=True)
    import dbus
    import dbus.service
    from gi.repository import GLib

    log_path = Path(os.environ["SNIPCHORD_TRAY_WATCHER_LOG"])
    bus = dbus.SessionBus()
    reply = bus.request_name(WATCHER_NAME, dbus.bus.NAME_FLAG_DO_NOT_QUEUE)
    if reply != dbus.bus.REQUEST_NAME_REPLY_PRIMARY_OWNER:
        print("watcher could not own org.kde.StatusNotifierWatcher", flush=True)
        return 1

    class Watcher(dbus.service.Object):
        def __init__(self) -> None:
            super().__init__(bus, WATCHER_PATH)

        @dbus.service.method(WATCHER_INTERFACE, in_signature="s", out_signature="")
        def RegisterStatusNotifierItem(self, service: str) -> None:  # noqa: N802
            log_path.parent.mkdir(parents=True, exist_ok=True)
            with log_path.open("a", encoding="utf-8") as log:
                log.write(f"{service}\n")

        @dbus.service.method(WATCHER_INTERFACE, in_signature="s", out_signature="")
        def RegisterStatusNotifierHost(self, _service: str) -> None:  # noqa: N802
            return None

        @dbus.service.method(DBUS_PROPERTIES, in_signature="ss", out_signature="v")
        def Get(self, interface: str, prop: str) -> Any:  # noqa: N802
            if interface != WATCHER_INTERFACE:
                raise dbus.exceptions.DBusException("org.freedesktop.DBus.Error.UnknownInterface")
            if prop == "RegisteredStatusNotifierItems":
                return dbus.Array([], signature="s")
            if prop == "IsStatusNotifierHostRegistered":
                return dbus.Boolean(True)
            if prop == "ProtocolVersion":
                return dbus.Int32(0)
            raise dbus.exceptions.DBusException("org.freedesktop.DBus.Error.UnknownProperty")

        @dbus.service.method(DBUS_PROPERTIES, in_signature="s", out_signature="a{sv}")
        def GetAll(self, interface: str) -> dict[str, Any]:  # noqa: N802
            if interface != WATCHER_INTERFACE:
                return {}
            return {
                "RegisteredStatusNotifierItems": dbus.Array([], signature="s"),
                "IsStatusNotifierHostRegistered": dbus.Boolean(True),
                "ProtocolVersion": dbus.Int32(0),
            }

    watcher = Watcher()
    loop = GLib.MainLoop()
    signal.signal(signal.SIGTERM, lambda *_args: loop.quit())
    signal.signal(signal.SIGINT, lambda *_args: loop.quit())
    print("WATCHER_READY", flush=True)
    loop.run()
    del watcher
    bus.release_name(WATCHER_NAME)
    return 0


def _wait_for_watcher_ready(process: subprocess.Popen[bytes], timeout: float = 4.0) -> None:
    _read_until(process, __import__("re").compile(r"WATCHER_READY"), timeout)


def _wm_process(env: Mapping[str, str]) -> subprocess.Popen[bytes]:
    """Start the deliberately slow private window manager fixture.

    A bare Xvfb maps windows immediately because there is no manager to own the
    MapRequest.  The user's failure happened with a real WM: SetInputFocus can
    return BadMatch in the small interval between MapWindow and the WM's map.
    Running this fixture makes that interval deterministic without touching the
    user's desktop WM.
    """
    return _spawn([sys.executable, str(Path(__file__).resolve()), "--wm"], env)


def _run_wm() -> int:
    """Own and slowly service MapRequest events on the isolated X display."""
    try:
        from Xlib import X, display, error
    except ImportError as exc:
        raise SmokeError("python-xlib is required for the delayed WM fixture") from exc

    connection = display.Display()
    root = connection.screen().root
    try:
        # SubstructureRedirect makes this process the private display's window
        # manager.  No reparenting is needed; applying requests on the root is
        # enough to reproduce the viewability race and keeps xdotool titles
        # stable for this smoke test.
        root.change_attributes(
            event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask
        )
        connection.sync()
    except error.BadAccess as exc:
        connection.close()
        raise SmokeError(f"could not own the isolated X11 WM selection: {exc}") from exc

    try:
        print("WM_READY", flush=True)
        delay = max(0.0, float(os.environ.get("SNIPCHORD_WM_MAP_DELAY_MS", "350")) / 1000.0)
        while True:
            event = connection.next_event()
            if event.type == X.MapRequest:
                time.sleep(delay)
                event.window.map()
                connection.flush()
            elif event.type == X.ConfigureRequest:
                values: dict[str, int] = {}
                mask = event.value_mask
                if mask & X.CWX:
                    values["x"] = event.x
                if mask & X.CWY:
                    values["y"] = event.y
                if mask & X.CWWidth:
                    values["width"] = event.width
                if mask & X.CWHeight:
                    values["height"] = event.height
                if mask & X.CWBorderWidth:
                    values["border_width"] = event.border_width
                if mask & X.CWSibling:
                    values["sibling"] = event.above_sibling
                if mask & X.CWStackMode:
                    values["stack_mode"] = event.stack_mode
                if values:
                    event.window.configure(**values)
                    connection.flush()
    finally:
        connection.close()


def _wait_for_wm_ready(process: subprocess.Popen[bytes], timeout: float = 4.0) -> None:
    _read_until(process, __import__("re").compile(r"WM_READY"), timeout)


def _new_fullscreen_window_ids(env: Mapping[str, str]) -> set[str]:
    return {
        str(window["id"])
        for window in _window_geometries(env)
        if int(window["width"]) == 1024 and int(window["height"]) == 768
    }


def _close_named_window(env: Mapping[str, str], name: str, timeout: float = 3) -> None:
    """Close one focused transient surface and verify it disappears."""
    windows = _run(
        ["xdotool", "search", "--name", name],
        env,
        check=False,
        timeout=2,
    ).stdout.decode(errors="replace").split()
    if windows:
        # The tiny fixture WM deliberately does not implement an EWMH focus
        # policy.  Focus the known test window explicitly so Escape tests the
        # app's close path rather than the fixture's policy.
        _run(["xdotool", "windowfocus", "--sync", windows[-1]], env, timeout=3)
    _run(["xdotool", "key", "Escape"], env, timeout=3)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not _run(
            ["xdotool", "search", "--name", name],
            env,
            check=False,
            timeout=2,
        ).stdout.strip():
            return
        time.sleep(0.05)
    raise SmokeError(f"{name} menu action did not close after Escape")


def _wait_for_viewable_named_window(
    env: Mapping[str, str], name: str, timeout: float = 5
) -> str:
    """Wait until the title is present and the WM has made it viewable."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        candidates = _run(
            ["xdotool", "search", "--name", name],
            env,
            check=False,
            timeout=2,
        ).stdout.decode(errors="replace").split()
        for window in reversed(candidates):
            state = _run(["xwininfo", "-id", window], env, check=False, timeout=2)
            if state.returncode == 0 and "Map State: IsViewable" in state.stdout.decode(
                errors="replace"
            ):
                return window
        time.sleep(0.05)
    raise SmokeError(f"viewable window {name!r} did not appear")


def _close_about(env: Mapping[str, str]) -> None:
    """Close About through Escape; the surface intentionally has no button."""
    _close_named_window(env, "About SnipChord")


def _click_window_relative(
    env: Mapping[str, str], window_id: str, x: int, y: int
) -> None:
    """Click a native surface using coordinates relative to its X11 window."""
    _run(["xdotool", "windowfocus", "--sync", window_id], env, timeout=3)
    _run(
        ["xdotool", "mousemove", "--window", window_id, str(x), str(y)],
        env,
        timeout=3,
    )
    _run(["xdotool", "click", "1"], env, timeout=3)
    time.sleep(0.2)


def _capture_preferences_tab(
    env: Mapping[str, str], window_id: str, tab: str, destination: Path
) -> None:
    """Select one sidebar tab and persist its rendered surface for visual QA."""
    # The dialog follows the compact two-item sidebar from the design reference:
    # General is the first row and Shortcuts the second.  Keep these points in
    # one place so a layout adjustment changes the test contract explicitly.
    tab_point = {"general": (84, 124), "shortcuts": (84, 174)}.get(tab)
    if tab_point is None:
        raise SmokeError(f"unknown Preferences tab {tab!r}")
    _click_window_relative(env, window_id, *tab_point)
    _capture_window_png(env, window_id, destination)
    if not destination.is_file() or destination.stat().st_size == 0:
        raise SmokeError(f"Preferences {tab} capture was not written: {destination}")


def _click_repository_link(env: Mapping[str, str], window_id: str) -> None:
    """Activate About's compact GitHub link without opening a real browser."""
    geometry = next(
        (
            candidate
            for candidate in _window_geometries(env)
            if int(str(candidate["id"]), 0) == int(window_id, 0)
        ),
        None,
    )
    if geometry is None:
        raise SmokeError(f"could not find About geometry for link click {window_id}")
    # The repository mark/link is centered below the description in the compact
    # About surface.  Use a center click so the icon and text share one hitbox.
    _click_window_relative(
        env,
        window_id,
        int(geometry["width"]) // 2,
        150,
    )


def _send_wm_delete(env: Mapping[str, str], window_id: str) -> None:
    """Send the ICCCM close request that a window manager sends on close."""
    try:
        from Xlib import X, display
        from Xlib.protocol import event
    except ImportError as error:
        raise SmokeError("python-xlib is required for WM_DELETE verification") from error

    connection = display.Display(env["DISPLAY"])
    try:
        window = connection.create_resource_object("window", int(window_id, 0))
        wm_protocols = connection.intern_atom("WM_PROTOCOLS")
        wm_delete_window = connection.intern_atom("WM_DELETE_WINDOW")
        message = event.ClientMessage(
            window=window.id,
            client_type=wm_protocols,
            data=(32, [wm_delete_window, X.CurrentTime, 0, 0, 0]),
        )
        window.send_event(message, event_mask=0, propagate=False)
        connection.flush()
    finally:
        connection.close()


def _temporary_focus_window(env: Mapping[str, str]) -> tuple[Any, Any]:
    """Create a viewable, focused X11 window that the fixture can destroy."""
    try:
        from Xlib import X, display
    except ImportError as error:
        raise SmokeError("python-xlib is required for vanished-focus verification") from error

    connection = display.Display(env["DISPLAY"])
    try:
        sentinel = connection.screen().root.create_window(
            2,
            2,
            8,
            8,
            0,
            X.CopyFromParent,
            X.InputOutput,
            X.CopyFromParent,
            override_redirect=1,
        )
        sentinel.map()
        sentinel.set_input_focus(X.RevertToParent, X.CurrentTime)
        connection.flush()
        connection.sync()
        focus = connection.get_input_focus().focus
        focus_id = getattr(focus, "id", focus)
        if focus_id != sentinel.id:
            sentinel.destroy()
            connection.flush()
            connection.close()
            raise SmokeError(
                f"could not establish temporary focus window: {focus_id!r} != {sentinel.id!r}"
            )
        return connection, sentinel
    except Exception:
        connection.close()
        raise


def _exercise_vanished_focus_wm_delete(
    menu_iface: Any, daemon: subprocess.Popen[bytes], env: Mapping[str, str]
) -> list[str]:
    """Close dialogs after their saved focus window has been destroyed.

    A tray popup can be the focused X11 window when it opens a dialog.  That
    popup may disappear before the user closes Preferences/About.  The daemon
    must treat the stale focus ID as recoverable when processing WM_DELETE.
    """
    exercised: list[str] = []
    for menu_id, title in ((1, "SnipChord Preferences"), (2, "About SnipChord")):
        connection, sentinel = _temporary_focus_window(env)
        try:
            _click_menu(menu_iface, menu_id)
            dialog = _wait_for_viewable_named_window(env, title, 5)
            sentinel.destroy()
            connection.flush()
            connection.sync()
            _send_wm_delete(env, dialog)
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                if not _run(
                    ["xdotool", "search", "--name", title],
                    env,
                    check=False,
                    timeout=2,
                ).stdout.strip():
                    break
                time.sleep(0.05)
            else:
                raise SmokeError(f"{title} did not close after WM_DELETE_WINDOW")
            if daemon.poll() is not None:
                raise SmokeError(
                    f"daemon exited after {title} WM_DELETE with vanished focus "
                    f"({daemon.returncode})"
                )
            exercised.append(title)
        finally:
            with contextlib.suppress(Exception):
                sentinel.destroy()
                connection.flush()
            connection.close()
    return exercised


def _exercise_menu(menu_iface: Any, daemon: subprocess.Popen[bytes], env: Mapping[str, str]) -> dict[str, Any]:
    """Exercise the three tray surfaces without starting a capture."""
    # Open and close Preferences twice.  The second invocation catches stale
    # focus/window state, while the delayed private WM makes the initial
    # MapRequest -> MapNotify ordering deterministic.
    preferences: list[str] = []
    for _ in range(2):
        _click_menu(menu_iface, 1)
        window = _wait_for_viewable_named_window(env, "SnipChord Preferences", 5)
        geometry = next(
            (
                candidate
                for candidate in _window_geometries(env)
                if int(str(candidate["id"]), 0) == int(window, 0)
            ),
            None,
        )
        if geometry is None:
            raise SmokeError(f"could not find Preferences geometry for {window}")
        _assert_window_on_screen(geometry, (DEFAULT_WIDTH, DEFAULT_HEIGHT), "Preferences")
        _assert_window_centered(geometry, (DEFAULT_WIDTH, DEFAULT_HEIGHT), "Preferences")
        if not preferences:
            preferences_png = Path(
                os.environ.get(
                    "SNIPCHORD_PREFERENCES_QA_PATH",
                    "/tmp/snipchord-preferences-general.png",
                )
            )
            _capture_preferences_tab(env, window, "general", preferences_png)
            shortcuts_png = Path(
                os.environ.get(
                    "SNIPCHORD_SHORTCUTS_QA_PATH",
                    "/tmp/snipchord-preferences-shortcuts.png",
                )
            )
            _capture_preferences_tab(env, window, "shortcuts", shortcuts_png)
            if preferences_png.read_bytes() == shortcuts_png.read_bytes():
                raise SmokeError("Preferences General and Shortcuts tabs rendered identically")
        preferences.append(window)
        _close_named_window(env, "SnipChord Preferences")

    # About is a normal transient X11 window too.  Keep a capture of its
    # pixels in the private runtime directory so a reviewer can inspect the
    # rendered name, version, and compact GitHub link without requiring an
    # accessibility toolkit on the test host.
    _click_menu(menu_iface, 2)
    about = _wait_for_viewable_named_window(env, "About SnipChord", 5)
    about_png = Path(
        os.environ.get(
            "SNIPCHORD_ABOUT_QA_PATH",
            "/tmp/snipchord-about.png",
        )
    )
    _capture_window_png(env, about, about_png)
    if not about_png.is_file() or about_png.stat().st_size == 0:
        raise SmokeError(f"About window capture was not written: {about_png}")
    about_geometry = next(
        (
            candidate
            for candidate in _window_geometries(env)
            if int(str(candidate["id"]), 0) == int(about, 0)
        ),
        None,
    )
    if about_geometry is None:
        raise SmokeError(f"could not find About geometry for {about}")
    _assert_window_on_screen(about_geometry, (DEFAULT_WIDTH, DEFAULT_HEIGHT), "About")
    _assert_window_centered(about_geometry, (DEFAULT_WIDTH, DEFAULT_HEIGHT), "About")
    # The old full-width Close button occupied the bottom edge. A click there
    # must leave the new link-only About surface open; Escape is its close path.
    _click_window_relative(env, about, int(about_geometry["width"]) // 2, 205)
    if not _run(
        ["xdotool", "search", "--name", "About SnipChord"],
        env,
        check=False,
        timeout=2,
    ).stdout.strip():
        raise SmokeError("About closed from the removed Close-button area")
    link_env, link_log = _xdg_open_stub(env)
    # The daemon inherits the same private environment, but callers may pass a
    # copied mapping.  Update it in place so xdotool and the child see one stub.
    if isinstance(env, dict):
        env.update(link_env)
    _click_repository_link(env, about)
    _wait_for_opened_path(link_log)
    opened_url = link_log.read_text(errors="replace").splitlines()[-1].strip()
    if opened_url != "https://github.com/ShanePark/snipchord":
        raise SmokeError(f"About GitHub link opened unexpected target: {opened_url}")
    _close_about(env)
    if daemon.poll() is not None:
        raise SmokeError(f"daemon exited after tray surfaces ({daemon.returncode})")
    vanished_focus = _exercise_vanished_focus_wm_delete(menu_iface, daemon, env)
    return {
        "preferences_windows": preferences,
        "preferences_png": str(
            os.environ.get(
                "SNIPCHORD_PREFERENCES_QA_PATH",
                "/tmp/snipchord-preferences-general.png",
            )
        ),
        "shortcuts_png": str(
            os.environ.get(
                "SNIPCHORD_SHORTCUTS_QA_PATH",
                "/tmp/snipchord-preferences-shortcuts.png",
            )
        ),
        "about_window": about,
        "about_png": str(about_png),
        "repository_url": opened_url,
        "wm_delete_vanished_focus": vanished_focus,
    }


def _run_smoke(args: argparse.Namespace) -> int:
    _require_python_dbus()
    missing = _require_commands(["xdotool", "xdpyinfo"])
    if missing:
        print(f"SKIP: missing commands: {', '.join(missing)}", file=sys.stderr)
        return 2
    try:
        binary = _resolve_binary(args.binary)
    except SmokeError as error:
        print(f"SKIP: {error}", file=sys.stderr)
        return 2

    with __import__("tempfile").TemporaryDirectory(prefix="snipchord-tray-") as temporary:
        runtime = Path(temporary)
        with XvfbServer(runtime, 1024, 768, args.display) as server:
            # XvfbServer intentionally strips desktop D-Bus from its child
            # environment.  Put back only this process's private dbus-run-session
            # address; it is never the live desktop address.
            env = dict(server.env)
            env["DBUS_SESSION_BUS_ADDRESS"] = os.environ["DBUS_SESSION_BUS_ADDRESS"]
            env["SNIPCHORD_WM_MAP_DELAY_MS"] = "350"
            # About's repository link must be exercised through a private
            # xdg-open shim. Install it before spawning the resident daemon so
            # the child inherits the safe PATH and log location.
            env, _ = _xdg_open_stub(env)
            env, gsettings_log = _gsettings_stub(env)
            wm = _wm_process(env)
            bus = None
            daemon = None
            watcher = None
            try:
                _wait_for_wm_ready(wm)
                bus = _dbus_bus()
                daemon = _spawn([str(binary), "--daemon"], env)
                _wait_for_instance_owner(env)
                name, item = _wait_for_tray(bus)
                props = __import__("dbus").Interface(item, DBUS_PROPERTIES)
                observed = {
                    "id": str(props.Get(SNI_INTERFACE, "Id")),
                    "title": str(props.Get(SNI_INTERFACE, "Title")),
                    "icon_name": str(props.Get(SNI_INTERFACE, "IconName")),
                    "menu": str(props.Get(SNI_INTERFACE, "Menu")),
                    "item_is_menu": bool(props.Get(SNI_INTERFACE, "ItemIsMenu")),
                }
                if observed != {
                    "id": "snipchord",
                    "title": "SnipChord",
                    "icon_name": "snipchord",
                    "menu": MENU_PATH,
                    "item_is_menu": True,
                }:
                    raise SmokeError(f"unexpected SNI properties: {observed!r}")
                menu_iface, labels = _tray_menu(bus, name)

                # A nonexistent watcher is the expected initial state.  The
                # private item remains exported and the core daemon stays live.
                if WATCHER_NAME in _bus_names(bus):
                    raise SmokeError("private tray test unexpectedly had an initial watcher")
                if daemon.poll() is not None:
                    raise SmokeError(f"daemon exited with no watcher ({daemon.returncode})")

                log_path = runtime / "watcher.log"
                watcher = _watcher_process(env, log_path)
                _wait_for_watcher_ready(watcher)
                _wait_for_watcher_log(log_path, name)
                if daemon.poll() is not None:
                    raise SmokeError(f"daemon exited after late watcher ({daemon.returncode})")

                actions = _exercise_menu(menu_iface, daemon, env)
                if not gsettings_log.is_file():
                    raise SmokeError("Preferences did not query the shortcut settings fixture")
                _click_menu(menu_iface, 3, allow_disconnect=True)
                deadline = time.monotonic() + 6
                while daemon.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.05)
                if daemon.poll() is None:
                    raise SmokeError("Quit menu action did not stop the resident daemon")
                if name in _bus_names(bus):
                    raise SmokeError("SNI well-known name remained after Quit")
                print(
                    f"PASS SNI name={name} path={SNI_PATH} menu={MENU_PATH} "
                    f"labels={labels} late_watcher=True actions={actions} quit=True"
                )
            finally:
                if watcher is not None:
                    _terminate(watcher)
                with contextlib.suppress(Exception):
                    if daemon is not None and daemon.poll() is None:
                        _run([str(binary), "--quit"], env, timeout=3)
                _terminate(daemon)
                if bus is not None:
                    bus.close()
                _terminate(wm)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", help="Rust snipchord executable")
    parser.add_argument("--display", type=int, help="isolated X display number")
    parser.add_argument("--watcher", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--wm", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--inner", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)

    if args.watcher:
        return _run_watcher()
    if args.wm:
        try:
            return _run_wm()
        except SmokeError as error:
            print(f"FAIL: {error}", file=sys.stderr)
            return 1
    if not args.inner:
        if shutil.which("dbus-run-session") is None:
            print("SKIP: dbus-run-session was not found", file=sys.stderr)
            return 2
        child_env = dict(os.environ)
        child_env.pop("DBUS_SESSION_BUS_ADDRESS", None)
        command = [
            "dbus-run-session",
            "--",
            sys.executable,
            str(Path(__file__).resolve()),
            "--inner",
        ]
        if args.binary is not None:
            command.extend(["--binary", args.binary])
        if args.display is not None:
            command.extend(["--display", str(args.display)])
        result = subprocess.run(command, env=child_env)
        return result.returncode
    try:
        return _run_smoke(args)
    except SmokeError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    except (OSError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
