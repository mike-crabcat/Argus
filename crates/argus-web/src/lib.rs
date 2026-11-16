//! The Argus browser app.
//!
//! A camera grid: each tile opens a WebSocket to `/api/live/{camera}/sub`,
//! decodes with WebCodecs and uploads every decoded `VideoFrame` straight
//! into a wgpu texture (`copy_external_image_to_texture`), which egui draws.
//! Late frames are dropped, so a tile always shows the newest frame.
//! Double-click a tile for that camera's main stream, full window.
//!
//! The browser-only parts are compiled for `wasm32` only; the layout and
//! statistics helpers below are plain Rust and tested natively.

pub mod layout;
pub mod stats;

#[cfg(target_arch = "wasm32")]
mod web;

/// A camera as `/api/cameras` describes it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Camera {
    pub name: String,
    pub streams: Vec<StreamInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct StreamInfo {
    pub name: String,
    pub live: bool,
    pub codec: Option<String>,
    pub width: Option<u16>,
    pub height: Option<u16>,
}

#[derive(Debug, serde::Deserialize)]
pub struct CameraList {
    pub cameras: Vec<Camera>,
}

impl Camera {
    /// The stream a grid tile shows: `sub` if there is one, else the first.
    pub fn grid_stream(&self) -> Option<&str> {
        self.streams
            .iter()
            .find(|s| s.name == "sub")
            .or(self.streams.first())
            .map(|s| s.name.as_str())
    }

    /// The stream for the full-window view: `main` if there is one.
    pub fn focus_stream(&self) -> Option<&str> {
        self.streams
            .iter()
            .find(|s| s.name == "main")
            .or(self.streams.first())
            .map(|s| s.name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camera_list_and_picks_streams() {
        let list: CameraList = serde_json::from_str(
            r#"{"cameras":[{"name":"door","streams":[
                {"audio":"mp4a.40.2","codec":"avc1.640033","height":1920,"live":true,"name":"main","width":2560},
                {"audio":null,"codec":null,"height":null,"live":false,"name":"sub","width":null}]}]}"#,
        )
        .unwrap();
        let c = &list.cameras[0];
        assert_eq!(c.grid_stream(), Some("sub"));
        assert_eq!(c.focus_stream(), Some("main"));
        assert_eq!(c.streams[0].width, Some(2560));
        let only_main = Camera {
            name: "x".into(),
            streams: vec![c.streams[0].clone()],
        };
        assert_eq!(only_main.grid_stream(), Some("main"));
    }
}
