//! Tauri shell around `livestream-core`: starts the MJPEG server on
//! 127.0.0.1:8765, owns the producer supervisor, and exposes commands to the
//! UI: `get_status`, `set_playing`, `set_config`, `save_frame`, camera
//! discovery (`list_cameras`, `list_aravis`) and V4L2 controls
//! (`list_controls`, `set_control`).
//!
//! DTOs are the JSON contract with `ui/main.js`; conversions to and from the
//! core types are pure and tested here.

use std::env::consts::OS;
use std::sync::Arc;

use livestream_core::aravis::{self, AravisConfig, AravisDevice, Level, Range};
use livestream_core::avfoundation;
use livestream_core::camera::{Backend, CameraConfig, CameraDevice, FrameRate};
use livestream_core::dslr::{DslrConfig, Mode, JPEG_FORMATS, MAX_STILLS_INTERVAL};
use livestream_core::hub::FrameHub;
use livestream_core::server::{self, ServerOptions, STREAM_PATH};
use livestream_core::snapshot;
use livestream_core::source::{self, CaptureConfig, SourceState, SourceStatus, Supervisor, SupervisorOptions};
use livestream_core::tools;
use livestream_core::v4l2::{self, Control, ControlKind};
use serde::{Deserialize, Serialize};
use tauri::{Manager, RunEvent, State};

pub const STREAM_HOST: &str = "127.0.0.1";
pub const STREAM_PORT: u16 = 8765;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModeDto {
    Liveview,
    Stills,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateDto {
    pub num: u32,
    pub den: u32,
}

impl From<FrameRate> for RateDto {
    fn from(r: FrameRate) -> Self {
        RateDto { num: r.num, den: r.den }
    }
}

impl RateDto {
    fn to_rate(self) -> Result<FrameRate, String> {
        FrameRate::new(self.num, self.den).ok_or_else(|| format!("invalid frame rate {}/{}", self.num, self.den))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum LevelDto {
    Unchanged,
    Auto,
    Manual { value: f64 },
}

impl From<LevelDto> for Level {
    fn from(dto: LevelDto) -> Self {
        match dto {
            LevelDto::Unchanged => Level::Unchanged,
            LevelDto::Auto => Level::Auto,
            LevelDto::Manual { value } => Level::Manual(value),
        }
    }
}

impl From<Level> for LevelDto {
    fn from(level: Level) -> Self {
        match level {
            Level::Unchanged => LevelDto::Unchanged,
            Level::Auto => LevelDto::Auto,
            Level::Manual(value) => LevelDto::Manual { value },
        }
    }
}

/// Source configuration, tagged by `source`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum ConfigDto {
    Dslr {
        mode: ModeDto,
        interval: u32,
        image_format: String,
    },
    Camera {
        device: String,
        format: Option<String>,
        width: u32,
        height: u32,
        fps: RateDto,
    },
    Aravis {
        camera: String,
        pixel_format: String,
        binning: u32,
        fps: Option<RateDto>,
        exposure: LevelDto,
        gain: LevelDto,
    },
}

/// Core configuration for `dto` on `os` (which decides the camera backend).
pub fn to_config(dto: ConfigDto, os: &str) -> Result<CaptureConfig, String> {
    match dto {
        ConfigDto::Dslr { mode, interval, image_format } => Ok(CaptureConfig::Dslr(DslrConfig {
            mode: match mode {
                ModeDto::Liveview => Mode::Liveview,
                ModeDto::Stills => Mode::Stills,
            },
            interval,
            image_format,
        })),
        ConfigDto::Camera { device, format, width, height, fps } => Ok(CaptureConfig::Camera(CameraConfig {
            backend: Backend::for_os(os).ok_or_else(|| format!("cameras are not supported on {os}"))?,
            device,
            format,
            width,
            height,
            rate: fps.to_rate()?,
        })),
        ConfigDto::Aravis { camera, pixel_format, binning, fps, exposure, gain } => {
            Ok(CaptureConfig::Aravis(AravisConfig {
                camera,
                pixel_format,
                binning,
                rate: fps.map(RateDto::to_rate).transpose()?,
                exposure: exposure.into(),
                gain: gain.into(),
            }))
        }
    }
}

impl From<&CaptureConfig> for ConfigDto {
    fn from(config: &CaptureConfig) -> Self {
        match config {
            CaptureConfig::Dslr(c) => ConfigDto::Dslr {
                mode: match c.mode {
                    Mode::Liveview => ModeDto::Liveview,
                    Mode::Stills => ModeDto::Stills,
                },
                interval: c.interval,
                image_format: c.image_format.clone(),
            },
            CaptureConfig::Camera(c) => ConfigDto::Camera {
                device: c.device.clone(),
                format: c.format.clone(),
                width: c.width,
                height: c.height,
                fps: c.rate.into(),
            },
            CaptureConfig::Aravis(c) => ConfigDto::Aravis {
                camera: c.camera.clone(),
                pixel_format: c.pixel_format.clone(),
                binning: c.binning,
                fps: c.rate.map(RateDto::from),
                exposure: c.exposure.into(),
                gain: c.gain.into(),
            },
        }
    }
}

/// Paths of the external tools, `None` when not installed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsDto {
    pub gphoto2: Option<String>,
    pub ffmpeg: Option<String>,
    pub gst_launch: Option<String>,
    pub arv_tool: Option<String>,
}

impl ToolsDto {
    fn locate() -> Self {
        let find = |name: &str| tools::locate(name).map(|p| p.display().to_string());
        ToolsDto {
            gphoto2: find("gphoto2"),
            ffmpeg: find("ffmpeg"),
            gst_launch: find(aravis::GST_LAUNCH),
            arv_tool: find(aravis::ARV_TOOL),
        }
    }
}

/// Install instructions for whatever the selected source needs and is missing.
pub fn missing_tool(config: &CaptureConfig, t: &ToolsDto, os: &str) -> Option<String> {
    let hint = |name: &str| format!("{name} not found — install it with `{}`.", tools::install_hint(name, os));
    match config {
        CaptureConfig::Dslr(_) if t.gphoto2.is_none() => Some(hint("gphoto2")),
        CaptureConfig::Camera(_) if t.ffmpeg.is_none() => Some(hint("ffmpeg")),
        CaptureConfig::Aravis(_) if t.gst_launch.is_none() || t.arv_tool.is_none() => Some(aravis::missing_tools()),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub playing: bool,
    pub os: &'static str,
    /// `v4l2` / `avfoundation`, or `None` when cameras are unsupported on this OS.
    pub camera_backend: Option<&'static str>,
    pub config: ConfigDto,
    pub state: &'static str,
    pub frames: u64,
    pub fps: f64,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub last_frame_age_ms: Option<u64>,
    pub last_error: Option<String>,
    pub missing_tool: Option<String>,
    pub spawns: u64,
    pub clients: usize,
    pub has_frame: bool,
    pub stream_url: String,
    pub server_error: Option<String>,
    pub tools: ToolsDto,
    pub jpeg_formats: Vec<&'static str>,
    pub max_interval: u32,
}

pub fn state_name(state: SourceState) -> &'static str {
    match state {
        SourceState::Paused => "paused",
        SourceState::Starting => "starting",
        SourceState::Streaming => "streaming",
        SourceState::Retrying => "retrying",
    }
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::V4l2 => "v4l2",
        Backend::AvFoundation => "avfoundation",
    }
}

/// Pure assembly of the UI status from core status and app context.
pub fn status_dto(
    s: SourceStatus,
    clients: usize,
    has_frame: bool,
    stream_url: &str,
    server_error: Option<&str>,
    tools: ToolsDto,
    os: &'static str,
) -> StatusDto {
    StatusDto {
        playing: s.playing,
        os,
        camera_backend: Backend::for_os(os).map(backend_name),
        missing_tool: missing_tool(&s.config, &tools, os),
        config: ConfigDto::from(&s.config),
        state: state_name(s.state),
        frames: s.frames,
        fps: s.fps,
        width: s.dimensions.map(|d| d.0),
        height: s.dimensions.map(|d| d.1),
        last_frame_age_ms: s.last_frame_age.map(|d| d.as_millis() as u64),
        last_error: s.last_error,
        spawns: s.spawns,
        clients,
        has_frame,
        stream_url: stream_url.to_string(),
        server_error: server_error.map(str::to_string),
        tools,
        jpeg_formats: JPEG_FORMATS.to_vec(),
        max_interval: MAX_STILLS_INTERVAL,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraModeDto {
    pub format: Option<String>,
    pub format_label: String,
    pub width: u32,
    pub height: u32,
    pub rates: Vec<RateDto>,
    pub transcoded: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraDeviceDto {
    pub id: String,
    pub name: String,
    pub backend: &'static str,
    pub modes: Vec<CameraModeDto>,
    pub error: Option<String>,
}

impl From<CameraDevice> for CameraDeviceDto {
    fn from(d: CameraDevice) -> Self {
        CameraDeviceDto {
            id: d.id,
            name: d.name,
            backend: backend_name(d.backend),
            modes: d
                .modes
                .into_iter()
                .map(|m| CameraModeDto {
                    format: m.format,
                    format_label: m.format_label,
                    width: m.width,
                    height: m.height,
                    rates: m.rates.into_iter().map(RateDto::from).collect(),
                    transcoded: m.transcoded,
                })
                .collect(),
            error: d.error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct RangeDto {
    pub value: f64,
    pub min: f64,
    pub max: f64,
}

impl From<Range> for RangeDto {
    fn from(r: Range) -> Self {
        RangeDto { value: r.value, min: r.min, max: r.max }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AravisDeviceDto {
    pub id: String,
    pub address: String,
    pub pixel_formats: Vec<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub max_binning: u32,
    pub exposure: Option<RangeDto>,
    pub exposure_auto: bool,
    pub gain: Option<RangeDto>,
    pub gain_auto: bool,
    pub frame_rate: Option<RangeDto>,
    pub error: Option<String>,
}

impl From<AravisDevice> for AravisDeviceDto {
    fn from(d: AravisDevice) -> Self {
        AravisDeviceDto {
            id: d.id,
            address: d.address,
            pixel_formats: d.pixel_formats,
            width: d.width,
            height: d.height,
            max_binning: d.max_binning,
            exposure: d.exposure.map(RangeDto::from),
            exposure_auto: d.exposure_auto,
            gain: d.gain.map(RangeDto::from),
            gain_auto: d.gain_auto,
            frame_rate: d.frame_rate.map(RangeDto::from),
            error: d.error,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MenuItemDto {
    pub index: i32,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlDto {
    pub id: u32,
    pub name: String,
    /// `integer`, `boolean`, `menu`, `integerMenu` or `button`.
    pub kind: &'static str,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
    pub value: Option<i32>,
    pub inactive: bool,
    pub read_only: bool,
    pub menu: Vec<MenuItemDto>,
}

impl From<Control> for ControlDto {
    fn from(c: Control) -> Self {
        ControlDto {
            id: c.id,
            kind: match c.kind {
                ControlKind::Integer => "integer",
                ControlKind::Boolean => "boolean",
                ControlKind::Menu => "menu",
                ControlKind::IntegerMenu => "integerMenu",
                ControlKind::Button => "button",
            },
            min: c.minimum,
            max: c.maximum,
            step: c.step,
            default: c.default,
            value: c.value,
            inactive: c.inactive(),
            read_only: c.read_only(),
            menu: c.menu.iter().map(|m| MenuItemDto { index: m.index, label: m.label.clone() }).collect(),
            name: c.name,
        }
    }
}

fn controls_dto(controls: Vec<Control>) -> Vec<ControlDto> {
    controls.into_iter().map(ControlDto::from).collect()
}

struct AppState {
    hub: Arc<FrameHub>,
    supervisor: Supervisor,
    stream_url: String,
    server_error: Option<String>,
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> StatusDto {
    status_dto(
        state.supervisor.status(),
        state.hub.clients(),
        state.hub.latest().1.is_some(),
        &state.stream_url,
        state.server_error.as_deref(),
        ToolsDto::locate(),
        OS,
    )
}

#[tauri::command]
fn set_playing(state: State<'_, AppState>, playing: bool) {
    state.supervisor.set_playing(playing);
}

#[tauri::command]
fn set_config(state: State<'_, AppState>, config: ConfigDto) -> Result<(), String> {
    state.supervisor.set_config(to_config(config, OS)?)
}

/// Save the latest frame to the Desktop as `<stem>.jpg`; returns the written path.
#[tauri::command]
fn save_frame(app: tauri::AppHandle, state: State<'_, AppState>, stem: String) -> Result<String, String> {
    let frame = state.hub.latest().1.ok_or("no frame to save yet")?;
    let dir = app.path().desktop_dir().map_err(|e| format!("no Desktop folder: {e}"))?;
    snapshot::save_frame(&dir, &stem, &frame)
        .map(|path| path.display().to_string())
        .map_err(|e| format!("cannot save frame in {}: {e}", dir.display()))
}

/// UVC cameras: V4L2 nodes on Linux, AVFoundation devices (via ffmpeg) on macOS.
#[tauri::command(async)]
fn list_cameras() -> Result<Vec<CameraDeviceDto>, String> {
    let devices = match Backend::for_os(OS) {
        Some(Backend::V4l2) => v4l2::discover(),
        Some(Backend::AvFoundation) => {
            avfoundation::discover(&tools::locate("ffmpeg").ok_or_else(|| tools::not_found("ffmpeg"))?)
        }
        None => vec![],
    };
    Ok(devices.into_iter().map(CameraDeviceDto::from).collect())
}

/// GenICam cameras seen by Aravis.
#[tauri::command(async)]
fn list_aravis() -> Result<Vec<AravisDeviceDto>, String> {
    let arv_tool = tools::locate(aravis::ARV_TOOL).ok_or_else(aravis::missing_tools)?;
    Ok(aravis::discover(&arv_tool)?.into_iter().map(AravisDeviceDto::from).collect())
}

#[tauri::command(async)]
fn list_controls(device: String) -> Result<Vec<ControlDto>, String> {
    v4l2::controls_at(&device).map(controls_dto)
}

/// Set a V4L2 control live; returns the refreshed control list.
#[tauri::command(async)]
fn set_control(device: String, id: u32, value: i32) -> Result<Vec<ControlDto>, String> {
    v4l2::apply_control_at(&device, id, value).map(controls_dto)
}

pub fn run() {
    let hub = Arc::new(FrameHub::new());
    let server_error = server::spawn(hub.clone(), (STREAM_HOST, STREAM_PORT), ServerOptions::default())
        .err()
        .map(|e| format!("cannot listen on {STREAM_HOST}:{STREAM_PORT}: {e}"));
    let supervisor = Supervisor::new(
        hub.clone(),
        Box::new(source::producer_command),
        Some(Box::new(source::prepare)),
        SupervisorOptions::default(),
    );
    let state = AppState {
        hub,
        supervisor,
        stream_url: format!("http://{STREAM_HOST}:{STREAM_PORT}{STREAM_PATH}"),
        server_error,
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_status,
            set_playing,
            set_config,
            save_frame,
            list_cameras,
            list_aravis,
            list_controls,
            set_control
        ])
        .build(tauri::generate_context!())
        .expect("failed to build the Tauri application")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                let state = app.state::<AppState>();
                state.supervisor.shutdown();
                state.hub.close();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use livestream_core::camera::CameraMode;
    use std::time::Duration;

    fn round_trip(json: &str, os: &str) -> (CaptureConfig, serde_json::Value) {
        let dto: ConfigDto = serde_json::from_str(json).unwrap();
        let config = to_config(dto.clone(), os).unwrap();
        assert_eq!(ConfigDto::from(&config), dto);
        (config, serde_json::to_value(&dto).unwrap())
    }

    #[test]
    fn dslr_config_json() {
        let (config, value) = round_trip(r#"{"source":"dslr","mode":"stills","interval":3,"imageFormat":"M"}"#, "macos");
        assert!(matches!(config, CaptureConfig::Dslr(DslrConfig { mode: Mode::Stills, interval: 3, .. })));
        assert_eq!(value["imageFormat"], "M");
        assert!(serde_json::from_str::<ConfigDto>(r#"{"source":"dslr","mode":"video","interval":1,"imageFormat":"L"}"#).is_err());
        assert!(serde_json::from_str::<ConfigDto>(r#"{"mode":"stills","interval":1,"imageFormat":"L"}"#).is_err());
    }

    #[test]
    fn camera_config_json_picks_backend_from_os() {
        let json = r#"{"source":"camera","device":"/dev/video0","format":"MJPG","width":1280,"height":720,"fps":{"num":30,"den":1}}"#;
        let (config, _) = round_trip(json, "linux");
        let CaptureConfig::Camera(c) = config else { panic!("camera config expected") };
        assert_eq!(c.backend, Backend::V4l2);
        assert_eq!(c.rate, FrameRate::new(30, 1).unwrap());
        let CaptureConfig::Camera(c) = round_trip(json, "macos").0 else { panic!() };
        assert_eq!(c.backend, Backend::AvFoundation);
        assert!(to_config(serde_json::from_str(json).unwrap(), "windows").is_err());
        let zero = json.replace(r#""num":30"#, r#""num":0"#);
        assert!(to_config(serde_json::from_str(&zero).unwrap(), "linux").is_err());
    }

    #[test]
    fn aravis_config_json() {
        let json = r#"{"source":"aravis","camera":"Basler-1","pixelFormat":"Mono8","binning":2,"fps":null,
            "exposure":{"mode":"manual","value":5000.0},"gain":{"mode":"auto"}}"#;
        let (config, value) = round_trip(json, "linux");
        let CaptureConfig::Aravis(c) = config else { panic!("aravis config expected") };
        assert_eq!(c.exposure, Level::Manual(5000.0));
        assert_eq!(c.gain, Level::Auto);
        assert_eq!(c.rate, None);
        assert_eq!(value["pixelFormat"], "Mono8");
        assert_eq!(value["exposure"]["mode"], "manual");
    }

    #[test]
    fn missing_tool_depends_on_source() {
        let none = ToolsDto::default();
        let dslr = CaptureConfig::default();
        assert!(missing_tool(&dslr, &none, "linux").unwrap().contains("sudo apt install gphoto2"));
        let all = ToolsDto {
            gphoto2: Some("g".into()),
            ffmpeg: Some("f".into()),
            gst_launch: Some("s".into()),
            arv_tool: Some("a".into()),
        };
        assert_eq!(missing_tool(&dslr, &all, "linux"), None);
        let aravis = to_config(
            ConfigDto::Aravis {
                camera: "x".into(),
                pixel_format: "Mono8".into(),
                binning: 1,
                fps: None,
                exposure: LevelDto::Unchanged,
                gain: LevelDto::Unchanged,
            },
            "linux",
        )
        .unwrap();
        let no_arv = ToolsDto { arv_tool: None, ..all.clone() };
        assert!(missing_tool(&aravis, &no_arv, "linux").unwrap().contains(aravis::APT_PACKAGES));
        assert!(missing_tool(&dslr, &no_arv, "linux").is_none());
    }

    #[test]
    fn status_dto_shape() {
        let s = SourceStatus {
            playing: true,
            config: CaptureConfig::default(),
            state: SourceState::Streaming,
            frames: 7,
            fps: 15.0,
            dimensions: Some((1056, 704)),
            last_frame_age: Some(Duration::from_millis(42)),
            last_error: None,
            spawns: 1,
        };
        let value =
            serde_json::to_value(status_dto(s, 2, true, "http://x/video_feed", None, ToolsDto::default(), "linux")).unwrap();
        assert_eq!(value["state"], "streaming");
        assert_eq!(value["config"]["source"], "dslr");
        assert_eq!(value["config"]["mode"], "liveview");
        assert_eq!(value["cameraBackend"], "v4l2");
        assert_eq!(value["width"], 1056);
        assert_eq!(value["lastFrameAgeMs"], 42);
        assert_eq!(value["clients"], 2);
        assert_eq!(value["hasFrame"], true);
        assert_eq!(value["maxInterval"], MAX_STILLS_INTERVAL);
        assert!(value["missingTool"].as_str().unwrap().contains("gphoto2"));
        assert_eq!(value["tools"]["arvTool"], serde_json::Value::Null);
    }

    #[test]
    fn device_and_control_dtos() {
        let dev = CameraDeviceDto::from(CameraDevice {
            backend: Backend::V4l2,
            id: "/dev/video0".into(),
            name: "USB Camera".into(),
            modes: vec![CameraMode {
                format: Some("MJPG".into()),
                format_label: "Motion-JPEG".into(),
                width: 640,
                height: 480,
                rates: vec![FrameRate::new(30, 1).unwrap()],
                transcoded: false,
            }],
            error: None,
        });
        let value = serde_json::to_value(dev).unwrap();
        assert_eq!(value["backend"], "v4l2");
        assert_eq!(value["modes"][0]["formatLabel"], "Motion-JPEG");
        assert_eq!(value["modes"][0]["rates"][0]["num"], 30);

        let control = ControlDto::from(Control {
            id: 0x009a_0902,
            name: "Exposure Time, Absolute".into(),
            kind: ControlKind::Integer,
            minimum: 1,
            maximum: 5000,
            step: 1,
            default: 156,
            value: Some(156),
            flags: v4l2::CTRL_FLAG_INACTIVE,
            menu: vec![],
        });
        let value = serde_json::to_value(control).unwrap();
        assert_eq!(value["kind"], "integer");
        assert_eq!(value["inactive"], true);
        assert_eq!(value["readOnly"], false);
    }
}
