//! Writing the current frame to disk as a standalone JPEG.
//!
//! Frames are saved byte-for-byte as the camera produced them (full
//! resolution and EXIF intact in stills mode). The file name stem comes from
//! the caller (the UI formats a local timestamp) and is sanitised here; an
//! existing file is never overwritten, a numeric suffix is added instead.

use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

const MAX_STEM_CHARS: usize = 100;
const MAX_SUFFIX: u32 = 999;

/// Keep `[A-Za-z0-9_-]`, replace anything else with `_`; never empty.
pub fn sanitize_stem(stem: &str) -> String {
    let cleaned: String = stem
        .chars()
        .take(MAX_STEM_CHARS)
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    match cleaned.trim_matches('_').is_empty() {
        true => "frame".to_string(),
        false => cleaned,
    }
}

/// Candidate file name for attempt `n` (0 = no suffix).
pub fn candidate_name(stem: &str, n: u32) -> String {
    match n {
        0 => format!("{stem}.jpg"),
        n => format!("{stem}-{}.jpg", n + 1),
    }
}

/// Write `frame` into `dir` as `<stem>.jpg`, or `<stem>-2.jpg`, ... if taken.
pub fn save_frame(dir: &Path, stem: &str, frame: &[u8]) -> io::Result<PathBuf> {
    let stem = sanitize_stem(stem);
    for n in 0..=MAX_SUFFIX {
        let path = dir.join(candidate_name(&stem, n));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(frame)?;
                file.sync_all()?;
                return Ok(path);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(ErrorKind::AlreadyExists, format!("too many files named {stem}-N.jpg")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("livestream-snap-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_stem("livestream-2026-11-03_16-21-28"), "livestream-2026-11-03_16-21-28");
        assert_eq!(sanitize_stem("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize_stem("a b:c"), "a_b_c");
        assert_eq!(sanitize_stem(""), "frame");
        assert_eq!(sanitize_stem("///"), "frame");
        assert_eq!(sanitize_stem(&"x".repeat(500)).len(), MAX_STEM_CHARS);
    }

    #[test]
    fn candidate_names() {
        assert_eq!(candidate_name("f", 0), "f.jpg");
        assert_eq!(candidate_name("f", 1), "f-2.jpg");
    }

    #[test]
    fn saves_bytes_and_never_overwrites() {
        let dir = temp_dir("unique");
        let a = save_frame(&dir, "shot", b"first").unwrap();
        let b = save_frame(&dir, "shot", b"second").unwrap();
        assert_eq!(a, dir.join("shot.jpg"));
        assert_eq!(b, dir.join("shot-2.jpg"));
        assert_eq!(fs::read(&a).unwrap(), b"first");
        assert_eq!(fs::read(&b).unwrap(), b"second");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn path_traversal_stays_in_dir() {
        let dir = temp_dir("traversal");
        let path = save_frame(&dir, "../escape", b"x").unwrap();
        assert_eq!(path.parent().unwrap(), dir);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_dir_is_an_error() {
        assert!(save_frame(Path::new("/nonexistent-livestream-dir"), "x", b"x").is_err());
    }
}
