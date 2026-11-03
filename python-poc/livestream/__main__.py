"""Command line entry point: ``python3 -m livestream``."""

import argparse
import logging
import shlex
import signal
import sys
import threading

from .hub import FrameHub
from .server import STREAM_PATH, make_server
from .source import JPEG_FORMATS, MODES, StreamSource, gphoto2_command, release_macos_ptpcamera

DEFAULT_PORT = 8765


def parse_args(argv):
    p = argparse.ArgumentParser(
        prog="livestream",
        description="Serve a gphoto2 camera liveview as an MJPEG HTTP stream (e.g. for DFTFringe).",
    )
    p.add_argument("--host", default="127.0.0.1", help="bind address (0.0.0.0 to expose on the LAN)")
    p.add_argument("--port", type=int, default=DEFAULT_PORT)
    p.add_argument("--gphoto2", default="gphoto2", help="gphoto2 executable")
    p.add_argument(
        "--mode",
        choices=MODES,
        default="liveview",
        help="liveview: fast preview frames; stills: full-size captures (fires the shutter every frame)",
    )
    p.add_argument("--interval", type=int, default=1, help="stills mode: seconds between captures (>= 1)")
    p.add_argument(
        "--image-format",
        choices=JPEG_FORMATS,
        default="L",
        help="stills mode: Canon JPEG size/quality (L = full resolution, c = compressed)",
    )
    p.add_argument(
        "--source-cmd",
        help="override the producer command (any program writing concatenated JPEGs to stdout)",
    )
    p.add_argument("--restart-delay", type=float, default=2.0, help="seconds before respawning the source")
    p.add_argument("--frame-timeout", type=float, default=10.0, help="drop stream clients after this many idle seconds")
    p.add_argument("--keep-ptpcamera", action="store_true", help="do not kill macOS PTPCamera before starting")
    p.add_argument("-v", "--verbose", action="store_true")
    return p.parse_args(argv)


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    logging.basicConfig(
        level=logging.INFO if args.verbose else logging.WARNING,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    command = (
        shlex.split(args.source_cmd)
        if args.source_cmd
        else gphoto2_command(args.gphoto2, args.mode, args.interval, args.image_format)
    )
    before = None if (args.keep_ptpcamera or args.source_cmd) else release_macos_ptpcamera

    hub = FrameHub()
    source = StreamSource(hub, command, before_start=before, restart_delay=args.restart_delay)
    server = make_server(hub, args.host, args.port, args.frame_timeout)

    shown_host = "127.0.0.1" if args.host in ("0.0.0.0", "") else args.host
    print(f"Streaming on http://{shown_host}:{server.server_port}{STREAM_PATH}", flush=True)
    if args.mode == "stills" and not args.source_cmd:
        print(f"Stills mode: one {args.image_format} capture every ~{args.interval}s+ (each one is a shutter actuation).", flush=True)
    print("Paste that URL in DFTFringe > Live view > Settings. Ctrl-C to stop.", flush=True)

    stopping = threading.Event()

    def shutdown(*_):
        if stopping.is_set():
            return
        stopping.set()
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGINT, shutdown)
    signal.signal(signal.SIGTERM, shutdown)

    source.start()
    try:
        server.serve_forever()
    finally:
        hub.close()
        source.stop()
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
