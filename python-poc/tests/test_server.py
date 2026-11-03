import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
from pathlib import Path

from livestream.hub import FrameHub
from livestream.server import BOUNDARY, make_server, multipart_part
from livestream.source import StreamSource


class Running:
    """Context manager running a server on an ephemeral port."""

    def __init__(self, hub, frame_timeout=2.0):
        self.server = make_server(hub, "127.0.0.1", 0, frame_timeout)
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def __enter__(self):
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *exc):
        self.server.shutdown()
        self.server.server_close()


class FrameHubTest(unittest.TestCase):
    def test_wait_newer_returns_latest_and_times_out(self):
        hub = FrameHub()
        self.assertEqual(hub.latest(), (0, None))
        self.assertIsNone(hub.wait_newer(0, 0.01))
        hub.publish(b"a")
        hub.publish(b"b")
        self.assertEqual(hub.wait_newer(0, 0.01), (2, b"b"))
        self.assertIsNone(hub.wait_newer(2, 0.01))

    def test_close_releases_waiters(self):
        hub = FrameHub()
        threading.Timer(0.05, hub.close).start()
        self.assertIsNone(hub.wait_newer(0, 5))


class ServerTest(unittest.TestCase):
    def test_multipart_part_format(self):
        part = multipart_part(b"JPEG")
        self.assertEqual(
            part,
            f"--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\nJPEG\r\n".encode(),
        )

    def test_snapshot_and_stream(self):
        hub = FrameHub()
        hub.publish(b"\xff\xd8one\xff\xd9")
        with Running(hub) as srv:
            with urllib.request.urlopen(srv.url + "/snapshot.jpg", timeout=5) as r:
                self.assertEqual(r.headers["Content-Type"], "image/jpeg")
                self.assertEqual(r.read(), b"\xff\xd8one\xff\xd9")
            with urllib.request.urlopen(srv.url + "/video_feed", timeout=5) as r:
                self.assertIn("multipart/x-mixed-replace", r.headers["Content-Type"])
                first = r.read(len(multipart_part(b"\xff\xd8one\xff\xd9")))
                self.assertEqual(first, multipart_part(b"\xff\xd8one\xff\xd9"))
                hub.publish(b"\xff\xd8two\xff\xd9")
                second = r.read(len(multipart_part(b"\xff\xd8two\xff\xd9")))
                self.assertEqual(second, multipart_part(b"\xff\xd8two\xff\xd9"))

    def test_snapshot_without_frame_is_503(self):
        with Running(FrameHub(), frame_timeout=0.1) as srv:
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(srv.url + "/snapshot.jpg", timeout=5)
            self.assertEqual(ctx.exception.code, 503)

    def test_unknown_path_is_404(self):
        with Running(FrameHub()) as srv:
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(srv.url + "/nope", timeout=5)
            self.assertEqual(ctx.exception.code, 404)


@unittest.skipUnless(shutil.which("ffmpeg"), "ffmpeg not installed")
class EndToEndTest(unittest.TestCase):
    """Source subprocess -> splitter -> hub -> HTTP -> ffmpeg (same demuxer OpenCV uses)."""

    def test_ffmpeg_reads_stream_and_source_restarts(self):
        tmp = Path(tempfile.mkdtemp())
        clip = tmp / "clip.mjpg"
        subprocess.run(
            ["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10",
             "-frames:v", "5", "-f", "mjpeg", str(clip)],
            check=True,
        )
        producer = [sys.executable, "-c",
                    "import sys,time\n"
                    f"d=open({str(clip)!r},'rb').read()\n"
                    "for i in range(0,len(d),997):\n"
                    "    sys.stdout.buffer.write(d[i:i+997]); sys.stdout.flush(); time.sleep(0.005)\n"]
        hub = FrameHub()
        source = StreamSource(hub, producer, restart_delay=0.05)
        source.start()
        try:
            with Running(hub) as srv:
                out = subprocess.run(
                    ["ffprobe", "-v", "error", "-count_frames", "-read_intervals", "%+#12",
                     "-show_entries", "stream=codec_name,width,height,nb_read_frames",
                     "-of", "default=nw=1", srv.url + "/video_feed"],
                    capture_output=True, text=True, timeout=30,
                )
        finally:
            source.stop()
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("codec_name=mjpeg", out.stdout)
        self.assertIn("width=320", out.stdout)
        self.assertIn("height=240", out.stdout)
        self.assertGreaterEqual(source.spawn_count, 2)


if __name__ == "__main__":
    unittest.main()
