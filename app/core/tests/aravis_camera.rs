//! Real-camera checks through Aravis, ignored by default. With Aravis'
//! simulator (`arv-fake-gv-camera-0.8 -i 127.0.0.1`, loopback-only network):
//!
//!     LIVESTREAM_ARAVIS_CAMERA=Aravis-Fake-GV01 cargo test --test aravis_camera -- --ignored --test-threads=1
//!
//! Needs `arv-tool-0.8`, `gst-launch-1.0`, the `aravissrc` element and `jpegenc`.

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use livestream_core::aravis::{self, AravisConfig, Level};
use livestream_core::camera::FrameRate;
use livestream_core::hub::FrameHub;
use livestream_core::source::{self, CaptureConfig, SourceState, Supervisor, SupervisorOptions};
use livestream_core::tools;

fn camera_id() -> String {
    env::var("LIVESTREAM_ARAVIS_CAMERA").unwrap_or_else(|_| "Aravis-Fake-GV01".into())
}

#[test]
#[ignore]
fn discovers_and_streams_with_settings() {
    let arv_tool = tools::locate(aravis::ARV_TOOL).expect("arv-tool-0.8 installed");
    let devices = aravis::discover(&arv_tool).unwrap();
    eprintln!("{devices:#?}");
    let dev = devices.into_iter().find(|d| d.id == camera_id()).expect("camera listed");
    assert_eq!(dev.error, None);

    let hub = Arc::new(FrameHub::new());
    let options = SupervisorOptions { restart_delay: Duration::from_millis(300), stop_grace: Duration::from_secs(3) };
    let sup = Supervisor::new(hub, Box::new(source::producer_command), Some(Box::new(source::prepare)), options);
    let cases = [
        (dev.pixel_formats[0].clone(), 1, None, Level::Unchanged, Level::Unchanged),
        (dev.pixel_formats[0].clone(), 2, FrameRate::new(10, 1), Level::Manual(5000.0), Level::Manual(1.0)),
        ("Mono16".to_string(), 1, None, Level::Unchanged, Level::Unchanged),
    ];
    for (pixel_format, binning, rate, exposure, gain) in cases {
        if !dev.pixel_formats.contains(&pixel_format) {
            continue;
        }
        let config = AravisConfig { camera: dev.id.clone(), pixel_format: pixel_format.clone(), binning, rate, exposure, gain };
        sup.set_config(CaptureConfig::Aravis(config)).unwrap();
        sup.set_playing(true);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !(sup.status().state == SourceState::Streaming && sup.status().frames >= 5) {
            assert!(Instant::now() < deadline, "{pixel_format} x{binning}: no frames, status {:?}", sup.status());
            std::thread::sleep(Duration::from_millis(50));
        }
        let status = sup.status();
        eprintln!("{pixel_format} binning {binning} rate {rate:?}: {:?} {:.1} fps", status.dimensions, status.fps);
        let actual = status.dimensions.unwrap();
        sup.set_playing(false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while sup.status().state != SourceState::Paused {
            assert!(Instant::now() < deadline, "did not stop: {:?}", sup.status());
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(500));
        let out = std::process::Command::new(&arv_tool)
            .args(["-n", &dev.id, "control", "BinningHorizontal", "BinningVertical", "Width", "Height"])
            .output()
            .unwrap();
        let readings = aravis::parse_control(&String::from_utf8_lossy(&out.stdout));
        eprintln!("camera after run: {readings:?}");
        aravis::check_binning(&readings, binning).unwrap();
        let region = aravis::full_region_writes(&readings).unwrap();
        let value = |name: &str| {
            readings.iter().find_map(|(n, r)| match r {
                aravis::Reading::Number(range) if n == name => Some(range.value as u16),
                _ => None,
            })
        };
        eprintln!("full region would be {region:?}");
        assert_eq!(actual, (value("Width").unwrap(), value("Height").unwrap()), "frames must match the camera region");
    }
    sup.shutdown();
}
