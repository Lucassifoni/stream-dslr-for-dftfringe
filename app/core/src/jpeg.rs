//! Incremental splitter turning a concatenated JPEG byte stream into frames.
//!
//! `gphoto2 --capture-movie --stdout` and `--capture-image-and-download --stdout`
//! emit JPEGs back to back with no container. Splitting naively on `FFD9`
//! breaks on stills carrying EXIF thumbnails, so the splitter walks marker
//! segments by length and only searches for end-of-image inside entropy-coded
//! data, where `FF` is always followed by `00` (stuffing), `D0..=D7`
//! (restart markers) or more `FF` fill bytes.

const SOI: [u8; 2] = [0xFF, 0xD8];
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;

/// Frames growing beyond this are considered corrupt and dropped.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

fn is_standalone(marker: u8) -> bool {
    marker == 0x01 || (0xD0..=0xD7).contains(&marker)
}

fn is_entropy_continuation(next: u8) -> bool {
    next == 0x00 || next == 0xFF || (0xD0..=0xD7).contains(&next)
}

fn is_start_of_frame(marker: u8) -> bool {
    (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
}

enum Step {
    Incomplete,
    Frame(Vec<u8>),
    Resync,
}

/// Feed arbitrary chunks, get back complete JPEG frames.
///
/// Bytes outside of a frame are discarded. A frame with a malformed marker
/// structure, or growing past `max_frame_bytes`, is dropped and the splitter
/// resynchronises on the next start-of-image marker.
pub struct JpegSplitter {
    buf: Vec<u8>,
    cursor: usize,
    in_entropy: bool,
    max_frame_bytes: usize,
    dropped: u64,
}

impl Default for JpegSplitter {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }
}

impl JpegSplitter {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            in_entropy: false,
            max_frame_bytes,
            dropped: 0,
        }
    }

    /// Number of frames discarded as malformed or oversized so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Append `chunk` and return the frames it completed, in stream order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while self.sync_to_soi() {
            match self.advance() {
                Step::Incomplete => break,
                Step::Resync => self.drop_current(),
                Step::Frame(frame) => frames.push(frame),
            }
        }
        if self.buf.len() > self.max_frame_bytes {
            self.drop_current();
        }
        frames
    }

    fn sync_to_soi(&mut self) -> bool {
        if self.cursor != 0 {
            return true;
        }
        match self.buf.windows(2).position(|w| w == SOI) {
            Some(start) => {
                self.buf.drain(..start);
                self.cursor = 2;
                self.in_entropy = false;
                true
            }
            None => {
                let keep = usize::from(self.buf.last() == Some(&0xFF));
                let discard = self.buf.len() - keep;
                self.buf.drain(..discard);
                false
            }
        }
    }

    fn advance(&mut self) -> Step {
        loop {
            if self.in_entropy {
                if !self.skip_entropy() {
                    return Step::Incomplete;
                }
                continue;
            }
            let i = self.cursor;
            let len = self.buf.len();
            if i + 2 > len {
                return Step::Incomplete;
            }
            if self.buf[i] != 0xFF {
                return Step::Resync;
            }
            let marker = self.buf[i + 1];
            if marker == 0xFF {
                self.cursor = i + 1;
                continue;
            }
            if marker == EOI {
                return Step::Frame(self.emit(i + 2));
            }
            if is_standalone(marker) {
                self.cursor = i + 2;
                continue;
            }
            if i + 4 > len {
                return Step::Incomplete;
            }
            let length = usize::from(u16::from_be_bytes([self.buf[i + 2], self.buf[i + 3]]));
            if length < 2 {
                return Step::Resync;
            }
            let end = i + 2 + length;
            if end > len {
                return Step::Incomplete;
            }
            self.cursor = end;
            self.in_entropy = marker == SOS;
        }
    }

    fn skip_entropy(&mut self) -> bool {
        let mut from = self.cursor;
        loop {
            let found = self.buf[from..].iter().position(|&b| b == 0xFF).map(|p| p + from);
            match found {
                None => {
                    self.cursor = self.buf.len();
                    return false;
                }
                Some(j) if j + 1 >= self.buf.len() => {
                    self.cursor = j;
                    return false;
                }
                Some(j) if is_entropy_continuation(self.buf[j + 1]) => from = j + 1,
                Some(j) => {
                    self.cursor = j;
                    self.in_entropy = false;
                    return true;
                }
            }
        }
    }

    fn emit(&mut self, end: usize) -> Vec<u8> {
        self.cursor = 0;
        self.in_entropy = false;
        self.buf.drain(..end).collect()
    }

    fn drop_current(&mut self) {
        let n = self.buf.len().min(2);
        self.buf.drain(..n);
        self.dropped += 1;
        self.cursor = 0;
        self.in_entropy = false;
    }
}

/// Width and height from the first start-of-frame segment, if the header is readable.
pub fn dimensions(frame: &[u8]) -> Option<(u16, u16)> {
    if !frame.starts_with(&SOI) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= frame.len() {
        if frame[i] != 0xFF {
            return None;
        }
        let marker = frame[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if is_standalone(marker) {
            i += 2;
            continue;
        }
        if marker == SOS || marker == EOI {
            return None;
        }
        if is_start_of_frame(marker) && i + 9 <= frame.len() {
            let height = u16::from_be_bytes([frame[i + 5], frame[i + 6]]);
            let width = u16::from_be_bytes([frame[i + 7], frame[i + 8]]);
            return Some((width, height));
        }
        i += 2 + usize::from(u16::from_be_bytes([frame[i + 2], frame[i + 3]]));
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, marker];
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    pub(crate) fn sof(width: u16, height: u16) -> Vec<u8> {
        let mut payload = vec![8];
        payload.extend_from_slice(&height.to_be_bytes());
        payload.extend_from_slice(&width.to_be_bytes());
        payload.extend_from_slice(&[1, 1, 0x11, 0]);
        segment(0xC0, &payload)
    }

    pub(crate) fn fake_jpeg_with(entropy: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut out = SOI.to_vec();
        out.extend(segment(0xE0, b"JFIF\0"));
        out.extend_from_slice(extra);
        out.extend(sof(320, 240));
        out.extend(segment(0xDA, &[1, 1, 0, 0, 0x3F, 0]));
        out.extend_from_slice(entropy);
        out.extend_from_slice(&[0xFF, EOI]);
        out
    }

    pub(crate) fn fake_jpeg() -> Vec<u8> {
        fake_jpeg_with(&[0x12, 0x34, 0xFF, 0x00, 0x56], &[])
    }

    fn feed_in_chunks(splitter: &mut JpegSplitter, data: &[u8], size: usize) -> Vec<Vec<u8>> {
        data.chunks(size).flat_map(|c| splitter.feed(c)).collect()
    }

    #[test]
    fn single_frame() {
        let frame = fake_jpeg();
        assert_eq!(JpegSplitter::default().feed(&frame), vec![frame]);
    }

    #[test]
    fn concatenated_frames() {
        let frames: Vec<_> = (0..5u8).map(|i| fake_jpeg_with(&[i; 10], &[])).collect();
        assert_eq!(JpegSplitter::default().feed(&frames.concat()), frames);
    }

    #[test]
    fn every_chunk_boundary() {
        let frames = vec![fake_jpeg(), fake_jpeg_with(&[0xFF, 0xD0, 0x01, 0xFF, 0x00], &[])];
        let data = frames.concat();
        for size in 1..=data.len() {
            assert_eq!(feed_in_chunks(&mut JpegSplitter::default(), &data, size), frames, "chunk size {size}");
        }
    }

    #[test]
    fn garbage_between_and_around_frames_is_discarded() {
        let (a, b) = (fake_jpeg(), fake_jpeg_with(&[0x99], &[]));
        let data = [b"noise\xff".as_slice(), &a, b"\x00\x01\xff\xfe", &b, b"trailing"].concat();
        assert_eq!(JpegSplitter::default().feed(&data), vec![a, b]);
    }

    #[test]
    fn eoi_inside_header_segment_does_not_end_frame() {
        let thumbnail = fake_jpeg_with(&[0x01], &[]);
        let exif = segment(0xE1, &[b"Exif\0\0".as_slice(), &thumbnail].concat());
        let frame = fake_jpeg_with(&[0x42], &exif);
        assert_eq!(JpegSplitter::default().feed(&frame), vec![frame]);
    }

    #[test]
    fn stuffed_restart_and_fill_bytes_in_entropy_data() {
        let frame = fake_jpeg_with(&[0xFF, 0x00, 0xFF, 0xD3, 0xAA, 0xFF, 0xFF, 0x00, 0xBB], &[]);
        assert_eq!(JpegSplitter::default().feed(&frame), vec![frame]);
    }

    #[test]
    fn progressive_multiple_scans() {
        let sos = segment(0xDA, &[1, 1, 0, 0, 0x3F, 0]);
        let frame = [
            SOI.as_slice(),
            &sos,
            &[1, 2],
            &segment(0xC4, &[0; 17]),
            &sos,
            &[3],
            &[0xFF, EOI],
        ]
        .concat();
        assert_eq!(JpegSplitter::default().feed(&frame), vec![frame]);
    }

    #[test]
    fn incomplete_frame_is_held_until_completed() {
        let frame = fake_jpeg();
        let mut s = JpegSplitter::default();
        assert!(s.feed(&frame[..frame.len() - 1]).is_empty());
        assert_eq!(s.feed(&frame[frame.len() - 1..]), vec![frame]);
    }

    #[test]
    fn malformed_frame_resyncs_on_next_soi() {
        let good = fake_jpeg();
        let data = [b"\xff\xd8\x00\x00garbage".as_slice(), &good].concat();
        let mut s = JpegSplitter::default();
        assert_eq!(s.feed(&data), vec![good]);
        assert_eq!(s.dropped(), 1);
    }

    #[test]
    fn oversized_frame_is_dropped() {
        let mut s = JpegSplitter::new(64);
        let big = [SOI.as_slice(), &segment(0xDA, &[0; 4]), &[1; 100]].concat();
        s.feed(&big);
        assert_eq!(s.dropped(), 1);
        let good = fake_jpeg();
        assert_eq!(s.feed(&good), vec![good]);
    }

    #[test]
    fn dimensions_from_sof() {
        assert_eq!(dimensions(&fake_jpeg()), Some((320, 240)));
    }

    #[test]
    fn dimensions_skip_exif_thumbnail_segment() {
        let thumbnail = [SOI.as_slice(), &sof(160, 120), &[0xFF, EOI]].concat();
        let exif = segment(0xE1, &[b"Exif\0\0".as_slice(), &thumbnail].concat());
        assert_eq!(dimensions(&fake_jpeg_with(&[0x42], &exif)), Some((320, 240)));
    }

    #[test]
    fn dimensions_reject_non_jpeg() {
        assert_eq!(dimensions(b"not a jpeg"), None);
        assert_eq!(dimensions(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
    }

    /// Set `LIVESTREAM_FIXTURE` to a real `gphoto2 --stdout` capture and
    /// `LIVESTREAM_FIXTURE_FRAMES` to its frame count (e.g. from ffprobe).
    #[test]
    fn real_capture_fixture() {
        let (Ok(path), Ok(expected)) = (
            std::env::var("LIVESTREAM_FIXTURE"),
            std::env::var("LIVESTREAM_FIXTURE_FRAMES"),
        ) else {
            return;
        };
        let data = std::fs::read(path).unwrap();
        let expected: usize = expected.parse().unwrap();
        for size in [1000, 4096, 65536, data.len()] {
            let mut s = JpegSplitter::default();
            let frames = feed_in_chunks(&mut s, &data, size);
            assert_eq!(frames.len(), expected, "chunk size {size}");
            assert_eq!(frames.iter().map(Vec::len).sum::<usize>(), data.len());
            assert!(frames.iter().all(|f| dimensions(f).is_some()));
        }
    }
}
