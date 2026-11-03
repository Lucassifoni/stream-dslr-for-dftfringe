"""Thread-safe holder of the latest frame, shared by one producer and many consumers."""

import threading


class FrameHub:
    """Keeps only the most recent frame; consumers block until a newer one arrives.

    Each published frame gets a monotonically increasing sequence number so a
    consumer can ask for "anything newer than what I already sent", which
    naturally drops frames for slow clients instead of queueing them.
    """

    def __init__(self):
        self._cond = threading.Condition()
        self._frame = None
        self._seq = 0
        self._closed = False

    def publish(self, frame):
        """Replace the current frame and wake up all waiting consumers."""
        with self._cond:
            self._frame = frame
            self._seq += 1
            self._cond.notify_all()

    def latest(self):
        """Return ``(seq, frame)`` for the current frame, ``(0, None)`` before the first."""
        with self._cond:
            return self._seq, self._frame

    def wait_newer(self, after_seq, timeout):
        """Block until a frame newer than ``after_seq`` exists.

        Returns ``(seq, frame)``, or ``None`` on timeout or once the hub is closed.
        """
        with self._cond:
            ready = self._cond.wait_for(
                lambda: self._closed or self._seq > after_seq, timeout
            )
            if not ready or self._closed:
                return None
            return self._seq, self._frame

    @property
    def closed(self):
        with self._cond:
            return self._closed

    def close(self):
        """Release every waiting consumer; subsequent waits return ``None``."""
        with self._cond:
            self._closed = True
            self._cond.notify_all()
