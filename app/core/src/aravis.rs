//! GenICam source (USB3 Vision / GigE Vision industrial cameras: FLIR / Point
//! Grey, Basler, ...) through Aravis.
//!
//! Frames come from a GStreamer pipeline (`aravissrc ! videoconvert !
//! jpegenc ! fdsink`) writing concatenated JPEGs to stdout. Exposure, gain,
//! binning, pixel format and frame rate are applied when the pipeline starts:
//! USB3 Vision gives one process exclusive control of the camera, so changing
//! them restarts the producer. Exposure and gain go through `aravissrc`'s own
//! properties, which map to whatever feature names the vendor uses
//! (`ExposureTime` / `ExposureTimeAbs`, `Gain` / `GainRaw`).
//!
//! Discovery parses `arv-tool-0.8` output.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::camera::FrameRate;
use crate::tools;

pub const ARV_TOOL: &str = "arv-tool-0.8";
pub const GST_LAUNCH: &str = "gst-launch-1.0";
/// Debian / Ubuntu / Mint packages providing both tools and the `aravissrc` element.
pub const APT_PACKAGES: &str = "aravis-tools-cli gstreamer1.0-tools gstreamer1.0-plugins-good";
pub const JPEG_QUALITY: u32 = 95;
pub const MAX_BINNING: u32 = 16;

/// GenICam `PixelFormat` → GStreamer raw caps format, for formats `videoconvert` can take.
pub const PIXEL_FORMATS: [(&str, &str); 4] = [("Mono8", "GRAY8"), ("Mono16", "GRAY16_LE"), ("RGB8", "RGB"), ("BGR8", "BGR")];

pub fn caps_format(pixel_format: &str) -> Option<&'static str> {
    PIXEL_FORMATS.iter().find(|(p, _)| *p == pixel_format).map(|(_, c)| *c)
}

/// How a camera setting is applied when the pipeline starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    /// Leave the camera's current setting alone.
    Unchanged,
    /// Camera-driven (GenICam `...Auto = Continuous`).
    Auto,
    /// Fixed value: microseconds for exposure, camera units (usually dB) for gain.
    Manual(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AravisConfig {
    /// Aravis device id, as listed by `arv-tool-0.8` (e.g. `Basler-210ab4`).
    pub camera: String,
    /// One of [`PIXEL_FORMATS`].
    pub pixel_format: String,
    /// Same factor horizontally and vertically; 1 disables binning.
    pub binning: u32,
    /// `None` keeps the camera's frame rate.
    pub rate: Option<FrameRate>,
    pub exposure: Level,
    pub gain: Level,
}

impl AravisConfig {
    pub fn validate(&self) -> Result<(), String> {
        let forbidden = |c: char| matches!(c, '"' | '\\' | '!') || c.is_control();
        match () {
            _ if self.camera.trim().is_empty() => Err("no GenICam camera selected".into()),
            _ if self.camera.chars().any(forbidden) => Err(format!("unsupported camera id {:?}", self.camera)),
            _ if caps_format(&self.pixel_format).is_none() => Err(format!(
                "pixel format {:?} is not supported, expected one of {:?}",
                self.pixel_format,
                PIXEL_FORMATS.map(|(p, _)| p)
            )),
            _ if !(1..=MAX_BINNING).contains(&self.binning) => {
                Err(format!("binning must be between 1 and {MAX_BINNING}, got {}", self.binning))
            }
            _ if matches!(self.exposure, Level::Manual(v) if !(v.is_finite() && v > 0.0)) => {
                Err("exposure must be a positive number of microseconds".into())
            }
            _ if matches!(self.gain, Level::Manual(v) if !v.is_finite()) => Err("gain must be a number".into()),
            _ => Ok(()),
        }
    }

    /// `gst-launch-1.0` arguments; gst-launch joins them into one pipeline description.
    pub fn gst_args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec!["-q".into(), "-e".into(), "aravissrc".into()];
        args.push(format!("camera-name=\"{}\"", self.camera));
        args.extend(level_args("exposure", self.exposure));
        args.extend(level_args("gain", self.gain));
        if self.binning > 1 {
            args.push(format!("h-binning={}", self.binning));
            args.push(format!("v-binning={}", self.binning));
        }
        let format = caps_format(&self.pixel_format).unwrap_or("GRAY8");
        let caps = match self.rate {
            Some(rate) => format!("video/x-raw,format={format},framerate={rate}"),
            None => format!("video/x-raw,format={format}"),
        };
        args.extend(
            ["!", &caps, "!", "videoconvert", "!", "jpegenc", &format!("quality={JPEG_QUALITY}"), "!", "fdsink", "fd=1"]
                .map(String::from),
        );
        args
    }
}

fn level_args(name: &str, level: Level) -> Vec<String> {
    match level {
        Level::Unchanged => vec![],
        Level::Auto => vec![format!("{name}-auto=on")],
        Level::Manual(v) => vec![format!("{name}-auto=off"), format!("{name}={v}")],
    }
}

/// Build the gst-launch command for `config`, failing with an actionable message if absent.
pub fn gst_command(config: &AravisConfig) -> Result<Command, String> {
    let binary = tools::locate(GST_LAUNCH).ok_or_else(missing_tools)?;
    let mut command = Command::new(binary);
    command.args(config.gst_args());
    Ok(command)
}

/// Error message when the Aravis / GStreamer tools are not installed.
pub fn missing_tools() -> String {
    format!("Aravis or GStreamer tools not found — install them with `sudo apt install {APT_PACKAGES}`")
}

/// A numeric feature with its bounds, as `arv-tool control` reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    pub value: f64,
    pub min: f64,
    pub max: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AravisDevice {
    pub id: String,
    /// Transport address (IP for GigE, empty / bus id for USB).
    pub address: String,
    /// Supported entries of [`PIXEL_FORMATS`], camera's current one first.
    pub pixel_formats: Vec<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub max_binning: u32,
    pub exposure: Option<Range>,
    pub exposure_auto: bool,
    pub gain: Option<Range>,
    pub gain_auto: bool,
    pub frame_rate: Option<Range>,
    pub error: Option<String>,
}

/// Value read by `arv-tool control`.
#[derive(Clone, Debug, PartialEq)]
pub enum Reading {
    Number(Range),
    Text(String),
}

/// `(id, address)` per line of `arv-tool-0.8` without a command.
pub fn parse_device_list(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "No device found")
        .map(|l| match l.strip_suffix(')').and_then(|s| s.rsplit_once(" (")) {
            Some((id, address)) => (id.to_string(), address.to_string()),
            None => (l.to_string(), String::new()),
        })
        .collect()
}

/// `name = value [unit] [min:x max:y]` lines of `arv-tool control`; missing features are absent.
pub fn parse_control(stdout: &str) -> Vec<(String, Reading)> {
    stdout.lines().filter_map(parse_control_line).collect()
}

fn parse_control_line(line: &str) -> Option<(String, Reading)> {
    let (name, rest) = line.split_once(" = ")?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let bound = |prefix: &str| tokens.iter().find_map(|t| t.strip_prefix(prefix)).and_then(|v| v.parse::<f64>().ok());
    let reading = match (tokens.first().and_then(|v| v.parse::<f64>().ok()), bound("min:"), bound("max:")) {
        (Some(value), Some(min), Some(max)) => Reading::Number(Range { value, min, max }),
        _ => Reading::Text(rest.trim().to_string()),
    };
    Some((name.trim().to_string(), reading))
}

/// Entries of the `feature` enumeration in an `arv-tool features` tree.
pub fn parse_enum_entries(stdout: &str, feature: &str) -> Vec<String> {
    let header = format!("'{feature}'");
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut lines = stdout.lines().skip_while(|l| !(l.contains("Enumeration") && l.trim_end().ends_with(&header)));
    let base = match lines.next() {
        Some(l) => indent(l),
        None => return vec![],
    };
    lines
        .take_while(|l| indent(l) > base)
        .filter_map(|l| l.trim().strip_prefix("EnumEntry").map(|r| r.trim_start_matches([' ', ':']).trim_matches('\'').to_string()))
        .collect()
}

/// Features read with `arv-tool control` during discovery, SFNC names before legacy ones.
pub const PROBED_FEATURES: [&str; 12] = [
    "PixelFormat",
    "Width",
    "Height",
    "BinningHorizontal",
    "ExposureTime",
    "ExposureTimeAbs",
    "ExposureAuto",
    "Gain",
    "GainRaw",
    "GainAuto",
    "AcquisitionFrameRate",
    "AcquisitionFrameRateAbs",
];

/// Assemble a device from its id, `control` readings and `PixelFormat` entries.
pub fn device_from(id: &str, address: &str, readings: &[(String, Reading)], entries: &[String]) -> AravisDevice {
    let get = |name: &str| readings.iter().find(|(n, _)| n == name).map(|(_, r)| r);
    let range = |names: &[&str]| {
        names.iter().find_map(|n| match get(n) {
            Some(Reading::Number(r)) => Some(*r),
            _ => None,
        })
    };
    let current_format = match get("PixelFormat") {
        Some(Reading::Text(t)) => Some(t.clone()),
        _ => None,
    };
    let mut pixel_formats: Vec<String> =
        entries.iter().filter(|e| caps_format(e).is_some()).cloned().collect();
    if let Some(current) = current_format.filter(|c| caps_format(c).is_some()) {
        pixel_formats.retain(|p| *p != current);
        pixel_formats.insert(0, current);
    }
    let error = match pixel_formats.is_empty() {
        true => Some(format!(
            "{id}: no supported pixel format (supported: {:?})",
            PIXEL_FORMATS.map(|(p, _)| p)
        )),
        false => None,
    };
    AravisDevice {
        id: id.to_string(),
        address: address.to_string(),
        pixel_formats,
        width: range(&["Width"]).map(|r| r.value as u32),
        height: range(&["Height"]).map(|r| r.value as u32),
        max_binning: range(&["BinningHorizontal"]).map(|r| r.max.clamp(1.0, MAX_BINNING as f64) as u32).unwrap_or(1),
        exposure: range(&["ExposureTime", "ExposureTimeAbs"]),
        exposure_auto: get("ExposureAuto").is_some(),
        gain: range(&["Gain", "GainRaw"]),
        gain_auto: get("GainAuto").is_some(),
        frame_rate: range(&["AcquisitionFrameRate", "AcquisitionFrameRateAbs"]),
        error,
    }
}

fn run(tool: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", tool.display()))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every camera Aravis sees, with its capabilities. Opens each camera briefly,
/// so a camera that is currently streaming is reported with an error.
pub fn discover(arv_tool: &Path) -> Result<Vec<AravisDevice>, String> {
    let devices = parse_device_list(&run(arv_tool, &[])?);
    Ok(devices
        .into_iter()
        .map(|(id, address)| {
            let mut control = vec!["-n", id.as_str(), "control"];
            control.extend(PROBED_FEATURES);
            let readings = run(arv_tool, &control).map(|out| parse_control(&out)).unwrap_or_default();
            let entries = run(arv_tool, &["-n", &id, "features"])
                .map(|out| parse_enum_entries(&out, "PixelFormat"))
                .unwrap_or_default();
            match readings.is_empty() {
                true => AravisDevice {
                    error: Some(format!("{id}: cannot read features (camera busy or permission denied?)")),
                    ..device_from(&id, &address, &[], &[])
                },
                false => device_from(&id, &address, &readings, &entries),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded from `arv-fake-gv-camera-0.8` (Aravis 0.8.30), which uses legacy names.
    const FAKE_CONTROL: &str = "\
Aravis-Fake-GV01 (127.0.0.1)
PixelFormat = Mono8
Width = 512 min:1 max:2048
Height = 512 min:1 max:2048
BinningHorizontal = 1 min:1 max:16
Feature 'ExposureTime' not found
ExposureTimeAbs = 10000 min:10 max:1e+07
Feature 'ExposureAuto' not found
Feature 'Gain' not found
GainRaw = 0 min:0 max:10
GainAuto = Off
AcquisitionFrameRate = 25 min:0.1 max:1000
Feature 'AcquisitionFrameRateAbs' not found
";

    const FAKE_FEATURES: &str = "\
Aravis-Fake-GV01 (127.0.0.1)
Category    : 'Root'
    Category    : 'ImageFormatControl'
        Integer      : [RW] 'BinningVertical'
        Enumeration  : [RW] 'PixelFormat'
            EnumEntry   : 'Mono16'
            EnumEntry   : 'RGB8'
            EnumEntry   : 'Mono8'
            EnumEntry   : 'BayerRG8'
    Category    : 'AcquisitionControl'
        Enumeration  : [RW] 'AcquisitionMode'
            EnumEntry   : 'Continuous'
";

    fn config() -> AravisConfig {
        AravisConfig {
            camera: "FLIR-Blackfly BFLY-U3-23S6M-01234567".into(),
            pixel_format: "Mono8".into(),
            binning: 1,
            rate: None,
            exposure: Level::Unchanged,
            gain: Level::Unchanged,
        }
    }

    #[test]
    fn minimal_pipeline() {
        assert_eq!(
            config().gst_args().join(" "),
            "-q -e aravissrc camera-name=\"FLIR-Blackfly BFLY-U3-23S6M-01234567\" ! video/x-raw,format=GRAY8 ! \
             videoconvert ! jpegenc quality=95 ! fdsink fd=1"
        );
    }

    #[test]
    fn pipeline_with_settings() {
        let c = AravisConfig {
            pixel_format: "Mono16".into(),
            binning: 2,
            rate: FrameRate::new(10, 1),
            exposure: Level::Manual(5000.0),
            gain: Level::Auto,
            ..config()
        };
        let line = c.gst_args().join(" ");
        assert!(line.contains(" exposure-auto=off exposure=5000 gain-auto=on h-binning=2 v-binning=2 ! "));
        assert!(line.contains("video/x-raw,format=GRAY16_LE,framerate=10/1 ! videoconvert"));
    }

    #[test]
    fn validation() {
        assert!(config().validate().is_ok());
        assert!(AravisConfig { camera: "".into(), ..config() }.validate().is_err());
        assert!(AravisConfig { camera: "a\" ! filesink location=/x".into(), ..config() }.validate().is_err());
        assert!(AravisConfig { pixel_format: "BayerRG8".into(), ..config() }.validate().is_err());
        assert!(AravisConfig { binning: 0, ..config() }.validate().is_err());
        assert!(AravisConfig { exposure: Level::Manual(0.0), ..config() }.validate().is_err());
        assert!(AravisConfig { gain: Level::Manual(f64::NAN), ..config() }.validate().is_err());
        assert!(AravisConfig { gain: Level::Manual(-3.0), ..config() }.validate().is_ok());
    }

    #[test]
    fn device_list() {
        assert_eq!(
            parse_device_list("Aravis-Fake-GV01 (127.0.0.1)\nBasler-acA1920-40um-22334455 ()\n"),
            [("Aravis-Fake-GV01".into(), "127.0.0.1".into()), ("Basler-acA1920-40um-22334455".into(), "".into())]
        );
        assert!(parse_device_list("No device found\n").is_empty());
    }

    #[test]
    fn control_readings() {
        let r = parse_control(FAKE_CONTROL);
        assert_eq!(r[0], ("PixelFormat".into(), Reading::Text("Mono8".into())));
        assert!(r.contains(&("ExposureTimeAbs".into(), Reading::Number(Range { value: 10000.0, min: 10.0, max: 1e7 }))));
        assert!(!r.iter().any(|(n, _)| n == "ExposureTime"));
        assert_eq!(
            parse_control_line("ExposureTime = 1500.5 us min:10 max:30000000"),
            Some(("ExposureTime".into(), Reading::Number(Range { value: 1500.5, min: 10.0, max: 3e7 })))
        );
    }

    #[test]
    fn enum_entries() {
        assert_eq!(parse_enum_entries(FAKE_FEATURES, "PixelFormat"), ["Mono16", "RGB8", "Mono8", "BayerRG8"]);
        assert_eq!(parse_enum_entries(FAKE_FEATURES, "AcquisitionMode"), ["Continuous"]);
        assert!(parse_enum_entries(FAKE_FEATURES, "Missing").is_empty());
    }

    #[test]
    fn device_from_fake_camera() {
        let entries = parse_enum_entries(FAKE_FEATURES, "PixelFormat");
        let dev = device_from("Aravis-Fake-GV01", "127.0.0.1", &parse_control(FAKE_CONTROL), &entries);
        assert_eq!(dev.pixel_formats, ["Mono8", "Mono16", "RGB8"], "current first, Bayer dropped");
        assert_eq!((dev.width, dev.height), (Some(512), Some(512)));
        assert_eq!(dev.max_binning, 16);
        assert_eq!(dev.exposure.map(|r| r.max), Some(1e7), "legacy ExposureTimeAbs");
        assert!(!dev.exposure_auto);
        assert_eq!(dev.gain.map(|r| r.max), Some(10.0), "legacy GainRaw");
        assert!(dev.gain_auto);
        assert_eq!(dev.frame_rate.map(|r| r.value), Some(25.0));
        assert_eq!(dev.error, None);
    }

    #[test]
    fn device_without_usable_formats_reports_it() {
        let dev = device_from("X", "", &[], &["BayerRG8".to_string()]);
        assert!(dev.error.unwrap().contains("no supported pixel format"));
    }
}
