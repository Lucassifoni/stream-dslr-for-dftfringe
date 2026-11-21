//! Capture configuration and the producer process supervisor.
//!
//! The [`Supervisor`] owns one background thread that spawns the producer
//! (gphoto2 for DSLRs, ffmpeg for UVC cameras, gst-launch for GenICam cameras) while playing, splits its stdout
//! into JPEG frames, publishes them to the [`FrameHub`], and respawns it after
//! unexpected exits. Every user action (play/pause, config change) bumps a
//! generation counter and interrupts the current child with SIGINT, which
//! both producers handle by stopping cleanly; SIGKILL follows after a grace
//! period.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::aravis::{self, AravisConfig};
use crate::camera::{self, CameraConfig};
use crate::dslr::{self, DslrConfig};
use crate::hub::FrameHub;
use crate::jpeg::{self, JpegSplitter};

const RECENT_FRAMES: usize = 10;
const STDERR_LINES: usize = 20;
const READ_CHUNK: usize = 64 * 1024;

/// What to stream from.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptureConfig {
    /// A gphoto2-supported camera (liveview or stills).
    Dslr(DslrConfig),
    /// A UVC / V4L2 / AVFoundation camera through ffmpeg.
    Camera(CameraConfig),
    /// A GenICam (USB3 Vision / GigE Vision) camera through Aravis and GStreamer.
    Aravis(AravisConfig),
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig::Dslr(DslrConfig::default())
    }
}

impl CaptureConfig {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            CaptureConfig::Dslr(c) => c.validate(),
            CaptureConfig::Camera(c) => c.validate(),
            CaptureConfig::Aravis(c) => c.validate(),
        }
    }
}

/// The producer command for `config` (the default [`CommandFactory`]).
pub fn producer_command(config: &CaptureConfig) -> Result<Command, String> {
    match config {
        CaptureConfig::Dslr(c) => dslr::gphoto2_command(c),
        CaptureConfig::Camera(c) => camera::ffmpeg_command(c),
        CaptureConfig::Aravis(c) => aravis::gst_command(c),
    }
}

/// Pre-spawn step for `config` (the default [`Hook`]): DSLRs need desktop
/// services that claim PTP cameras stopped first.
pub fn prepare(config: &CaptureConfig) {
    if let CaptureConfig::Dslr(_) = config {
        dslr::release_camera_claimers();
    }
}

fn send_signal(pid: u32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceState {
    Paused,
    Starting,
    Streaming,
    /// The producer failed or exited on its own; it will be respawned.
    Retrying,
}

#[derive(Clone, Debug)]
pub struct SourceStatus {
    pub playing: bool,
    pub config: CaptureConfig,
    pub state: SourceState,
    /// Frames received during the current run.
    pub frames: u64,
    pub fps: f64,
    pub dimensions: Option<(u16, u16)>,
    pub last_frame_age: Option<Duration>,
    pub last_error: Option<String>,
    pub spawns: u64,
}

/// Builds the producer command for a configuration.
pub type CommandFactory = Box<dyn Fn(&CaptureConfig) -> Result<Command, String> + Send + Sync>;
/// Runs before every spawn with the config about to start (e.g. [`prepare`]).
pub type Hook = Box<dyn Fn(&CaptureConfig) + Send + Sync>;

#[derive(Clone, Copy, Debug)]
pub struct SupervisorOptions {
    pub restart_delay: Duration,
    /// Time allowed after SIGINT before SIGKILL.
    pub stop_grace: Duration,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            restart_delay: Duration::from_secs(2),
            stop_grace: Duration::from_secs(5),
        }
    }
}

struct Control {
    playing: bool,
    shutdown: bool,
    config: CaptureConfig,
    generation: u64,
    child: Option<u32>,
    state: SourceState,
    frames: u64,
    recent: VecDeque<Instant>,
    dimensions: Option<(u16, u16)>,
    last_error: Option<String>,
    spawns: u64,
}

struct Inner {
    hub: Arc<FrameHub>,
    factory: CommandFactory,
    before_start: Option<Hook>,
    options: SupervisorOptions,
    ctl: Mutex<Control>,
    cond: Condvar,
}

/// Runs the producer while playing; starts paused.
pub struct Supervisor {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Supervisor {
    pub fn new(
        hub: Arc<FrameHub>,
        factory: CommandFactory,
        before_start: Option<Hook>,
        options: SupervisorOptions,
    ) -> Self {
        hub.set_paused(true);
        let inner = Arc::new(Inner {
            hub,
            factory,
            before_start,
            options,
            ctl: Mutex::new(Control {
                playing: false,
                shutdown: false,
                config: CaptureConfig::default(),
                generation: 0,
                child: None,
                state: SourceState::Paused,
                frames: 0,
                recent: VecDeque::new(),
                dimensions: None,
                last_error: None,
                spawns: 0,
            }),
            cond: Condvar::new(),
        });
        let runner = inner.clone();
        let thread = thread::Builder::new()
            .name("stream-source".into())
            .spawn(move || runner.run())
            .expect("spawn supervisor thread");
        Self {
            inner,
            thread: Mutex::new(Some(thread)),
        }
    }

    pub fn set_playing(&self, playing: bool) {
        self.inner.update(|c| {
            let changed = c.playing != playing;
            c.playing = playing;
            changed
        });
    }

    /// Validate and apply `config`; restarts the producer if it is running.
    pub fn set_config(&self, config: CaptureConfig) -> Result<(), String> {
        config.validate()?;
        self.inner.update(|c| {
            let changed = c.config != config;
            c.config = config;
            changed && c.playing
        });
        Ok(())
    }

    pub fn status(&self) -> SourceStatus {
        let c = self.inner.lock();
        let streaming = c.state == SourceState::Streaming;
        SourceStatus {
            playing: c.playing,
            config: c.config.clone(),
            state: c.state,
            frames: c.frames,
            fps: if streaming { fps(&c.recent) } else { 0.0 },
            dimensions: c.dimensions,
            last_frame_age: c.recent.back().map(Instant::elapsed),
            last_error: c.last_error.clone(),
            spawns: c.spawns,
        }
    }

    /// Stop the producer and the supervisor thread. Idempotent.
    pub fn shutdown(&self) {
        self.inner.update(|c| {
            c.shutdown = true;
            c.playing = false;
            true
        });
        let handle = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn fps(recent: &VecDeque<Instant>) -> f64 {
    match (recent.front(), recent.back()) {
        (Some(first), Some(last)) if recent.len() >= 2 && last > first => {
            (recent.len() - 1) as f64 / last.duration_since(*first).as_secs_f64()
        }
        _ => 0.0,
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Control> {
        self.ctl.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Apply `change`; when it returns true, interrupt the running child so the new state takes effect.
    fn update(self: &Arc<Self>, change: impl FnOnce(&mut Control) -> bool) {
        let (interrupt, child, playing) = {
            let mut c = self.lock();
            let interrupt = change(&mut c);
            if interrupt {
                c.generation += 1;
            }
            (interrupt, c.child, c.playing)
        };
        self.hub.set_paused(!playing);
        if let (true, Some(pid)) = (interrupt, child) {
            self.stop_child(pid);
        }
        self.cond.notify_all();
    }

    fn stop_child(self: &Arc<Self>, pid: u32) {
        send_signal(pid, "-INT");
        let me = self.clone();
        let grace = self.options.stop_grace;
        let _ = thread::Builder::new().name("stop-escalation".into()).spawn(move || {
            thread::sleep(grace);
            if me.lock().child == Some(pid) {
                send_signal(pid, "-KILL");
            }
        });
    }

    fn run(self: Arc<Self>) {
        while let Some((generation, config)) = self.wait_for_play() {
            self.run_once(generation, &config);
            self.wait_before_restart(generation);
        }
    }

    fn wait_for_play(&self) -> Option<(u64, CaptureConfig)> {
        let mut c = self.lock();
        loop {
            if c.shutdown {
                return None;
            }
            if c.playing {
                return Some((c.generation, c.config.clone()));
            }
            c.state = SourceState::Paused;
            c = self.cond.wait(c).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn wait_before_restart(&self, generation: u64) {
        let c = self.lock();
        let _ = self
            .cond
            .wait_timeout_while(c, self.options.restart_delay, |c| !c.shutdown && c.generation == generation);
    }

    fn current(&self, generation: u64) -> bool {
        let c = self.lock();
        c.generation == generation && c.playing && !c.shutdown
    }

    fn run_once(self: &Arc<Self>, generation: u64, config: &CaptureConfig) {
        {
            let mut c = self.lock();
            c.state = SourceState::Starting;
            c.frames = 0;
            c.recent.clear();
            c.dimensions = None;
        }
        if let Some(hook) = &self.before_start {
            hook(config);
        }
        let spawned = (self.factory)(config).and_then(|mut command| {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("cannot start {:?}: {e}", command.get_program()))
        });
        let mut child = match spawned {
            Ok(child) => child,
            Err(message) => return self.fail(generation, message),
        };
        let pid = child.id();
        let stale = {
            let mut c = self.lock();
            c.child = Some(pid);
            c.spawns += 1;
            c.generation != generation
        };
        if stale {
            self.stop_child(pid);
        }

        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let drain = child.stderr.take().map(|stderr| {
            let tail = tail.clone();
            thread::spawn(move || collect_lines(stderr, &tail))
        });
        if let Some(mut stdout) = child.stdout.take() {
            self.pump(generation, &mut stdout);
        }
        let status = child.wait();
        if let Some(drain) = drain {
            let _ = drain.join();
        }
        let tail: Vec<String> = tail.lock().unwrap_or_else(PoisonError::into_inner).drain(..).collect();
        self.on_exit(generation, status, &tail);
    }

    fn pump(&self, generation: u64, stdout: &mut impl Read) {
        let mut splitter = JpegSplitter::default();
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => splitter.feed(&buf[..n]).into_iter().for_each(|f| self.on_frame(generation, f)),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }

    fn on_frame(&self, generation: u64, frame: Vec<u8>) {
        let dimensions = jpeg::dimensions(&frame);
        {
            let mut c = self.lock();
            c.frames += 1;
            c.recent.push_back(Instant::now());
            if c.recent.len() > RECENT_FRAMES {
                c.recent.pop_front();
            }
            c.dimensions = dimensions.or(c.dimensions);
            if c.generation == generation && c.playing {
                c.state = SourceState::Streaming;
                c.last_error = None;
            }
        }
        self.hub.publish(frame);
    }

    fn on_exit(&self, generation: u64, status: std::io::Result<ExitStatus>, stderr: &[String]) {
        let unexpected = self.current(generation);
        let mut c = self.lock();
        c.child = None;
        if unexpected {
            let code = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
            let detail = stderr.iter().rev().take(3).rev().cloned().collect::<Vec<_>>().join(" | ");
            c.state = SourceState::Retrying;
            c.last_error = Some(format!("producer exited ({code}) {detail}").trim_end().to_string());
        }
        drop(c);
        self.cond.notify_all();
    }

    fn fail(&self, generation: u64, message: String) {
        let unexpected = self.current(generation);
        let mut c = self.lock();
        if unexpected {
            c.state = SourceState::Retrying;
            c.last_error = Some(message);
        }
    }
}

fn collect_lines(stream: impl Read, sink: &Mutex<VecDeque<String>>) {
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let mut sink = sink.lock().unwrap_or_else(PoisonError::into_inner);
        sink.push_back(line);
        if sink.len() > STDERR_LINES {
            sink.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_dispatches_per_source() {
        assert!(CaptureConfig::default().validate().is_ok());
        let bad_dslr = DslrConfig { mode: dslr::Mode::Stills, interval: 0, ..DslrConfig::default() };
        assert!(CaptureConfig::Dslr(bad_dslr).validate().is_err());
        let bad_camera = CameraConfig {
            backend: camera::Backend::V4l2,
            device: String::new(),
            format: Some("MJPG".into()),
            width: 640,
            height: 480,
            rate: camera::FrameRate { num: 30, den: 1 },
        };
        assert!(CaptureConfig::Camera(bad_camera).validate().is_err());
    }

    #[test]
    fn fps_from_timestamps() {
        let t0 = Instant::now();
        let recent: VecDeque<_> = (0..5).map(|i| t0 + Duration::from_millis(100 * i)).collect();
        assert!((fps(&recent) - 10.0).abs() < 1e-6);
        assert_eq!(fps(&VecDeque::from([t0])), 0.0);
    }
}
