//! Real-device checks, ignored by default. Run on Linux with a UVC camera:
//!
//!     LIVESTREAM_V4L2_DEVICE=/dev/video0 cargo test --test v4l2_device -- --ignored --test-threads=1
//!
//! The streaming test also needs `ffmpeg` in PATH. Controls touched are
//! restored to their previous value.
#![cfg(target_os = "linux")]

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use livestream_core::camera::{Backend, CameraConfig};
use livestream_core::hub::FrameHub;
use livestream_core::source::{self, CaptureConfig, SourceState, Supervisor, SupervisorOptions};
use livestream_core::v4l2::{self, ControlKind};

fn device() -> String {
    env::var("LIVESTREAM_V4L2_DEVICE").unwrap_or_else(|_| "/dev/video0".into())
}

#[test]
#[ignore]
fn discovers_the_camera_with_modes() {
    let devices = v4l2::discover();
    for d in &devices {
        eprintln!("{} {:?} modes={} error={:?}", d.id, d.name, d.modes.len(), d.error);
        for m in &d.modes {
            eprintln!("  {:?} {}x{} {:?}", m.format, m.width, m.height, m.rates.iter().map(|r| r.fps()).collect::<Vec<_>>());
        }
    }
    let camera = devices.iter().find(|d| d.id == device()).expect("device listed");
    assert_eq!(camera.error, None);
    assert!(!camera.modes.is_empty());
}

#[test]
#[ignore]
fn reads_and_sets_an_integer_control() {
    let dev = v4l2::open(&device()).unwrap();
    let controls = v4l2::controls(&dev).unwrap();
    for c in &controls {
        eprintln!("{:#x} {:?} {:?} value={:?} inactive={} menu={:?}", c.id, c.name, c.kind, c.value, c.inactive(), c.menu);
    }
    let c = controls
        .iter()
        .find(|c| c.kind == ControlKind::Integer && !c.inactive() && !c.read_only() && c.maximum > c.minimum)
        .expect("a writable integer control");
    let before = c.value.unwrap();
    let target = if before == c.maximum { c.minimum } else { c.maximum };
    v4l2::set_control(&dev, c.id, target).unwrap();
    assert_eq!(v4l2::get_control(&dev, c.id).unwrap(), target);
    v4l2::set_control(&dev, c.id, before).unwrap();
}

#[test]
#[ignore]
fn streams_every_discovered_format_and_sets_controls_while_streaming() {
    let camera = v4l2::discover().into_iter().find(|d| d.id == device()).expect("device listed");
    let hub = Arc::new(FrameHub::new());
    let options = SupervisorOptions { restart_delay: Duration::from_millis(200), stop_grace: Duration::from_secs(3) };
    let sup = Supervisor::new(hub.clone(), Box::new(source::producer_command), None, options);
    let mut seen_formats = Vec::new();
    for mode in camera.modes.iter().filter(|m| {
        let new = !seen_formats.contains(&m.format);
        seen_formats.push(m.format.clone());
        new
    }) {
        let config = CameraConfig {
            backend: Backend::V4l2,
            device: camera.id.clone(),
            format: mode.format.clone(),
            width: mode.width,
            height: mode.height,
            rate: mode.rates[0],
        };
        sup.set_config(CaptureConfig::Camera(config)).unwrap();
        sup.set_playing(true);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(sup.status().state == SourceState::Streaming && sup.status().frames >= 10) {
            assert!(Instant::now() < deadline, "{:?}: no frames, status {:?}", mode.format, sup.status());
            std::thread::sleep(Duration::from_millis(50));
        }
        let status = sup.status();
        eprintln!("{:?} {}x{}: dims {:?}, {:.1} fps", mode.format, mode.width, mode.height, status.dimensions, status.fps);
        assert_eq!(status.dimensions, Some((mode.width as u16, mode.height as u16)));

        let dev = v4l2::open(&camera.id).unwrap();
        let gain = v4l2::controls(&dev).unwrap().into_iter().find(|c| c.name == "Gain");
        if let Some(gain) = gain {
            let before = gain.value.unwrap();
            v4l2::set_control(&dev, gain.id, gain.maximum).unwrap();
            assert_eq!(v4l2::get_control(&dev, gain.id).unwrap(), gain.maximum);
            v4l2::set_control(&dev, gain.id, before).unwrap();
        }
        sup.set_playing(false);
    }
    sup.shutdown();
}
