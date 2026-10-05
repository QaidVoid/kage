//! The kage client: one window over one engine link.
//!
//! The shell ([`app`]) renders three resizable panels over the
//! [`store`], which fronts `kage-client`, and pumps frames through a
//! [`transport`]. One crate builds two ways:
//!
//! - Desktop: [`launch`] parses the flags and opens a native window
//!   over stdio, WebSocket or the replay recording.
//! - Browser: [`web`] compiles for wasm32-unknown-unknown and opens
//!   the same shell over a web-sys WebSocket. GPUI renders through
//!   `gpui-pre-web`, WebGPU with a WebGL2 canvas fallback.

pub mod app;
pub mod assets;
pub mod clock;
pub mod gate;
#[cfg(not(target_arch = "wasm32"))]
pub mod icon;
pub mod prefs;
#[doc(hidden)]
pub mod store;
pub mod theme;
pub mod themes;
pub mod timing;
pub mod transport;
pub mod views;

#[cfg(not(target_arch = "wasm32"))]
pub mod launch;

#[cfg(target_arch = "wasm32")]
pub mod web;
