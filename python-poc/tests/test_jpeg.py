import unittest

from livestream.jpeg import JpegSplitter


def segment(marker, payload):
    return bytes([0xFF, marker]) + (len(payload) + 2).to_bytes(2, "big") + payload


def fake_jpeg(entropy=b"\x12\x34\xff\x00\x56", app=b"JFIF\x00", extra_segments=b""):
    """Structurally valid JPEG: SOI, APP0, extra, SOS header, entropy data, EOI."""
    return (
        b"\xff\xd8"
        + segment(0xE0, app)
        + extra_segments
        + segment(0xDA, b"\x01\x01\x00\x00\x3f\x00")
        + entropy
        + b"\xff\xd9"
    )


def feed_in_chunks(splitter, data, size):
    frames = []
    for i in range(0, len(data), size):
        frames += splitter.feed(data[i : i + size])
    return frames


class JpegSplitterTest(unittest.TestCase):
    def test_single_frame(self):
        frame = fake_jpeg()
        self.assertEqual(JpegSplitter().feed(frame), [frame])

    def test_concatenated_frames(self):
        frames = [fake_jpeg(entropy=bytes([i]) * 10) for i in range(5)]
        self.assertEqual(JpegSplitter().feed(b"".join(frames)), frames)

    def test_every_chunk_boundary(self):
        frames = [fake_jpeg(), fake_jpeg(entropy=b"\xff\xd0\x01\xff\x00")]
        data = b"".join(frames)
        for size in range(1, len(data) + 1):
            with self.subTest(size=size):
                self.assertEqual(feed_in_chunks(JpegSplitter(), data, size), frames)

    def test_garbage_between_and_around_frames_is_discarded(self):
        a, b = fake_jpeg(), fake_jpeg(entropy=b"\x99")
        data = b"noise\xff" + a + b"\x00\x01\xff\xfe" + b + b"trailing"
        self.assertEqual(JpegSplitter().feed(data), [a, b])

    def test_eoi_bytes_inside_header_segment_do_not_end_frame(self):
        thumbnail = fake_jpeg(entropy=b"\x01")
        frame = fake_jpeg(extra_segments=segment(0xE1, b"Exif\x00\x00" + thumbnail))
        self.assertEqual(JpegSplitter().feed(frame), [frame])

    def test_stuffed_and_restart_markers_in_entropy_data(self):
        frame = fake_jpeg(entropy=b"\xff\x00\xff\xd3\xaa\xff\xff\x00\xbb")
        self.assertEqual(JpegSplitter().feed(frame), [frame])

    def test_progressive_multiple_scans(self):
        sos = segment(0xDA, b"\x01\x01\x00\x00\x3f\x00")
        frame = b"\xff\xd8" + sos + b"\x01\x02" + segment(0xC4, b"\x00" * 17) + sos + b"\x03" + b"\xff\xd9"
        self.assertEqual(JpegSplitter().feed(frame), [frame])

    def test_incomplete_frame_is_held_until_completed(self):
        frame = fake_jpeg()
        s = JpegSplitter()
        self.assertEqual(s.feed(frame[:-1]), [])
        self.assertEqual(s.feed(frame[-1:]), [frame])

    def test_malformed_frame_resyncs_on_next_soi(self):
        good = fake_jpeg()
        broken = b"\xff\xd8\x00\x00garbage"
        s = JpegSplitter()
        self.assertEqual(s.feed(broken + good), [good])
        self.assertEqual(s.dropped, 1)

    def test_oversized_frame_is_dropped(self):
        s = JpegSplitter(max_frame_bytes=64)
        s.feed(b"\xff\xd8" + segment(0xDA, b"\x00" * 4) + b"\x01" * 100)
        self.assertEqual(s.dropped, 1)
        good = fake_jpeg()
        self.assertEqual(s.feed(good), [good])


if __name__ == "__main__":
    unittest.main()
