//! Camera source: UVC / V4L2 devices on Linux, AVFoundation devices on macOS,
//! captured by ffmpeg and written to stdout as concatenated JPEGs.
//!
//! MJPEG input is passed through untouched (`-c:v copy`); every other pixel
//! format is transcoded to MJPEG. Device discovery lives in [`crate::v4l2`]
//! and [`crate::avfoundation`]; this module holds the backend-neutral model.

use std::fmt;
use std::process::Command;

use crate::tools;

/// ffmpeg MJPEG encoder quality for transcoded formats (2 = best practical).
pub const TRANSCODE_QUALITY: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    V4l2,
    AvFoundation,
}

impl Backend {
    /// The backend for `os` (as in [`std::env::consts::OS`]), if cameras are supported there.
    pub fn for_os(os: &str) -> Option<Backend> {
        match os {
            "linux" => Some(Backend::V4l2),
            "macos" => Some(Backend::AvFoundation),
            _ => None,
        }
    }

    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Backend::V4l2 => "v4l2",
            Backend::AvFoundation => "avfoundation",
        }
    }
}

/// Frames per second as the exact fraction `num / den`, reduced.
///
/// V4L2 reports frame intervals as fractions (often `333333/10000000`);
/// keeping them exact lets ffmpeg request the very interval the driver listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

impl FrameRate {
    /// `num / den` reduced; `None` when either part is zero.
    pub fn new(num: u32, den: u32) -> Option<FrameRate> {
        match (num, den) {
            (0, _) | (_, 0) => None,
            _ => {
                let g = gcd(num, den);
                Some(FrameRate { num: num / g, den: den / g })
            }
        }
    }

    /// The rate of a frame interval of `num / den` seconds.
    pub fn from_interval(num: u32, den: u32) -> Option<FrameRate> {
        FrameRate::new(den, num)
    }

    pub fn fps(self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    match b {
        0 => a,
        _ => gcd(b, a % b),
    }
}

/// V4L2 fourcc → ffmpeg `-input_format` (pixel format or codec name).
///
/// Formats missing here are not offered: ffmpeg could not be told to request them.
pub const V4L2_INPUT_FORMATS: [(&str, &str); 20] = [
    ("MJPG", "mjpeg"),
    ("JPEG", "mjpeg"),
    ("H264", "h264"),
    ("HEVC", "hevc"),
    ("YUYV", "yuyv422"),
    ("UYVY", "uyvy422"),
    ("YVYU", "yvyu422"),
    ("422P", "yuv422p"),
    ("YU12", "yuv420p"),
    ("NV12", "nv12"),
    ("NV21", "nv21"),
    ("GREY", "gray"),
    ("Y10", "gray10le"),
    ("Y12", "gray12le"),
    ("Y16", "gray16le"),
    ("RGB3", "rgb24"),
    ("BGR3", "bgr24"),
    ("BA81", "bayer_bggr8"),
    ("GBRG", "bayer_gbrg8"),
    ("RGGB", "bayer_rggb8"),
];

/// ffmpeg `-input_format` for a V4L2 fourcc (trailing spaces ignored).
pub fn ffmpeg_input_format(fourcc: &str) -> Option<&'static str> {
    let fourcc = fourcc.trim_end();
    V4L2_INPUT_FORMATS.iter().find(|(f, _)| *f == fourcc).map(|(_, name)| *name)
}

/// Whether frames in `fourcc` are already JPEG and can be copied without re-encoding.
pub fn is_passthrough(fourcc: &str) -> bool {
    ffmpeg_input_format(fourcc) == Some("mjpeg")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraConfig {
    pub backend: Backend,
    /// `/dev/videoN` (V4L2) or the device name (AVFoundation).
    pub device: String,
    /// V4L2 fourcc such as `MJPG`; `None` lets AVFoundation pick its native format.
    pub format: Option<String>,
    pub width: u32,
    pub height: u32,
    pub rate: FrameRate,
}

impl CameraConfig {
    pub fn validate(&self) -> Result<(), String> {
        match (self.backend, self.format.as_deref()) {
            _ if self.device.trim().is_empty() => Err("no camera selected".into()),
            _ if self.width == 0 || self.height == 0 => Err("camera size must be non-zero".into()),
            (_, _) if self.rate.num == 0 || self.rate.den == 0 => Err("frame rate must be non-zero".into()),
            (Backend::V4l2, None) => Err("V4L2 cameras need a pixel format".into()),
            (Backend::V4l2, Some(f)) if ffmpeg_input_format(f).is_none() => {
                Err(format!("pixel format {f:?} is not supported by ffmpeg's V4L2 input"))
            }
            _ => Ok(()),
        }
    }

    /// Whether frames are copied as-is (MJPEG input) rather than transcoded.
    pub fn passthrough(&self) -> bool {
        self.backend == Backend::V4l2 && self.format.as_deref().is_some_and(is_passthrough)
    }

    /// ffmpeg arguments writing concatenated JPEGs to stdout until interrupted.
    pub fn ffmpeg_args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "warning", "-f", self.backend.ffmpeg_format()]
            .map(String::from)
            .to_vec();
        if let (Backend::V4l2, Some(format)) = (self.backend, self.format.as_deref()) {
            args.extend(["-input_format".into(), ffmpeg_input_format(format).unwrap_or(format).into()]);
        }
        args.extend([
            "-video_size".into(),
            format!("{}x{}", self.width, self.height),
            "-framerate".into(),
            self.rate.to_string(),
            "-i".into(),
            self.input(),
        ]);
        args.extend(self.codec_args());
        args.extend(["-f".into(), "mjpeg".into(), "-".into()]);
        args
    }

    fn input(&self) -> String {
        match self.backend {
            Backend::V4l2 => self.device.clone(),
            Backend::AvFoundation => format!("{}:none", self.device),
        }
    }

    fn codec_args(&self) -> Vec<String> {
        match self.passthrough() {
            true => vec!["-c:v".into(), "copy".into()],
            false => vec!["-c:v".into(), "mjpeg".into(), "-q:v".into(), TRANSCODE_QUALITY.to_string()],
        }
    }
}

/// Build the ffmpeg command for `config`, failing with an actionable message if absent.
pub fn ffmpeg_command(config: &CameraConfig) -> Result<Command, String> {
    let binary = tools::locate("ffmpeg").ok_or_else(|| tools::not_found("ffmpeg"))?;
    let mut command = Command::new(binary);
    command.args(config.ffmpeg_args());
    Ok(command)
}

/// One capture mode: a pixel format at one size, with the rates it supports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraMode {
    /// V4L2 fourcc; `None` for AVFoundation.
    pub format: Option<String>,
    /// Human-readable format description from the driver.
    pub format_label: String,
    pub width: u32,
    pub height: u32,
    /// Fastest first.
    pub rates: Vec<FrameRate>,
    /// Frames are re-encoded to MJPEG (costs CPU, slight quality loss).
    pub transcoded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraDevice {
    pub backend: Backend,
    /// What goes into [`CameraConfig::device`].
    pub id: String,
    pub name: String,
    pub modes: Vec<CameraMode>,
    /// Set when the device exists but could not be queried (permissions, busy...).
    pub error: Option<String>,
}

/// `items` without later duplicates, order kept (firmwares list sizes and intervals twice).
pub fn dedup_keep_order<T: PartialEq + Clone>(items: &[T]) -> Vec<T> {
    items.iter().enumerate().filter(|(i, x)| !items[..*i].contains(x)).map(|(_, x)| x.clone()).collect()
}

/// `rates` deduplicated and sorted fastest first.
pub fn sorted_rates(rates: &[FrameRate]) -> Vec<FrameRate> {
    let mut out = dedup_keep_order(rates);
    out.sort_by(|a, b| b.fps().total_cmp(&a.fps()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4l2(format: &str) -> CameraConfig {
        CameraConfig {
            backend: Backend::V4l2,
            device: "/dev/video0".into(),
            format: Some(format.into()),
            width: 1280,
            height: 720,
            rate: FrameRate::new(30, 1).unwrap(),
        }
    }

    #[test]
    fn frame_rates_are_reduced_and_built_from_intervals() {
        assert_eq!(FrameRate::new(60, 2), Some(FrameRate { num: 30, den: 1 }));
        assert_eq!(FrameRate::from_interval(1, 30), Some(FrameRate { num: 30, den: 1 }));
        assert_eq!(
            FrameRate::from_interval(333333, 10000000),
            Some(FrameRate { num: 10000000, den: 333333 })
        );
        assert_eq!(FrameRate::from_interval(1001, 30000).unwrap().to_string(), "30000/1001");
        assert_eq!(FrameRate::new(0, 1), None);
        assert_eq!(FrameRate::new(1, 0), None);
    }

    #[test]
    fn mjpeg_is_copied() {
        assert_eq!(
            v4l2("MJPG").ffmpeg_args(),
            [
                "-hide_banner", "-nostdin", "-loglevel", "warning", "-f", "v4l2", "-input_format", "mjpeg",
                "-video_size", "1280x720", "-framerate", "30/1", "-i", "/dev/video0", "-c:v", "copy", "-f",
                "mjpeg", "-"
            ]
        );
        assert!(v4l2("MJPG").passthrough());
    }

    #[test]
    fn other_formats_are_transcoded() {
        let args = v4l2("YUYV").ffmpeg_args();
        assert!(args.windows(2).any(|w| w == ["-input_format", "yuyv422"]));
        assert!(args.windows(4).any(|w| w == ["-c:v", "mjpeg", "-q:v", "2"]));
        assert!(!v4l2("H264").passthrough());
    }

    #[test]
    fn avfoundation_has_no_input_format_and_no_audio() {
        let config = CameraConfig {
            backend: Backend::AvFoundation,
            device: "FaceTime HD Camera".into(),
            format: None,
            ..v4l2("MJPG")
        };
        let args = config.ffmpeg_args();
        assert!(!args.contains(&"-input_format".to_string()));
        assert!(args.windows(2).any(|w| w == ["-f", "avfoundation"]));
        assert!(args.windows(2).any(|w| w == ["-i", "FaceTime HD Camera:none"]));
        assert!(args.windows(2).any(|w| w == ["-c:v", "mjpeg"]));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validation() {
        assert!(v4l2("MJPG").validate().is_ok());
        assert!(v4l2("Y16 ").validate().is_ok());
        assert!(v4l2("XXXX").validate().is_err());
        assert!(CameraConfig { device: " ".into(), ..v4l2("MJPG") }.validate().is_err());
        assert!(CameraConfig { width: 0, ..v4l2("MJPG") }.validate().is_err());
        assert!(CameraConfig { format: None, ..v4l2("MJPG") }.validate().is_err());
        assert!(CameraConfig { rate: FrameRate { num: 0, den: 1 }, ..v4l2("MJPG") }.validate().is_err());
    }

    #[test]
    fn backends_per_os() {
        assert_eq!(Backend::for_os("linux"), Some(Backend::V4l2));
        assert_eq!(Backend::for_os("macos"), Some(Backend::AvFoundation));
        assert_eq!(Backend::for_os("windows"), None);
    }

    #[test]
    fn dedup_and_sort() {
        assert_eq!(dedup_keep_order(&[3, 1, 3, 2, 1]), [3, 1, 2]);
        let r = |n, d| FrameRate::new(n, d).unwrap();
        assert_eq!(sorted_rates(&[r(10, 1), r(30, 1), r(10, 1), r(5, 1)]), [r(30, 1), r(10, 1), r(5, 1)]);
    }
}
