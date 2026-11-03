# Tauri desktop app (Rust core)

Goal: a macOS app with a mode selector (liveview / stills) and a play/pause
button, serving the same MJPEG stream at `http://127.0.0.1:8765/video_feed`
for DFTFringe. Python POC moves to `python-poc/` as reference.

## Decisions

- Core ported to Rust inside the Tauri backend (no Python dependency).
- Pause stops gphoto2 (no liveview, no shutter) but keeps HTTP clients
  connected by re-sending the last frame every 2 s, so DFTFringe does not
  hit its reconnect path. Re-sent frames only happen while paused.
- While playing, idle clients are never dropped by the server (stills
  intervals can exceed any idle timeout). Note OpenCV's FFmpeg backend has a
  ~30 s read timeout, so stills interval is capped at 20 s in the UI.
- Blocking std threads + Mutex/Condvar (mirrors the POC, easy to test); no async.
- Frontend: vanilla HTML/JS with `withGlobalTauri`, no bundler.
- gphoto2 resolved from PATH then /opt/homebrew/bin, /usr/local/bin
  (GUI apps don't inherit the shell PATH).

## Layout

    app/package.json            tauri CLI only
    app/ui/                     index.html, main.js, style.css
    app/src-tauri/src/
      jpeg.rs     splitter + SOF dimension parsing (pure)
      hub.rs      latest frame, seq, paused flag, Condvar
      source.rs   command building (pure), supervisor thread, SIGINT stop
      server.rs   std TcpListener, thread per client, multipart
      lib.rs      Tauri commands: status, set_config, set_playing

## Steps

1. Port splitter + tests (incl. real-capture fixture check).
2. Hub, command builder, server + tests (server test with fake frames).
3. Supervisor with generation counter; integration test with fake producer.
4. Tauri shell + UI; manual test with the 550D and DFTFringe.

## Status (2026-11-03)

- Steps 1–3 done: core (zero deps) + shell, 34 tests green, clippy clean.
- Cargo.lock audited: 407 crates, all published before 2026-10-20
  (38 pinned back; wasm-bindgen family via temporary wasm32-only pins).
- Step 4 in progress: manual test with the 550D + DFTFringe.
- Not done: `.app` bundle (needs `@tauri-apps/cli`, pinned 2.12.3, local devDependency) — on request.
