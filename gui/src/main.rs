//! The native binary of the kage client. The application itself lives
//! in the library: `kage_desktop::launch` for the desktop and
//! `kage_desktop::web` for the browser build, which share the shell.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    kage_desktop::launch::run();
}

#[cfg(target_arch = "wasm32")]
fn main() {}
