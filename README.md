# livestream

Streams a gphoto2-supported camera (tested: Canon EOS 550D) as MJPEG over HTTP
at `http://127.0.0.1:8765/video_feed`, which DFTFringe's Live View opens as a URL.

- `app/` — macOS desktop app (Tauri 2, Rust). Mode selector (liveview / stills),
  play/pause, live preview, status. **This is the main tool.**
- `python-poc/` — original command-line proof of concept, same stream format.

gphoto2 is not bundled: install it with `brew install gphoto2`.

## App

    cd app
    cargo test                          # core + shell tests
    cargo run -p livestream             # run the app (dev build)

`app/core` has no dependencies and builds/tests offline. Dependency versions
are pinned exactly in `app/src-tauri/Cargo.toml`; `app/Cargo.lock` is the
audited lock (see `docs_and_plans/guides/architecture.md`).

In DFTFringe: Live view → Settings → paste the URL, press Enter.

## Modes

- **Liveview**: preview frames, 1056×704, 15–30 fps, no shutter wear.
- **Stills**: real captures (L = 5184×3456), ~1.1 s/frame at best.
  **Every frame is a shutter actuation** (550D rated ~100k). Use manual focus
  and manual exposure. Interval 1–20 s (OpenCV's FFmpeg reader times out ~30 s).
- **Pause** stops the camera; DFTFringe stays connected (the last frame is
  re-sent every 2 s). Pause DFTFringe's analysis loop too, or it re-analyses it.

## Python POC

    cd python-poc && python3 -m livestream [--mode stills] && python3 -m unittest
