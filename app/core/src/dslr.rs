//! DSLR source: gphoto2 liveview / stills configuration and command building.

use std::env;
use std::process::{Command, Stdio};

use crate::tools;

/// Canon JPEG-only `imageformat` choices; RAW variants would put CR2 data on stdout.
pub const JPEG_FORMATS: [&str; 6] = ["L", "cL", "M", "cM", "S", "cS"];

/// OpenCV's FFmpeg backend gives up on a read after ~30 s, so keep stills well below.
pub const MAX_STILLS_INTERVAL: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Preview frames via `--capture-movie` (fast, ~1056x704 on EOS, no shutter).
    Liveview,
    /// Full captures via `--capture-image-and-download` (fires the shutter each frame).
    Stills,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DslrConfig {
    pub mode: Mode,
    /// Stills only: seconds between captures, whole seconds as gphoto2 requires.
    pub interval: u32,
    /// Stills only: one of [`JPEG_FORMATS`].
    pub image_format: String,
}

impl Default for DslrConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Liveview,
            interval: 1,
            image_format: "L".into(),
        }
    }
}

impl DslrConfig {
    pub fn validate(&self) -> Result<(), String> {
        match self.mode {
            Mode::Liveview => Ok(()),
            Mode::Stills if !JPEG_FORMATS.contains(&self.image_format.as_str()) => Err(format!(
                "image format {:?} is not JPEG-only, expected one of {JPEG_FORMATS:?}",
                self.image_format
            )),
            Mode::Stills if !(1..=MAX_STILLS_INTERVAL).contains(&self.interval) => Err(format!(
                "interval must be between 1 and {MAX_STILLS_INTERVAL} seconds, got {}",
                self.interval
            )),
            Mode::Stills => Ok(()),
        }
    }

    /// gphoto2 arguments writing concatenated JPEGs to stdout until interrupted.
    ///
    /// Stills force the capture target to internal RAM (by index, since labels
    /// are localised) so nothing accumulates on the card.
    pub fn gphoto2_args(&self) -> Vec<String> {
        match self.mode {
            Mode::Liveview => vec!["--capture-movie".into(), "--stdout".into()],
            Mode::Stills => vec![
                "--set-config-index".into(),
                "capturetarget=0".into(),
                "--set-config".into(),
                format!("imageformat={}", self.image_format),
                "--capture-image-and-download".into(),
                "--interval".into(),
                self.interval.to_string(),
                "--stdout".into(),
            ],
        }
    }
}

/// Build the gphoto2 command for `config`, failing with an actionable message if absent.
pub fn gphoto2_command(config: &DslrConfig) -> Result<Command, String> {
    let binary = tools::locate("gphoto2").ok_or_else(|| tools::not_found("gphoto2"))?;
    let mut command = crate::tools::host_command(binary);
    command.args(config.gphoto2_args());
    Ok(command)
}

/// Commands killing desktop services that claim PTP cameras and block libgphoto2:
/// macOS's PTPCamera daemon, GNOME/GVFS's gphoto2 monitor on Linux.
pub fn camera_claimer_commands(os: &str) -> Vec<Vec<&'static str>> {
    match os {
        "macos" => vec![vec!["/usr/bin/killall", "PTPCamera"]],
        "linux" => vec![
            vec!["pkill", "-f", "gvfs-gphoto2-volume-monitor"],
            vec!["pkill", "-f", "gvfsd-gphoto2"],
        ],
        _ => vec![],
    }
}

/// Run [`camera_claimer_commands`] for the current OS, ignoring failures.
pub fn release_camera_claimers() {
    for argv in camera_claimer_commands(env::consts::OS) {
        let _ = crate::tools::host_command(argv[0])
            .args(&argv[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stills(interval: u32, format: &str) -> DslrConfig {
        DslrConfig {
            mode: Mode::Stills,
            interval,
            image_format: format.into(),
        }
    }

    #[test]
    fn liveview_args() {
        assert_eq!(DslrConfig::default().gphoto2_args(), ["--capture-movie", "--stdout"]);
    }

    #[test]
    fn stills_args() {
        assert_eq!(
            stills(3, "M").gphoto2_args(),
            [
                "--set-config-index",
                "capturetarget=0",
                "--set-config",
                "imageformat=M",
                "--capture-image-and-download",
                "--interval",
                "3",
                "--stdout"
            ]
        );
    }

    #[test]
    fn validation() {
        assert!(DslrConfig::default().validate().is_ok());
        assert!(stills(1, "L").validate().is_ok());
        assert!(stills(MAX_STILLS_INTERVAL, "cS").validate().is_ok());
        assert!(stills(1, "RAW").validate().is_err());
        assert!(stills(1, "RAW + L").validate().is_err());
        assert!(stills(0, "L").validate().is_err());
        assert!(stills(MAX_STILLS_INTERVAL + 1, "L").validate().is_err());
    }

    #[test]
    fn per_os_camera_claimers() {
        assert_eq!(camera_claimer_commands("macos"), vec![vec!["/usr/bin/killall", "PTPCamera"]]);
        assert!(camera_claimer_commands("linux").iter().all(|c| c[0] == "pkill"));
        assert!(camera_claimer_commands("windows").is_empty());
    }
}
