# Architecture & camera notes

## Pipeline

    gphoto2 ... --stdout               (concatenated JPEGs, no container)
      → Supervisor    app/core/src/source.rs   process supervisor, play/pause, mode, respawn
      → JpegSplitter  app/core/src/jpeg.rs     pure, marker-aware frame splitter (+ SOF dimensions)
      → FrameHub      app/core/src/hub.rs      latest frame + seq, paused flag, Condvar
      → HTTP server   app/core/src/server.rs   std::net, thread per client, multipart on /video_feed
      → Tauri shell   app/src-tauri/src/lib.rs commands get_status / set_playing / set_config
      → UI            app/ui/                  vanilla JS, polls status every 500 ms

The Python POC in `python-poc/` has the same structure (no pause/keepalive).
Frames are passed through untouched (no re-encode). Slow clients skip frames
rather than queue them.

## Play / pause semantics

- The app starts paused. Pause/config change bumps a generation counter and
  sends SIGINT to gphoto2 (clean liveview exit), SIGKILL after 5 s.
- While paused, stream clients get the last frame re-sent every 2 s so
  DFTFringe keeps its connection. No re-sends while playing.
- While playing, idle clients are never dropped (stills intervals can be long).
- Unexpected producer exits → state `retrying`, respawn after 2 s; the UI shows
  the exit code and last stderr lines.
- On app exit the supervisor stops gphoto2 before quitting.

## Dependencies policy

- `app/core`: zero dependencies; signals go through `/bin/kill`.
- `app/src-tauri`: tauri, tauri-build, serde, serde_json pinned with `=`; only
  releases at least two weeks old. `Cargo.lock` was audited against crates.io
  publish dates; anything published within two weeks of resolution was pinned
  back with `cargo update --precise`.
- No npm / Tauri CLI needed for dev (`cargo run`); icons are generated with
  ffmpeg + `iconutil`. The CLI is only needed to produce a `.app` bundle.

## Why a URL and not a camera index

DFTFringe (`videostreamworker.cpp`) opens an integer source with
`cv::VideoCapture(int)` (AVFoundation on macOS) and anything else with
`cv::VideoCapture(string)`, which goes through the bundled FFmpeg backend.
The 550D is a PTP device, not UVC, so it never appears as a camera index.
Exposing it as an index would need a virtual camera (CoreMediaIO extension
or OBS virtualcam) — more moving parts for no gain.

Consequence: DFTFringe's exposure/gain/resolution controls are disabled for
URL sources. Set exposure on the camera (M mode, Exposure Simulation enabled
in the liveview menu so the preview reflects the exposure).

## Camera facts (550D, measured)

- Liveview frames: 1056×704 JPEG, ~140 KB each, ~15–30 fps over USB.
- macOS `PTPCamera` daemon grabs the camera; it is killed before every spawn
  (`--keep-ptpcamera` disables that).
- gphoto2 exits liveview cleanly on SIGINT; shutdown sends SIGINT, then SIGKILL after 5 s.

## Stills mode

`--mode stills` runs, as a single long-lived process:

    gphoto2 --set-config-index capturetarget=0 --set-config imageformat=L \
            --capture-image-and-download --interval N --stdout

- `capturetarget=0` (internal RAM) keeps files off the SD card. Set by index
  because labels are localised (camera reports French labels here).
- Only JPEG `imageformat` choices are accepted; RAW/RAW+L would put CR2 data on stdout.
- Measured on 550D, USB 2.0: L (5184×3456, ~6 MB) ≈ 1.1 s/frame at `--interval 1`;
  M (3456×2304, ~2.2 MB) ≈ 1.1 s/frame. Capture+transfer dominates below ~1 s.
- FFmpeg (and so DFTFringe) opens the stream in ~1 s because the server sends
  the latest frame immediately on connect.
- Stills carry EXIF thumbnails (embedded `FFD8…FFD9`), which is why the splitter
  must be marker-aware.
- Each frame is a shutter actuation; at 1 fps that is ~3,600/hour.

## Splitter details

Naive splitting on `FFD9` breaks on frames carrying EXIF thumbnails. The
splitter walks marker segments by length and only searches for EOI inside
entropy-coded data, where `FF` is followed by `00` (stuffing) or `D0–D7`
(restarts). Malformed/oversized frames are dropped and it resyncs on the next SOI.
