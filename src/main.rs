//! SnipChord's small native X11 executable.

mod app;
mod clipboard;
mod geometry;
mod hotkeys;
mod image;
mod server_capture;
mod settings;
mod shortcuts;
mod storage;
mod tray;
mod ui;
mod window_capture;
mod x11;

use std::env;

fn main() {
    let args: Vec<_> = env::args_os().collect();
    std::process::exit(app::run(&args));
}
