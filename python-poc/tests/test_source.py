import unittest

from livestream.source import gphoto2_command


class Gphoto2CommandTest(unittest.TestCase):
    def test_liveview_is_default(self):
        self.assertEqual(gphoto2_command(), ["gphoto2", "--capture-movie", "--stdout"])

    def test_stills_command(self):
        self.assertEqual(
            gphoto2_command("/opt/gp", "stills", interval=3, image_format="M"),
            [
                "/opt/gp",
                "--set-config-index", "capturetarget=0",
                "--set-config", "imageformat=M",
                "--capture-image-and-download",
                "--interval", "3",
                "--stdout",
            ],
        )

    def test_stills_rejects_raw_formats(self):
        for fmt in ("RAW", "RAW + L"):
            with self.subTest(fmt=fmt), self.assertRaises(ValueError):
                gphoto2_command(mode="stills", image_format=fmt)

    def test_stills_rejects_sub_second_or_fractional_interval(self):
        for interval in (0, -1, 1.5):
            with self.subTest(interval=interval), self.assertRaises(ValueError):
                gphoto2_command(mode="stills", interval=interval)

    def test_unknown_mode(self):
        with self.assertRaises(ValueError):
            gphoto2_command(mode="video")


if __name__ == "__main__":
    unittest.main()
