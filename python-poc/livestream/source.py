"""Supervised subprocess producing a concatenated JPEG stream on stdout."""

import logging
import os
import signal
import subprocess
import threading

from .jpeg import JpegSplitter

log = logging.getLogger(__name__)

READ_CHUNK = 64 * 1024


MODES = ("liveview", "stills")
JPEG_FORMATS = ("L", "cL", "M", "cM", "S", "cS")


def gphoto2_command(binary="gphoto2", mode="liveview", interval=1, image_format="L"):
    """Command line writing concatenated JPEGs to stdout until interrupted.

    ``liveview`` streams the camera's preview frames (fast, ~1056x704 on EOS).
    ``stills`` fires the shutter every ``interval`` seconds (gphoto2 only
    accepts whole seconds, minimum 1) and downloads each full JPEG; the
    capture target is forced to internal RAM so nothing accumulates on the
    card, and ``image_format`` must be a JPEG-only Canon format so RAW data
    never reaches stdout.
    """
    if mode not in MODES:
        raise ValueError(f"unknown mode {mode!r}, expected one of {MODES}")
    if mode == "liveview":
        return [binary, "--capture-movie", "--stdout"]
    if image_format not in JPEG_FORMATS:
        raise ValueError(f"image format {image_format!r} is not JPEG-only, expected one of {JPEG_FORMATS}")
    if int(interval) != interval or interval < 1:
        raise ValueError(f"interval must be a whole number of seconds >= 1, got {interval!r}")
    return [
        binary,
        "--set-config-index", "capturetarget=0",
        "--set-config", f"imageformat={image_format}",
        "--capture-image-and-download",
        "--interval", str(int(interval)),
        "--stdout",
    ]


def release_macos_ptpcamera():
    """Kill macOS's PTPCamera daemon, which claims PTP cameras and blocks libgphoto2."""
    subprocess.run(
        ["killall", "PTPCamera"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


class StreamSource:
    """Runs ``command``, splits its stdout into JPEG frames and publishes them to ``hub``.

    The process is restarted after ``restart_delay`` seconds whenever it exits,
    which covers the camera being unplugged, switched off, or liveview timing out.
    ``before_start`` runs before every (re)spawn.
    """

    def __init__(
        self,
        hub,
        command,
        before_start=None,
        restart_delay=2.0,
        splitter_factory=JpegSplitter,
    ):
        self._hub = hub
        self._command = list(command)
        self._before_start = before_start
        self._restart_delay = restart_delay
        self._splitter_factory = splitter_factory
        self._stop = threading.Event()
        self._proc = None
        self._proc_lock = threading.Lock()
        self._thread = None
        self.spawn_count = 0

    def start(self):
        self._thread = threading.Thread(target=self._run, name="stream-source", daemon=True)
        self._thread.start()

    def stop(self, timeout=5.0):
        """Ask the child to end gracefully (SIGINT lets gphoto2 leave liveview), then kill."""
        self._stop.set()
        with self._proc_lock:
            proc = self._proc
        if proc and proc.poll() is None:
            proc.send_signal(signal.SIGINT)
            try:
                proc.wait(timeout)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        if self._thread:
            self._thread.join(timeout)

    def _run(self):
        while not self._stop.is_set():
            self._run_once()
            if self._stop.wait(self._restart_delay):
                break

    def _run_once(self):
        if self._before_start:
            self._before_start()
        log.info("starting: %s", " ".join(self._command))
        try:
            proc = subprocess.Popen(
                self._command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
        except OSError as exc:
            log.error("cannot start %s: %s", self._command[0], exc)
            return
        with self._proc_lock:
            self._proc = proc
        self.spawn_count += 1
        stderr_lines = []
        drain = threading.Thread(
            target=_collect_lines, args=(proc.stderr, stderr_lines), daemon=True
        )
        drain.start()
        splitter = self._splitter_factory()
        frames = 0
        fd = proc.stdout.fileno()
        while True:
            chunk = os.read(fd, READ_CHUNK)
            if not chunk:
                break
            for frame in splitter.feed(chunk):
                self._hub.publish(frame)
                frames += 1
        code = proc.wait()
        drain.join(1.0)
        proc.stdout.close()
        proc.stderr.close()
        with self._proc_lock:
            self._proc = None
        level = logging.INFO if self._stop.is_set() else logging.WARNING
        log.log(level, "source exited with code %s after %d frames", code, frames)
        for line in stderr_lines[-10:]:
            log.log(level, "  %s", line)


def _collect_lines(stream, sink):
    for raw in stream:
        line = raw.decode("utf-8", "replace").rstrip()
        if line:
            sink.append(line)
            del sink[:-50]
