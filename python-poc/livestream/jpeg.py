"""Incremental splitter turning a concatenated JPEG byte stream into frames.

`gphoto2 --capture-movie --stdout` emits liveview frames back to back with no
container. Splitting naively on ``FFD9`` breaks on frames carrying embedded
thumbnails (EXIF APP1), so this walks JPEG marker segments and only scans for
the end-of-image marker inside entropy-coded data, where ``FF`` is always
followed by ``00`` (stuffing) or ``D0``-``D7`` (restart markers).
"""

SOI = b"\xff\xd8"
_EOI = 0xD9
_SOS = 0xDA
_STANDALONE = frozenset([0x01, *range(0xD0, 0xD8)])

DEFAULT_MAX_FRAME_BYTES = 32 * 1024 * 1024

_RESYNC = object()


class JpegSplitter:
    """Feed arbitrary chunks, get back complete JPEG frames as ``bytes``.

    Bytes outside of a frame are discarded. A frame growing past
    ``max_frame_bytes`` or with a malformed marker structure is dropped and
    the splitter resynchronises on the next start-of-image marker.
    """

    def __init__(self, max_frame_bytes=DEFAULT_MAX_FRAME_BYTES):
        self._buf = bytearray()
        self._cursor = 0
        self._in_entropy = False
        self._max = max_frame_bytes
        self.dropped = 0

    def feed(self, chunk):
        """Append ``chunk`` and return the list of frames completed by it."""
        self._buf += chunk
        frames = []
        while self._sync_to_soi():
            frame = self._advance()
            if frame is None:
                break
            if frame is _RESYNC:
                self._drop_current()
                continue
            frames.append(frame)
        if len(self._buf) > self._max:
            self._drop_current()
        return frames

    def _sync_to_soi(self):
        if self._cursor:
            return True
        start = self._buf.find(SOI)
        if start < 0:
            keep = 1 if self._buf.endswith(b"\xff") else 0
            del self._buf[: len(self._buf) - keep]
            return False
        del self._buf[:start]
        self._cursor = 2
        self._in_entropy = False
        return True

    def _advance(self):
        buf = self._buf
        while True:
            if self._in_entropy:
                if not self._skip_entropy():
                    return None
                continue
            i = self._cursor
            if i + 2 > len(buf):
                return None
            if buf[i] != 0xFF:
                return _RESYNC
            marker = buf[i + 1]
            if marker == 0xFF:
                self._cursor = i + 1
                continue
            if marker == _EOI:
                return self._emit(i + 2)
            if marker in _STANDALONE:
                self._cursor = i + 2
                continue
            if i + 4 > len(buf):
                return None
            length = (buf[i + 2] << 8) | buf[i + 3]
            if length < 2:
                return _RESYNC
            end = i + 2 + length
            if end > len(buf):
                return None
            self._cursor = end
            self._in_entropy = marker == _SOS

    def _skip_entropy(self):
        buf = self._buf
        i = self._cursor
        while True:
            j = buf.find(b"\xff", i)
            if j < 0 or j + 1 >= len(buf):
                self._cursor = len(buf) - 1 if j >= 0 else len(buf)
                return False
            nxt = buf[j + 1]
            if nxt == 0x00 or 0xD0 <= nxt <= 0xD7 or nxt == 0xFF:
                i = j + 1
                continue
            self._cursor = j
            self._in_entropy = False
            return True

    def _emit(self, end):
        frame = bytes(self._buf[:end])
        del self._buf[:end]
        self._cursor = 0
        self._in_entropy = False
        return frame

    def _drop_current(self):
        self.dropped += 1
        del self._buf[:2]
        self._cursor = 0
        self._in_entropy = False
