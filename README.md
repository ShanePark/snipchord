# SnipChord

SnipChord is a small, keyboard-first screenshot tool for Linux X11. It was built
to paste screenshots directly into Windows applications running under Wine.

[Ubuntu's GNOME screenshot tool](https://github.com/GNOME/gnome-shell/blob/main/js/ui/screenshot.js#L2416-L2426)
copies PNG data, which some Windows apps under Wine cannot paste as a bitmap.
SnipChord provides both PNG and BMP; [Wine maps BMP to Windows
`CF_DIB`](https://github.com/wine-mirror/wine/blob/master/dlls/winex11.drv/clipboard.c#L150-L173),
the clipboard format those apps expect.

It also provides:

- Region or full-desktop capture directly to the clipboard or a PNG file.
- Immediate selection with a border visible on both light and dark content,
  without dimming the screen.
- Select from a frozen view of the desktop, including open context menus,
  so moving content stays at the moment capture started.
- Press `Space` before dragging to pick the window under the pointer, or while
  dragging to move the selection.
- Click the thumbnail to open the captured image. Clipboard previews keep only
  the latest five cached files.

## Install

Requirements: Rust 1.85 or newer, Cargo, Python 3, and an X11 session.

```sh
cargo build --release --locked
python3 tools/install.py --shortcuts --autostart
```

The installer places the executable in `~/.local/bin/snipchord` and preserves
existing settings and unrelated shortcuts. Omit `--shortcuts --autostart` to
install without changing keyboard shortcuts or login startup.

It also installs the `snipchord` tray icon at
`~/.local/share/icons/hicolor/scalable/apps/snipchord.svg`. The tray icon is
shown when the desktop provides a StatusNotifier host; Ubuntu GNOME uses its
AppIndicator extension for this. The installer does not install or enable that
desktop extension.

## Use

The default mode is region capture to the clipboard:

```sh
~/.local/bin/snipchord --region --clipboard
```

Use `--save` for a PNG file or `--fullscreen` for the full desktop. `Esc` or
right-click cancels a selection. Set the file destination with:

```sh
~/.local/bin/snipchord --save-dir ~/Pictures/Screenshots
```

Open preferences with `~/.local/bin/snipchord --preferences`.

When installed with `--shortcuts`, the default bindings are:

| Shortcut | Action |
| --- | --- |
| `Ctrl+Alt+Shift+4` | Region to clipboard |
| `Alt+Shift+4` | Region to file |
| `Ctrl+Alt+Shift+3` | Full desktop to clipboard |
| `Alt+Shift+3` | Full desktop to file |

## Scope

SnipChord currently targets X11. Wayland support, annotation, recording, and
scrolling capture are not implemented. Captures and preview cache files stay
local; the application does not upload or analyze screenshots.
