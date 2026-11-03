//! Tauri shell around `livestream-core`: starts the MJPEG server on
//! 127.0.0.1:8765, owns the gphoto2 supervisor, and exposes three commands
//! (`get_status`, `set_playing`, `set_config`) to the UI.

use std::sync::Arc;

use livestream_core::hub::FrameHub;
use livestream_core::server::{self, ServerOptions, STREAM_PATH};
use livestream_core::source::{
    self, CaptureConfig, Mode, SourceState, SourceStatus, Supervisor, SupervisorOptions, JPEG_FORMATS,
    MAX_STILLS_INTERVAL,
};
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDto {
    pub mode: ModeDto,
    pub interval: u32,
    pub image_format: String,
}

impl From<ConfigDto> for CaptureConfig {
    fn from(dto: ConfigDto) -> Self {
        CaptureConfig {
            mode: match dto.mode {
                ModeDto::Liveview => Mode::Liveview,
                ModeDto::Stills => Mode::Stills,
            },
            interval: dto.interval,
            image_format: dto.image_format,
        }
    }
}

impl From<&CaptureConfig> for ConfigDto {
    fn from(config: &CaptureConfig) -> Self {
        ConfigDto {
            mode: match config.mode {
                Mode::Liveview => ModeDto::Liveview,
                Mode::Stills => ModeDto::Stills,
            },
            interval: config.interval,
            image_format: config.image_format.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub playing: bool,
    pub install_hint: &'static str,
    pub config: ConfigDto,
    pub state: &'static str,
    pub frames: u64,
    pub fps: f64,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub last_frame_age_ms: Option<u64>,
    pub last_error: Option<String>,
    pub spawns: u64,
    pub clients: usize,
    pub stream_url: String,
    pub server_error: Option<String>,
    pub gphoto2: Option<String>,
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

struct AppState {
    hub: Arc<FrameHub>,
    supervisor: Supervisor,
    stream_url: String,
    server_error: Option<String>,
}

/// Pure assembly of the UI status from core status and app context.
pub fn status_dto(
    s: SourceStatus,
    clients: usize,
    stream_url: &str,
    server_error: Option<&str>,
    gphoto2: Option<String>,
) -> StatusDto {
    StatusDto {
        playing: s.playing,
        install_hint: source::install_hint(std::env::consts::OS),
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
        stream_url: stream_url.to_string(),
        server_error: server_error.map(str::to_string),
        gphoto2,
        jpeg_formats: JPEG_FORMATS.to_vec(),
        max_interval: MAX_STILLS_INTERVAL,
    }
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> StatusDto {
    status_dto(
        state.supervisor.status(),
        state.hub.clients(),
        &state.stream_url,
        state.server_error.as_deref(),
        source::locate_gphoto2().map(|p| p.display().to_string()),
    )
}

#[tauri::command]
fn set_playing(state: State<'_, AppState>, playing: bool) {
    state.supervisor.set_playing(playing);
}

#[tauri::command]
fn set_config(state: State<'_, AppState>, config: ConfigDto) -> Result<(), String> {
    state.supervisor.set_config(config.into())
}

pub fn run() {
    let hub = Arc::new(FrameHub::new());
    let server_error = server::spawn(hub.clone(), (STREAM_HOST, STREAM_PORT), ServerOptions::default())
        .err()
        .map(|e| format!("cannot listen on {STREAM_HOST}:{STREAM_PORT}: {e}"));
    let supervisor = Supervisor::new(
        hub.clone(),
        Box::new(source::gphoto2_command),
        Some(Box::new(source::release_camera_claimers)),
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
        .invoke_handler(tauri::generate_handler![get_status, set_playing, set_config])
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
    use std::time::Duration;

    #[test]
    fn config_dto_round_trip_and_json_shape() {
        let json = r#"{"mode":"stills","interval":3,"imageFormat":"M"}"#;
        let dto: ConfigDto = serde_json::from_str(json).unwrap();
        let config: CaptureConfig = dto.clone().into();
        assert_eq!(config.mode, Mode::Stills);
        assert_eq!(ConfigDto::from(&config), dto);
        assert!(serde_json::from_str::<ConfigDto>(r#"{"mode":"video","interval":1,"imageFormat":"L"}"#).is_err());
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
        let value = serde_json::to_value(status_dto(s, 2, "http://x/video_feed", None, None)).unwrap();
        assert_eq!(value["state"], "streaming");
        assert_eq!(value["config"]["mode"], "liveview");
        assert_eq!(value["config"]["imageFormat"], "L");
        assert_eq!(value["width"], 1056);
        assert_eq!(value["lastFrameAgeMs"], 42);
        assert_eq!(value["clients"], 2);
        assert_eq!(value["maxInterval"], MAX_STILLS_INTERVAL);
    }
}
