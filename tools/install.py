#!/usr/bin/env python3
"""Install a prebuilt user app, with opt-in GNOME shortcut setup."""

import argparse
import ast
from dataclasses import dataclass
from pathlib import Path
import shutil
import os
import shlex
import subprocess
import tempfile


MEDIA_KEYS_SCHEMA = "org.gnome.settings-daemon.plugins.media-keys"
CUSTOM_KEYBINDING_SCHEMA = (
    "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding"
)
CUSTOM_KEYBINDINGS_KEY = "custom-keybindings"
GNOME_SHELL_KEYBINDINGS_SCHEMA = "org.gnome.shell.keybindings"
GNOME_SHELL_SCREENSHOT_KEY = "screenshot"

# GNOME's built-in screenshot shortcut is represented as the ``numbersign``
# keysym on a US layout (Shift+3).  It otherwise overlaps SnipChord's
# Ctrl+Alt+Shift+3 shortcut.  Keep the removal deliberately narrow: the
# screenshot-window binding (and every other value in this key) belongs to the
# user and must remain intact.
CONFLICTING_SCREENSHOT_BINDINGS = frozenset(
    {
        "<Primary><Shift><Alt>numbersign",
        "<Primary><Alt><Shift>numbersign",
        "<Control><Shift><Alt>numbersign",
        "<Control><Alt><Shift>numbersign",
        "<Primary><Shift><Alt>3",
        "<Primary><Alt><Shift>3",
        "<Control><Shift><Alt>3",
        "<Control><Alt><Shift>3",
    }
)


@dataclass(frozen=True)
class Shortcut:
    """One SnipChord shortcut managed by the optional installer switch."""

    name: str
    binding: str
    arguments: tuple[str, ...]


SHORTCUTS = (
    Shortcut(
        "SnipChord Region to Clipboard",
        "<Control><Alt><Shift>4",
        ("--region", "--clipboard"),
    ),
    Shortcut(
        "SnipChord Region to File",
        "<Alt><Shift>4",
        ("--region", "--save"),
    ),
    Shortcut(
        "SnipChord Fullscreen to Clipboard",
        "<Control><Alt><Shift>3",
        ("--fullscreen", "--clipboard"),
    ),
    Shortcut(
        "SnipChord Fullscreen to File",
        "<Alt><Shift>3",
        ("--fullscreen", "--save"),
    ),
)


def _gsettings(*arguments):
    """Run gsettings and return stdout, surfacing useful errors to callers."""
    try:
        result = subprocess.run(
            ["gsettings", *arguments],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError as error:
        raise RuntimeError("gsettings was not found; install it before using --shortcuts") from error
    except subprocess.CalledProcessError as error:
        detail = error.stderr.strip() or error.stdout.strip() or "unknown gsettings error"
        raise RuntimeError(f"gsettings {' '.join(arguments)} failed: {detail}") from error
    return result.stdout.strip()


def _gvariant_string(value):
    """Decode a string returned by gsettings get."""
    try:
        decoded = ast.literal_eval(value)
    except (SyntaxError, ValueError) as error:
        raise RuntimeError(f"could not parse gsettings value {value!r}") from error
    if not isinstance(decoded, str):
        raise RuntimeError(f"expected a string from gsettings, got {value!r}")
    return decoded


def _gvariant_string_list(value):
    """Decode the custom-keybindings string-array returned by gsettings."""
    value = value.strip()
    if value.startswith("@as "):
        value = value[4:].lstrip()
    try:
        decoded = ast.literal_eval(value)
    except (SyntaxError, ValueError) as error:
        raise RuntimeError(f"could not parse custom-keybindings value {value!r}") from error
    if not isinstance(decoded, list) or not all(isinstance(item, str) for item in decoded):
        raise RuntimeError(f"expected a string list from gsettings, got {value!r}")
    return decoded


def _encode_gvariant_string_list(values):
    """Encode a Python string list as one gsettings string-array argument."""
    return "[" + ", ".join(repr(value) for value in values) + "]"


def _encode_gvariant_string(value):
    """Encode one Python string as a gsettings string argument."""
    return repr(value)


def _binding_schema(path):
    """Return the relocatable custom-keybinding schema at *path*."""
    return f"{CUSTOM_KEYBINDING_SCHEMA}:{path}"


def _read_custom_keybindings():
    paths = _gvariant_string_list(_gsettings("get", MEDIA_KEYS_SCHEMA, CUSTOM_KEYBINDINGS_KEY))
    entries = []
    for path in paths:
        schema = _binding_schema(path)
        entries.append(
            {
                "path": path,
                "name": _gvariant_string(_gsettings("get", schema, "name")),
                "command": _gvariant_string(_gsettings("get", schema, "command")),
                "binding": _gvariant_string(_gsettings("get", schema, "binding")),
            }
        )
    return paths, entries


def _new_custom_path(paths):
    """Allocate an unused conventional custom-keybinding path."""
    used = set(paths)
    index = 0
    while True:
        candidate = (
            "/org/gnome/settings-daemon/plugins/media-keys/"
            f"custom-keybindings/custom{index}/"
        )
        if candidate not in used:
            return candidate
        index += 1


def _managed_entry(entry, shortcut):
    """Recognize a prior SnipChord entry without touching unrelated bindings.

    The stable name is the primary marker.  The command fallback also lets an
    installation whose display name was edited in GNOME settings be updated
    when its command still points to SnipChord with the same capture mode.
    """
    if entry["name"] == shortcut.name:
        return True
    try:
        command_parts = shlex.split(entry["command"])
    except ValueError:
        return False
    if not command_parts or Path(command_parts[0]).name != "snipchord":
        return False
    if command_parts[1:] == list(shortcut.arguments):
        return True

    # Before destination flags were introduced, the default region/fullscreen
    # command copied to the clipboard.  Reuse that real binding instead of
    # installing a duplicate accelerator (GNOME identifies Ctrl as either
    # <Control> or <Primary> in existing configurations).
    legacy_binding = {
        ("--region", "--clipboard"): "<Primary><Alt><Shift>4",
        ("--fullscreen", "--clipboard"): "<Primary><Alt><Shift>3",
    }.get(shortcut.arguments)
    return (
        legacy_binding is not None
        and len(command_parts) == 2
        and command_parts[1:] == [shortcut.arguments[0]]
        and entry["binding"] in {legacy_binding, shortcut.binding}
    )


def configure_shortcuts(launcher):
    """Install or update SnipChord's four GNOME shortcuts.

    Existing custom bindings remain in their original order and are never
    removed.  Re-running this function updates the four entries in place,
    making the operation idempotent while allowing a changed install prefix.
    The one overlapping GNOME built-in screenshot accelerator is removed from
    its list while every other built-in screenshot value is preserved.
    """
    launcher = Path(launcher).resolve()
    paths, entries = _read_custom_keybindings()
    assignments = []
    used_paths = set(paths)
    for shortcut in SHORTCUTS:
        entry = next((item for item in entries if _managed_entry(item, shortcut)), None)
        if entry is None:
            path = _new_custom_path(list(used_paths))
            used_paths.add(path)
            paths.append(path)
            entry = {"path": path, "name": "", "command": "", "binding": ""}
            entries.append(entry)
        command = " ".join(
            [shlex.quote(str(launcher)), *(shlex.quote(argument) for argument in shortcut.arguments)]
        )
        assignments.append((entry["path"], shortcut, command))

    # Configure relocatable child schemas before publishing the paths in the
    # parent list.  A failed command therefore cannot expose a half-created
    # empty shortcut to GNOME.
    for path, shortcut, command in assignments:
        schema = _binding_schema(path)
        _gsettings("set", schema, "name", _encode_gvariant_string(shortcut.name))
        _gsettings("set", schema, "command", _encode_gvariant_string(command))
        _gsettings("set", schema, "binding", _encode_gvariant_string(shortcut.binding))
    _gsettings(
        "set",
        MEDIA_KEYS_SCHEMA,
        CUSTOM_KEYBINDINGS_KEY,
        _encode_gvariant_string_list(paths),
    )
    screenshot_bindings = _gvariant_string_list(
        _gsettings(
            "get",
            GNOME_SHELL_KEYBINDINGS_SCHEMA,
            GNOME_SHELL_SCREENSHOT_KEY,
        )
    )
    remaining_screenshot_bindings = [
        binding
        for binding in screenshot_bindings
        if binding not in CONFLICTING_SCREENSHOT_BINDINGS
    ]
    if remaining_screenshot_bindings != screenshot_bindings:
        _gsettings(
            "set",
            GNOME_SHELL_KEYBINDINGS_SCHEMA,
            GNOME_SHELL_SCREENSHOT_KEY,
            _encode_gvariant_string_list(remaining_screenshot_bindings),
        )
    return tuple(paths)


def install_binary(source, destination):
    """Replace an installed binary without truncating one that is running."""
    fd, temporary_name = tempfile.mkstemp(prefix=f".{destination.name}-", dir=destination.parent)
    os.close(fd)
    temporary = Path(temporary_name)
    try:
        shutil.copy2(source, temporary)
        temporary.chmod(0o755)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix",type=Path,default=Path.home()/".local",help="installation prefix (default ~/.local)")
    parser.add_argument("--autostart",action="store_true",help="start SnipChord at login; optional")
    parser.add_argument(
        "--shortcuts",
        action="store_true",
        help="install or update SnipChord's GNOME screenshot shortcuts; optional",
    )
    args=parser.parse_args()
    repo=Path(__file__).resolve().parents[1]
    prefix=args.prefix.expanduser().resolve()
    if args.autostart and prefix != (Path.home()/".local").resolve():
        parser.error("--autostart requires the default prefix")
    bundle=prefix/"share"/"snipchord"
    launcher=prefix/"bin"/"snipchord"
    icon_path=prefix/"share"/"icons"/"hicolor"/"scalable"/"apps"/"snipchord.svg"
    marker=bundle/".snipchord-managed"
    if (os.path.lexists(bundle) and not marker.exists()) or (os.path.lexists(launcher) and not marker.exists()):
        parser.error("Refusing to overwrite an unmanaged installation")
    if not marker.exists() and os.path.lexists(icon_path):
        parser.error("Refusing to overwrite an unmanaged icon")
    desktop_path = prefix/"share"/"applications"/"io.github.shane.snipchord.desktop"
    autostart_path = Path(os.environ.get("XDG_CONFIG_HOME",Path.home()/".config"))/"autostart"/"io.github.shane.snipchord.desktop"
    if not marker.exists() and (os.path.lexists(desktop_path) or (args.autostart and os.path.lexists(autostart_path))):
        parser.error("Refusing to overwrite unmanaged desktop entries")
    binary_source=repo/"target"/"release"/"snipchord"
    if not binary_source.is_file() or not os.access(binary_source, os.X_OK):
        parser.error(
            f"Release binary not found or not executable: {binary_source}\n"
            "Build it first with: cargo build --release"
        )
    bundle.mkdir(parents=True,exist_ok=True)
    shutil.copy2(repo/"assets"/"snipchord.svg",bundle/"snipchord.svg")
    icon_path.parent.mkdir(parents=True,exist_ok=True)
    shutil.copy2(repo/"assets"/"snipchord.svg",icon_path)
    marker.write_text("Managed by SnipChord tools/install.py\n")
    launcher.parent.mkdir(parents=True,exist_ok=True)
    install_binary(binary_source, launcher)
    # Desktop Exec escaping is separate from shell escaping.
    executable=str(launcher).replace('\\','\\\\').replace('"','\\"').replace('`','\\`').replace('$','\\$').replace('%','%%')
    desktop=("[Desktop Entry]\nType=Application\nName=SnipChord\n"
             "Comment=A macOS-style screenshot experience for Linux\n"
             f'Exec="{executable}" --region\nIcon={bundle}/snipchord.svg\n'
             "Terminal=false\nCategories=Graphics;\nStartupNotify=false\n"
             "Actions=Region;Fullscreen;Preferences;Quit;\n\n"
             f'[Desktop Action Region]\nName=Capture Region\nExec="{executable}" --region\n\n'
             f'[Desktop Action Fullscreen]\nName=Capture Full Desktop\nExec="{executable}" --fullscreen\n\n'
             f'[Desktop Action Preferences]\nName=Preferences\nExec="{executable}" --preferences\n\n'
             f'[Desktop Action Quit]\nName=Quit SnipChord\nExec="{executable}" --quit\n')
    apps=prefix/"share"/"applications"
    apps.mkdir(parents=True,exist_ok=True)
    (apps/"io.github.shane.snipchord.desktop").write_text(desktop)
    if args.autostart:
        autostart=Path(os.environ.get("XDG_CONFIG_HOME",Path.home()/".config"))/"autostart"
        autostart.mkdir(parents=True,exist_ok=True)
        (autostart/"io.github.shane.snipchord.desktop").write_text(
            "[Desktop Entry]\nType=Application\nName=SnipChord\n"
            f'Exec="{executable}" --daemon\nX-GNOME-Autostart-enabled=true\n')
    if args.shortcuts:
        try:
            configure_shortcuts(launcher)
        except RuntimeError as error:
            parser.error(str(error))
    print(f"Installed: {launcher}")
    if args.shortcuts:
        print("Configured GNOME keyboard shortcuts for region/fullscreen clipboard and file capture.")
    else:
        print("No keyboard shortcuts were changed.")


if __name__=="__main__":
    main()
