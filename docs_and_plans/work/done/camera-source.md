# Camera sources: UVC / V4L2, AVFoundation, GenICam (Aravis)

Goal: stream more than gphoto2 DSLRs through the same MJPEG URL, and expose
camera settings in the app since DFTFringe disables its own controls for URL
sources.

Motivation: Linux users report cameras that work in other programs but not in
DFTFringe. Two cases:

- UVC webcams that OpenCV's V4L2 backend (`cv::VideoCapture(int)`) handles
  badly; going through ffmpeg and the URL path sidesteps that backend.
- USB3 Vision / GigE Vision industrial cameras (FLIR / Point Grey Blackfly,
  Basler), which are not UVC at all: vendor viewers and Aravis see them, V4L2
  does not.

## Decisions

- All producers stay external system packages (gphoto2, ffmpeg, gst-launch +
  Aravis); the app stays dependency-light and shows install commands.
- UVC: ffmpeg, MJPG copied untouched, other formats re-encoded. V4L2 discovery
  and live controls through raw ioctls declared in-crate (no new crates).
- macOS: same camera source through AVFoundation (stderr parsing), no
  controls. Pending tests on a real camera.
- GenICam: GStreamer `aravissrc` pipeline; exposure / gain / binning / pixel
  format / frame rate applied at start (restart on change, as USB3 Vision
  control is exclusive). Ranges from `arv-tool-0.8 control`.
- Not done: Bayer formats (need gst-plugins-bad), free-form GenICam features
  (`features=` failed on the simulator), ROI selection.

## Steps

1. Split `source.rs`: `tools.rs`, `dslr.rs`, `camera.rs`; `CaptureConfig` enum. ✔
2. `v4l2.rs`: ioctl layer, discovery, controls + fake-device tests. ✔
3. `avfoundation.rs`: stderr parsers + tests. ✔ (real-camera test pending)
4. `aravis.rs`: pipeline args, `arv-tool` parsers, discovery + tests. ✔
5. Shell commands and DTOs + tests; UI source selector, pickers, controls
   panel; pure form logic in `ui/model.js` with `node --test`. ✔
6. Real-device checks. ✔ UVC camera on the LAN box (discovery, controls,
   MJPG / YUYV / H264 streaming); ✔ Aravis simulator (discovery, Mono8 /
   Mono16, manual exposure / gain / binning / frame rate); ✔ UI in headless
   Chromium with a mocked backend. Pending: a real USB3 Vision camera, the
   AppImage UI on a Linux desktop, AVFoundation on macOS.

## Status (2026-11-21)

Released as 0.2.0 for testing with a real GenICam camera.
