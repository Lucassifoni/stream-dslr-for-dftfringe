//! Capture configuration, gphoto2 command building, and the process supervisor.
//!
//! The [`Supervisor`] owns one background thread that spawns the producer
//! (gphoto2) while playing, splits its stdout into JPEG frames, publishes
//! them to the [`FrameHub`], and respawns it after unexpected exits. Every
//! user action (play/pause, config change) bumps a generation counter and
//! interrupts the current child with SIGINT, which gphoto2 handles by
//! leaving liveview / finishing the capture cleanly; SIGKILL follows after a
//! grace period.

use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::hub::FrameHub;
use crate::jpeg::{self, JpegSplitter};

/// Canon JPEG-only `imageformat` choices; RAW variants would put CR2 data on stdout.
pub const JPEG_FORMATS: [&str; 6] = ["L", "cL", "M", "cM", "S", "cS"];

/// OpenCV's FFmpeg backend gives up on a read after ~30 s, so keep stills well below.
pub const MAX_STILLS_INTERVAL: u32 = 20;

/// Locations checked after `PATH`, since macOS GUI apps do not inherit the shell PATH.
pub const GPHOTO2_FALLBACKS: [&str; 4] = [
    "/opt/homebrew/bin/gphoto2",
    "/usr/local/bin/gphoto2",
    "/opt/local/bin/gphoto2",
    "/usr/bin/gphoto2",
];

const RECENT_FRAMES: usize = 10;
const STDERR_LINES: usize = 20;
const READ_CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Preview frames via `--capture-movie` (fast, ~1056x704 on EOS, no shutter).
    Liveview,
    /// Full captures via `--capture-image-and-download` (fires the shutter each frame).
    Stills,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureConfig {
    pub mode: Mode,
    /// Stills only: seconds between captures, whole seconds as gphoto2 requires.
    pub interval: u32,
    /// Stills only: one of [`JPEG_FORMATS`].
    pub image_format: String,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Liveview,
            interval: 1,
            image_format: "L".into(),
        }
    }
}

impl CaptureConfig {
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

/// First executable `gphoto2` in `path_var`, then in `fallbacks`.
pub fn find_gphoto2(path_var: Option<OsString>, fallbacks: &[&str]) -> Option<PathBuf> {
    let from_path = path_var
        .map(|p| env::split_paths(&p).map(|dir| dir.join("gphoto2")).collect::<Vec<_>>())
        .unwrap_or_default();
    from_path
        .into_iter()
        .chain(fallbacks.iter().map(PathBuf::from))
        .find(|p| is_executable(p))
}

/// [`find_gphoto2`] against the process `PATH` and [`GPHOTO2_FALLBACKS`].
pub fn locate_gphoto2() -> Option<PathBuf> {
    find_gphoto2(env::var_os("PATH"), &GPHOTO2_FALLBACKS)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Build the gphoto2 command for `config`, failing with an actionable message if absent.
pub fn gphoto2_command(config: &CaptureConfig) -> Result<Command, String> {
    let binary = locate_gphoto2().ok_or_else(|| {
        format!(
            "gphoto2 not found in PATH or {} — install it with `{}`",
            GPHOTO2_FALLBACKS.join(", "),
            install_hint(env::consts::OS)
        )
    })?;
    let mut command = Command::new(binary);
    command.args(config.gphoto2_args());
    Ok(command)
}

/// Package manager command installing gphoto2 on `os` (as in [`std::env::consts::OS`]).
pub fn install_hint(os: &str) -> &'static str {
    match os {
        "macos" => "brew install gphoto2",
        "linux" => "sudo apt install gphoto2",
        _ => "install gphoto2 with your package manager",
    }
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
        let _ = Command::new(argv[0])
            .args(&argv[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
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
/// Runs before every spawn (e.g. [`release_camera_claimers`]).
pub type Hook = Box<dyn Fn() + Send + Sync>;

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
            hook();
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
    use std::fs;

    fn stills(interval: u32, format: &str) -> CaptureConfig {
        CaptureConfig {
            mode: Mode::Stills,
            interval,
            image_format: format.into(),
        }
    }

    #[test]
    fn liveview_args() {
        assert_eq!(CaptureConfig::default().gphoto2_args(), ["--capture-movie", "--stdout"]);
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
        assert!(CaptureConfig::default().validate().is_ok());
        assert!(stills(1, "L").validate().is_ok());
        assert!(stills(MAX_STILLS_INTERVAL, "cS").validate().is_ok());
        assert!(stills(1, "RAW").validate().is_err());
        assert!(stills(1, "RAW + L").validate().is_err());
        assert!(stills(0, "L").validate().is_err());
        assert!(stills(MAX_STILLS_INTERVAL + 1, "L").validate().is_err());
    }

    #[test]
    fn find_gphoto2_prefers_path_then_fallbacks() {
        use std::os::unix::fs::PermissionsExt;
        let root = env::temp_dir().join(format!("livestream-find-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let fallback = b.join("gphoto2");
        fs::write(&fallback, "").unwrap();
        fs::set_permissions(&fallback, fs::Permissions::from_mode(0o755)).unwrap();
        let fallback_str = fallback.to_str().unwrap();
        let path = env::join_paths([&a]).unwrap();

        assert_eq!(find_gphoto2(Some(path.clone()), &[fallback_str]), Some(fallback.clone()));

        let not_exec = a.join("gphoto2");
        fs::write(&not_exec, "").unwrap();
        assert_eq!(find_gphoto2(Some(path.clone()), &[fallback_str]), Some(fallback.clone()));

        fs::set_permissions(&not_exec, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_gphoto2(Some(path), &[fallback_str]), Some(not_exec));
        assert_eq!(find_gphoto2(None, &["/nonexistent/gphoto2"]), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn per_os_camera_claimers_and_hints() {
        assert_eq!(camera_claimer_commands("macos"), vec![vec!["/usr/bin/killall", "PTPCamera"]]);
        assert!(camera_claimer_commands("linux").iter().all(|c| c[0] == "pkill"));
        assert!(camera_claimer_commands("windows").is_empty());
        assert_eq!(install_hint("macos"), "brew install gphoto2");
        assert_eq!(install_hint("linux"), "sudo apt install gphoto2");
    }

    #[test]
    fn fps_from_timestamps() {
        let t0 = Instant::now();
        let recent: VecDeque<_> = (0..5).map(|i| t0 + Duration::from_millis(100 * i)).collect();
        assert!((fps(&recent) - 10.0).abs() < 1e-6);
        assert_eq!(fps(&VecDeque::from([t0])), 0.0);
    }
}
