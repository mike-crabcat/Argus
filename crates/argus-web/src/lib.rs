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

/// A URL query string (`?a=1&token=x`) without parameter `name`, for
/// taking the token out of the address bar. Returns "" if nothing is left.
pub fn without_param(search: &str, name: &str) -> String {
    let kept: Vec<&str> = search
        .trim_start_matches('?')
        .split('&')
        .filter(|p| !p.is_empty() && p.split('=').next() != Some(name))
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

/// The address a phone opens to sign in: the server's public address (or
/// this page's origin) with the device token as `?token=`.
pub fn pairing_url(public_url: Option<&str>, origin: &str, token: &str) -> String {
    let base = public_url
        .filter(|u| !u.trim().is_empty())
        .unwrap_or(origin);
    format!("{}/?token={token}", base.trim().trim_end_matches('/'))
}

/// Whether a URL points at this computer only, so a phone couldn't open it.
pub fn is_local_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host.starts_with("127.")
        || host == "::1"
        || host == "0.0.0.0"
}

/// A QR code for `text` as rows of modules (true = dark), without the
/// quiet zone. Returns (width, modules).
pub fn qr_modules(text: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let colors = code.to_colors();
    Some((
        code.width(),
        colors
            .into_iter()
            .map(|c| c == qrcode::Color::Dark)
            .collect(),
    ))
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn ago(now_ms: f64, then_ms: f64) -> String {
    let s = ((now_ms - then_ms) / 1000.0).max(0.0);
    if s < 90.0 {
        "just now".into()
    } else if s < 3600.0 {
        format!("{} min ago", (s / 60.0).round())
    } else if s < 86_400.0 {
        format!("{} h ago", (s / 3600.0).round())
    } else {
        let d = (s / 86_400.0).round();
        format!("{d} day{} ago", if d == 1.0 { "" } else { "s" })
    }
}

/// A device as `/api/devices` lists it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub created_ms: i64,
    pub last_used_ms: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
pub struct DeviceList {
    pub devices: Vec<Device>,
}

/// `/api/info`: the caller's role and the server's public address.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ServerInfo {
    pub role: String,
    pub public_url: Option<String>,
}

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
    fn strips_only_the_token_parameter() {
        assert_eq!(without_param("?token=abc", "token"), "");
        assert_eq!(without_param("?a=1&token=abc&b=2", "token"), "?a=1&b=2");
        assert_eq!(without_param("?tokens=1&token", "token"), "?tokens=1");
        assert_eq!(without_param("", "token"), "");
        assert_eq!(without_param("?a=1", "token"), "?a=1");
    }

    #[test]
    fn pairing_urls() {
        let t = "ab12";
        assert_eq!(
            pairing_url(Some("https://nvr.ts.net:8444/"), "http://127.0.0.1:8080", t),
            "https://nvr.ts.net:8444/?token=ab12"
        );
        assert_eq!(
            pairing_url(None, "http://192.168.1.5:8080", t),
            "http://192.168.1.5:8080/?token=ab12"
        );
        assert_eq!(
            pairing_url(Some(" "), "http://x", t),
            "http://x/?token=ab12"
        );
    }

    #[test]
    fn local_urls() {
        for u in [
            "http://localhost:8080",
            "http://127.0.0.1:8080/",
            "http://[::1]:8080",
            "https://LOCALHOST",
            "http://app.localhost",
            "http://0.0.0.0:80",
        ] {
            assert!(is_local_url(u), "{u}");
        }
        for u in [
            "https://mike-workstation.tail94e30e.ts.net:8444",
            "http://192.168.1.5:8080",
            "http://localhost.example.com",
        ] {
            assert!(!is_local_url(u), "{u}");
        }
    }

    #[test]
    fn qr_code_for_a_pairing_url() {
        let url = pairing_url(Some("https://nvr.example.ts.net:8444"), "", &"f".repeat(64));
        let (w, m) = qr_modules(&url).unwrap();
        assert_eq!(m.len(), w * w);
        assert!((21..=57).contains(&w), "version too large: {w}");
        // Finder pattern: the top-left corner is dark, ringed by light.
        assert!(m[0] && m[6] && !m[7] && m[6 * w]);
    }

    #[test]
    fn relative_times() {
        assert_eq!(ago(100_000.0, 99_000.0), "just now");
        assert_eq!(ago(10.0 * 60_000.0, 0.0), "10 min ago");
        assert_eq!(ago(3.0 * 3_600_000.0, 0.0), "3 h ago");
        assert_eq!(ago(86_400_000.0, 0.0), "1 day ago");
        assert_eq!(ago(5.0 * 86_400_000.0, 0.0), "5 days ago");
    }

    #[test]
    fn parses_device_list_and_info() {
        let list: DeviceList = serde_json::from_str(
            r#"{"devices":[{"id":"a1","name":"iPhone","created_ms":1,"last_used_ms":null}]}"#,
        )
        .unwrap();
        assert_eq!(list.devices[0].name, "iPhone");
        let info: ServerInfo =
            serde_json::from_str(r#"{"role":"admin","public_url":null,"version":"dev"}"#).unwrap();
        assert_eq!(info.role, "admin");
    }

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
