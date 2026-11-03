use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use livestream_core::hub::FrameHub;
use livestream_core::source::{CaptureConfig, Mode, SourceState, Supervisor, SupervisorOptions};

fn fake_jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 8];
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&[1, 1, 0x11, 0]);
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 0x3F, 0]);
    out.extend_from_slice(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD9]);
    out
}

fn fixture(name: &str, frame: &[u8]) -> PathBuf {
    let dir = env::temp_dir().join(format!("livestream-sup-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("frame.jpg");
    fs::write(&path, frame).unwrap();
    path
}

/// Liveview -> endless producer, stills -> producer exiting after one frame.
fn factory(liveview_frame: PathBuf, stills_frame: PathBuf) -> livestream_core::source::CommandFactory {
    Box::new(move |config: &CaptureConfig| {
        let mut command = Command::new("/bin/sh");
        match config.mode {
            Mode::Liveview => command
                .arg("-c")
                .arg("trap 'exit 0' INT; while :; do cat \"$0\"; sleep 0.02; done")
                .arg(&liveview_frame),
            Mode::Stills => command.arg("-c").arg("cat \"$0\"").arg(&stills_frame),
        };
        Ok(command)
    })
}

fn options() -> SupervisorOptions {
    SupervisorOptions {
        restart_delay: Duration::from_millis(50),
        stop_grace: Duration::from_secs(2),
    }
}

fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn play_pause_and_mode_switch() {
    let hub = Arc::new(FrameHub::new());
    let live = fixture("live", &fake_jpeg(1056, 704));
    let still = fixture("still", &fake_jpeg(5184, 3456));
    let sup = Supervisor::new(hub.clone(), factory(live, still), None, options());

    assert_eq!(sup.status().state, SourceState::Paused);
    assert!(hub.is_paused());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(hub.latest().0, 0, "nothing produced while paused");

    sup.set_playing(true);
    assert!(!hub.is_paused());
    eventually("streaming", || sup.status().state == SourceState::Streaming && sup.status().frames >= 3);
    assert_eq!(sup.status().dimensions, Some((1056, 704)));
    assert!(sup.status().fps > 0.0);

    sup.set_playing(false);
    eventually("paused", || sup.status().state == SourceState::Paused);
    assert!(hub.is_paused());
    let seq = hub.latest().0;
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(hub.latest().0, seq, "no frames after pause");
    assert_eq!(sup.status().spawns, 1);

    sup.set_config(CaptureConfig { mode: Mode::Stills, interval: 1, image_format: "L".into() }).unwrap();
    assert_eq!(sup.status().spawns, 1, "config change while paused does not spawn");
    sup.set_playing(true);
    eventually("stills frames", || sup.status().dimensions == Some((5184, 3456)));
    eventually("respawn after producer exit", || sup.status().spawns >= 3);

    sup.shutdown();
}

#[test]
fn config_change_while_playing_restarts_producer() {
    let hub = Arc::new(FrameHub::new());
    let live = fixture("restart-live", &fake_jpeg(10, 10));
    let still = fixture("restart-still", &fake_jpeg(20, 20));
    let sup = Supervisor::new(hub, factory(live, still), None, options());
    sup.set_playing(true);
    eventually("liveview", || sup.status().dimensions == Some((10, 10)));
    sup.set_config(CaptureConfig { mode: Mode::Stills, interval: 2, image_format: "M".into() }).unwrap();
    eventually("stills", || sup.status().dimensions == Some((20, 20)));
    assert!(sup.set_config(CaptureConfig { mode: Mode::Stills, interval: 1, image_format: "RAW".into() }).is_err());
    assert_eq!(sup.status().config.image_format, "M");
}

#[test]
fn missing_producer_reports_error_and_retries() {
    let hub = Arc::new(FrameHub::new());
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = attempts.clone();
    let sup = Supervisor::new(
        hub,
        Box::new(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("gphoto2 not found".into())
        }),
        None,
        options(),
    );
    sup.set_playing(true);
    eventually("retries", || attempts.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    let status = sup.status();
    assert_eq!(status.state, SourceState::Retrying);
    assert_eq!(status.last_error.as_deref(), Some("gphoto2 not found"));
}
