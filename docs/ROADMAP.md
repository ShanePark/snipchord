# Experience roadmap

## 0.1 — Make the core interaction tangible

- [x] Dedicated Rust application and repository
- [x] Rust build and installation path (`cargo` release binary + user installer)
- [x] Legacy Python implementation retained locally as a behavior reference; excluded from the Rust repository
- [x] Rust resident process, selection, thumbnail, clipboard, and preferences implementation
- [x] Final integrated X11 validation for the current thumbnail, window-picker, destination, and
  output-directory contracts
- [x] Explicit clipboard/file destinations for region and full-desktop capture
- [x] Persisted output directory configuration through `--save-dir PATH`
- [x] Clickable rounded thumbnail with a four-second lifetime
- [x] Space-before-drag window picker and Space-drag region movement
- [x] Server-side frozen X11 capture without dimming, with Composite window fallback
- [x] Reproducible event-based latency experiments with keyboard contention
- [ ] User evaluation of feel, timing, borders, feedback, and KakaoTalk paste

## Next — Match the interaction, not just the appearance

- Capture toolbar: region / monitor; destination and delay
- Refined movement and resizing, Shift axis constraints, Option-style symmetric resizing
- Pointer inclusion and richer window-selection rules
- Focus restoration, accessibility, keyboard-only selection and translations
- Frame-clock measurement of cold and warm start latency on the final Composite candidate
- Profile X11 capture, buffer copies, and PNG encoding; evaluate a shared-memory path only where
  measurements justify it
- [x] Finalize Composite edge handling and install the tested release binary
- Display scale / layout changes, selection across mixed-DPI monitors
- Clipboard interoperability matrix: browser, editor, LibreOffice, Wine KakaoTalk
- Tray menu and shortcut setup with conflict detection and rollback

## Platform integration

- Wayland: investigate portal and compositor interfaces; never promise an X11-like
  global screenshot overlay where the desktop security model forbids it
- Native packages and CI on supported distributions
- Signed releases / updates only after the core UX is validated
