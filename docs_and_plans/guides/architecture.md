# Architecture & camera notes

## Pipeline

    producer --stdout                  (concatenated JPEGs, no container)
      gphoto2       app/core/src/dslr.rs     DSLR liveview / stills
      ffmpeg        app/core/src/camera.rs   UVC: V4L2 (Linux), AVFoundation (macOS)
      gst-launch    app/core/src/aravis.rs   GenICam: aravissrc ! videoconvert ! jpegenc ! fdsink
      → Supervisor    app/core/src/source.rs   process supervisor, play/pause, config, respawn
      → JpegSplitter  app/core/src/jpeg.rs     pure, marker-aware frame splitter (+ SOF dimensions)
      → FrameHub      app/core/src/hub.rs      latest frame + seq, paused flag, Condvar
      → HTTP server   app/core/src/server.rs   std::net, thread per client, multipart on /video_feed
      → Tauri shell   app/src-tauri/src/lib.rs status, config, discovery, V4L2 controls
      → UI            app/ui/                  vanilla JS (pure form logic in model.js), polls status every 500 ms

Side channels, outside the producer:

- `v4l2.rs`: device discovery and controls through raw ioctls (see below).
- `avfoundation.rs`: discovery by parsing ffmpeg stderr.
- `aravis.rs`: discovery by parsing `arv-tool-0.8` output.
- `tools.rs`: locating binaries (PATH, then Homebrew / MacPorts / system dirs).

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

## Camera source (UVC / V4L2, AVFoundation)

- Producer: `ffmpeg -f v4l2 -input_format <fmt> -video_size WxH -framerate N/D -i /dev/videoN`
  then `-c:v copy` for MJPG (untouched frames) or `-c:v mjpeg -q:v 2` for
  anything else, `-f mjpeg -` to stdout. ffmpeg exits cleanly on SIGINT.
- Frame rates are kept as exact V4L2 fractions (`333333/10000000`) so ffmpeg
  requests the interval the driver listed.
- `mjpeg2jpeg` is not used: it only accepts AVI1 frames. UVC MJPEG frames
  tested so far carry their Huffman tables.
- The fps shown in the first second is inflated: ffmpeg probes a few frames
  then flushes them at once. It settles on the real rate.

### V4L2 without dependencies

`v4l2.rs` declares `ioctl(2)` itself (std already links libc) and handles the
`videodev2.h` structs as byte buffers at fixed UAPI offsets. Only the ioctl
call is unsafe; enumeration, deduplication and control logic run against an
`Ioctl` trait and are tested with a fake UVC device. Request numbers are
checked against the kernel header values in tests.

- Capture nodes: `device_caps & V4L2_CAP_VIDEO_CAPTURE`; UVC also exposes
  metadata-only nodes (`/dev/video1`, `/dev/video3` for the test camera).
- Firmwares list sizes and intervals twice; discovery dedupes.
- Menus can have gaps (`auto_exposure`: 1 = manual, 3 = aperture priority).
- Controls are set with `VIDIOC_S_CTRL` on a second open while ffmpeg streams;
  V4L2 only makes streaming exclusive. Switching an auto control flips other
  controls' `INACTIVE` flag, so the UI re-reads the list after each change.

### Test camera (LAN box, `uvcvideo`)

`/dev/video0`: MJPG and YUYV up to 1920×1080 (YUYV 1080p at 5 fps only);
`/dev/video2`: H264 only. Controls: brightness, contrast, saturation, hue,
gamma, gain, white balance auto/temperature, sharpness, backlight, power
line, auto exposure, exposure time, dynamic framerate. Measured: MJPG and
YUYV 1280×720 and H264 stream at the requested size.

## GenICam source (Aravis)

- Why: USB3 Vision / GigE Vision cameras are not UVC, so OpenCV's V4L2 backend
  (DFTFringe's camera-index path) never sees them on Linux, even though vendor
  viewers and Aravis do.
- Producer: `gst-launch-1.0 -q -e aravissrc camera-name="<id>" [exposure-auto=off
  exposure=µs] [gain-auto=off gain=x] [h-binning=n v-binning=n] !
  video/x-raw,format=GRAY8[,framerate=N/D] ! videoconvert ! jpegenc quality=95 !
  fdsink fd=1`. `-q` keeps progress messages off stdout; `-e` turns SIGINT
  into EOS for a clean stop.
- Exposure and gain go through `aravissrc` properties: Aravis maps them to the
  vendor's feature names (`ExposureTime` vs legacy `ExposureTimeAbs`, `Gain`
  vs `GainRaw`). `arv-tool-0.8 control` provides the ranges shown in the UI.
- USB3 Vision gives one process control of the camera: settings apply at
  pipeline start, and discovery fails while streaming (refresh is disabled
  while playing).
- The `features=` property of `aravissrc` failed on the simulator ("Controller
  privilege required"), so free-form GenICam features are not exposed.
- Bayer needs `bayer2rgb` (gst-plugins-bad); not offered.
- `libaravis-0.8-0` ships no udev rules; non-root USB access relies on the
  vendor SDK's rules.
- Simulator: `arv-fake-gv-camera-0.8 -i 127.0.0.1` (package `aravis-tools`).
  It only streams when loopback is the only interface (`docker run --network
  none`); otherwise discovery goes through the bridge address and the stream
  never arrives. It ignores binning and keeps the last frame rate set.
