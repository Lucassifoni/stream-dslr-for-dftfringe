//! Streaming core for the livestream app: camera producer supervision, JPEG
//! frame splitting and an MJPEG HTTP server, with no external dependencies so
//! it can be built and tested independently of the Tauri shell.

pub mod hub;
pub mod jpeg;
pub mod server;
pub mod snapshot;
pub mod source;
