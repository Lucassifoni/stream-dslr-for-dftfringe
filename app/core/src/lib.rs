//! Streaming core for the livestream app: producer supervision (gphoto2 for
//! DSLRs, ffmpeg for UVC / V4L2 / AVFoundation cameras, GStreamer + Aravis for
//! GenICam cameras), V4L2 controls, JPEG frame splitting and an MJPEG HTTP
//! server, with no external dependencies so it can be built and tested
//! independently of the Tauri shell.

pub mod aravis;
pub mod avfoundation;
pub mod camera;
pub mod dslr;
pub mod hub;
pub mod jpeg;
pub mod server;
pub mod snapshot;
pub mod source;
pub mod tools;
pub mod v4l2;
