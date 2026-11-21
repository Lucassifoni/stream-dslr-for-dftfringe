//! AVFoundation camera discovery (macOS) by parsing ffmpeg's stderr.
//!
//! `-list_devices true` prints the video devices; opening a device with an
//! impossible `-video_size 1x1` makes ffmpeg fail and print the device's
//! "Supported modes" (size and frame-rate range). Pending tests on a real
//! macOS camera: the parsers are written against ffmpeg 7.x log formats.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::camera::{self, Backend, CameraDevice, CameraMode, FrameRate};

/// Rates offered inside a supported range, fastest first, when they fall within it.
pub const COMMON_RATES: [u32; 9] = [60, 50, 30, 25, 24, 20, 15, 10, 5];

/// AVFoundation's own tolerance when matching a requested frame rate.
const RATE_TOLERANCE: f64 = 0.01;

/// The message part of an ffmpeg log line (`[avfoundation @ 0x...] message`).
fn message(line: &str) -> &str {
    match (line.trim_start().starts_with('['), line.find("] ")) {
        (true, Some(end)) => &line[end + 2..],
        _ => line,
    }
}

/// `(index, name)` of video devices, excluding screen capture pseudo-devices.
pub fn parse_video_devices(stderr: &str) -> Vec<(u32, String)> {
    let mut in_video = false;
    let mut out = Vec::new();
    for msg in stderr.lines().map(message) {
        match msg.trim() {
            "AVFoundation video devices:" => in_video = true,
            "AVFoundation audio devices:" => in_video = false,
            entry if in_video => {
                if let Some((index, name)) = parse_entry(entry) {
                    if !name.starts_with("Capture screen") {
                        out.push((index, name));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_entry(entry: &str) -> Option<(u32, String)> {
    let rest = entry.strip_prefix('[')?;
    let (index, name) = rest.split_once("] ")?;
    Some((index.parse().ok()?, name.trim().to_string()))
}

/// `(width, height, min_fps, max_fps)` from the "Supported modes" listing, sizes merged.
pub fn parse_modes(stderr: &str) -> Vec<(u32, u32, f64, f64)> {
    let mut out: Vec<(u32, u32, f64, f64)> = Vec::new();
    for (w, h, lo, hi) in stderr.lines().map(message).filter_map(parse_mode) {
        match out.iter_mut().find(|m| m.0 == w && m.1 == h) {
            Some(m) => {
                m.2 = m.2.min(lo);
                m.3 = m.3.max(hi);
            }
            None => out.push((w, h, lo, hi)),
        }
    }
    out
}

fn parse_mode(msg: &str) -> Option<(u32, u32, f64, f64)> {
    let (size, rates) = msg.trim().split_once('@')?;
    let (w, h) = size.split_once('x')?;
    let range = rates.strip_prefix('[')?.strip_suffix("]fps")?;
    let (lo, hi) = range.split_once(' ')?;
    Some((w.parse().ok()?, h.parse().ok()?, lo.parse().ok()?, hi.parse().ok()?))
}

/// Rates to offer for a supported `[min, max]` range: the max itself, then common rates inside.
pub fn rates_in(min: f64, max: f64) -> Vec<FrameRate> {
    let top = FrameRate::new((max * 1000.0).round() as u32, 1000);
    let common = COMMON_RATES
        .iter()
        .filter(|&&r| (r as f64) >= min - RATE_TOLERANCE && (r as f64) <= max + RATE_TOLERANCE)
        .filter_map(|&r| FrameRate::new(r, 1));
    let rates: Vec<FrameRate> = top.into_iter().chain(common).collect();
    let mut out = camera::sorted_rates(&rates);
    out.dedup_by(|a, b| (a.fps() - b.fps()).abs() < RATE_TOLERANCE);
    out
}

pub fn list_devices_args() -> Vec<String> {
    ["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""].map(String::from).to_vec()
}

pub fn probe_modes_args(name: &str) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-f", "avfoundation", "-video_size", "1x1", "-i"].map(String::from).to_vec();
    args.push(format!("{name}:none"));
    args
}

/// Device description from its name and the output of [`probe_modes_args`].
pub fn device_from_probe(name: &str, probe_stderr: &str) -> CameraDevice {
    let modes: Vec<CameraMode> = parse_modes(probe_stderr)
        .into_iter()
        .map(|(width, height, lo, hi)| CameraMode {
            format: None,
            format_label: "native".into(),
            width,
            height,
            rates: rates_in(lo, hi),
            transcoded: true,
        })
        .filter(|m| !m.rates.is_empty())
        .collect();
    let error = match modes.is_empty() {
        true => Some(format!("{name}: no supported modes reported (camera permission denied?)")),
        false => None,
    };
    CameraDevice {
        backend: Backend::AvFoundation,
        id: name.to_string(),
        name: name.to_string(),
        modes,
        error,
    }
}

fn stderr_of(ffmpeg: &Path, args: &[String]) -> String {
    Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default()
}

/// Every AVFoundation camera, probing each for its modes (briefly opens the camera).
pub fn discover(ffmpeg: &Path) -> Vec<CameraDevice> {
    parse_video_devices(&stderr_of(ffmpeg, &list_devices_args()))
        .into_iter()
        .map(|(_, name)| device_from_probe(&name, &stderr_of(ffmpeg, &probe_modes_args(&name))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
[AVFoundation indev @ 0x7f8b4c004a00] AVFoundation video devices:
[AVFoundation indev @ 0x7f8b4c004a00] [0] FaceTime HD Camera
[AVFoundation indev @ 0x7f8b4c004a00] [1] USB Camera
[AVFoundation indev @ 0x7f8b4c004a00] [2] Capture screen 0
[AVFoundation indev @ 0x7f8b4c004a00] AVFoundation audio devices:
[AVFoundation indev @ 0x7f8b4c004a00] [0] MacBook Pro Microphone
: Input/output error
";

    const PROBE: &str = "\
[avfoundation @ 0x13c604a00] Selected video size (1x1) or framerate (29.970030) is not supported by the device.
[avfoundation @ 0x13c604a00] Supported modes:
[avfoundation @ 0x13c604a00]   1920x1080@[1.000000 30.000000]fps
[avfoundation @ 0x13c604a00]   1280x720@[1.000000 30.000000]fps
[avfoundation @ 0x13c604a00]   1280x720@[1.000000 60.000000]fps
[avfoundation @ 0x13c604a00]   640x480@[5.000000 15.000000]fps
USB Camera:none: Input/output error
";

    #[test]
    fn lists_video_devices_only() {
        assert_eq!(parse_video_devices(LIST), [(0, "FaceTime HD Camera".into()), (1, "USB Camera".into())]);
        assert!(parse_video_devices("nothing useful").is_empty());
    }

    #[test]
    fn parses_and_merges_modes() {
        assert_eq!(
            parse_modes(PROBE),
            [(1920, 1080, 1.0, 30.0), (1280, 720, 1.0, 60.0), (640, 480, 5.0, 15.0)]
        );
    }

    #[test]
    fn rates_inside_ranges() {
        let fps: Vec<f64> = rates_in(5.0, 15.0).iter().map(|r| r.fps()).collect();
        assert_eq!(fps, [15.0, 10.0, 5.0]);
        let fps: Vec<f64> = rates_in(1.0, 30.00003).iter().map(|r| r.fps()).collect();
        assert_eq!(fps, [30.0, 25.0, 24.0, 20.0, 15.0, 10.0, 5.0]);
        assert_eq!(rates_in(1.0, 29.97)[0].to_string(), "2997/100");
    }

    #[test]
    fn devices_from_probe() {
        let dev = device_from_probe("USB Camera", PROBE);
        assert_eq!(dev.backend, Backend::AvFoundation);
        assert_eq!(dev.modes.len(), 3);
        assert!(dev.modes.iter().all(|m| m.format.is_none() && m.transcoded));
        assert_eq!(dev.error, None);
        assert!(device_from_probe("X", "denied").error.is_some());
    }

    #[test]
    fn probe_args_disable_audio() {
        assert_eq!(probe_modes_args("USB Camera").last().unwrap(), "USB Camera:none");
        assert_eq!(list_devices_args().last().unwrap(), "");
    }
}
