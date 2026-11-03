"""HTTP endpoints serving the hub's frames as MJPEG (multipart/x-mixed-replace).

This is the format OpenCV's FFmpeg backend (``cv::VideoCapture("http://...")``)
and browsers both understand.
"""

import logging
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

log = logging.getLogger(__name__)

BOUNDARY = "frame"
STREAM_PATH = "/video_feed"
SNAPSHOT_PATH = "/snapshot.jpg"

_INDEX = (
    "<!doctype html><title>livestream</title>"
    "<body style='margin:0;background:#111'>"
    f"<img src='{STREAM_PATH}' style='max-width:100vw;max-height:100vh;display:block;margin:auto'>"
)


def multipart_part(frame):
    """Encode one JPEG as a multipart/x-mixed-replace part."""
    header = (
        f"--{BOUNDARY}\r\n"
        "Content-Type: image/jpeg\r\n"
        f"Content-Length: {len(frame)}\r\n\r\n"
    ).encode("ascii")
    return header + frame + b"\r\n"


def make_handler(hub, frame_timeout=10.0):
    """Build a request handler class bound to ``hub``.

    A stream client that sees no new frame for ``frame_timeout`` seconds is
    disconnected, so the consumer notices and reconnects rather than hanging.
    """

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def do_GET(self):
            path = self.path.split("?", 1)[0]
            routes = {
                STREAM_PATH: self._stream,
                SNAPSHOT_PATH: self._snapshot,
                "/": self._index,
            }
            routes.get(path, self._not_found)()

        def _index(self):
            body = _INDEX.encode()
            self._send_headers(200, "text/html; charset=utf-8", len(body))
            self.wfile.write(body)

        def _not_found(self):
            self.send_error(404)

        def _snapshot(self):
            seq, frame = hub.latest()
            if frame is None:
                result = hub.wait_newer(seq, frame_timeout)
                frame = result[1] if result else None
            if frame is None:
                self.send_error(503, "no frame available yet")
                return
            self._send_headers(200, "image/jpeg", len(frame))
            self.wfile.write(frame)

        def _stream(self):
            self.send_response(200)
            self.send_header("Content-Type", f"multipart/x-mixed-replace; boundary={BOUNDARY}")
            self.send_header("Cache-Control", "no-cache, no-store")
            self.send_header("Pragma", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()
            seq, frame = hub.latest()
            try:
                if frame is not None:
                    self.wfile.write(multipart_part(frame))
                while True:
                    result = hub.wait_newer(seq, frame_timeout)
                    if result is None:
                        return
                    seq, frame = result
                    self.wfile.write(multipart_part(frame))
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                return

        def _send_headers(self, code, content_type, length):
            self.send_response(code)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(length))
            self.send_header("Cache-Control", "no-cache, no-store")
            self.end_headers()

        def log_message(self, fmt, *args):
            log.info("%s %s", self.address_string(), fmt % args)

    return Handler


def make_server(hub, host, port, frame_timeout=10.0):
    """Create a threaded HTTP server (call ``serve_forever`` on it)."""
    server = ThreadingHTTPServer((host, port), make_handler(hub, frame_timeout))
    server.daemon_threads = True
    return server
