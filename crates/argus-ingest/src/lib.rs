//! Camera ingest. Each source yields codec configs and untouched frames;
//! nothing here decodes video.

pub mod flv;
pub mod rtsp;

use anyhow::{Result, bail};
use argus_media::{AudioConfig, VideoConfig};
use bytes::Bytes;
use tokio::sync::mpsc;

/// A frame as it comes off the wire, before wall-clock mapping.
#[derive(Clone, Debug)]
pub struct RawFrame {
    pub pts_90k: i64,
    pub keyframe: bool,
    /// 4-byte length-prefixed NAL units.
    pub data: Bytes,
}

/// An audio frame as it comes off the wire. `pts_90k` is on the same clock
/// as the video's, so both map to wall time through one anchor.
#[derive(Clone, Debug)]
pub struct RawAudio {
    pub pts_90k: i64,
    /// Raw AAC (no ADTS header).
    pub data: Bytes,
}

#[derive(Clone, Debug)]
pub enum SourceEvent {
    Config(VideoConfig),
    Frame(RawFrame),
    AudioConfig(AudioConfig),
    Audio(RawAudio),
}

/// Runs one connection to a camera stream until it ends or fails. The caller
/// owns reconnects. The URL scheme picks the protocol:
/// `rtsp://` (retina) or `http(s)://` (HTTP-FLV, e.g. Reolink's `/flv?...`).
pub async fn run(source: &str, tx: mpsc::Sender<SourceEvent>) -> Result<()> {
    let url = url::Url::parse(source)?;
    match url.scheme() {
        "rtsp" | "rtsps" => rtsp::run(url, tx).await,
        "http" | "https" => flv::run(url, tx).await,
        s => bail!("unsupported source scheme {s:?} (use rtsp:// or http:// for FLV)"),
    }
}

/// Strips credentials from a URL for logging.
pub fn redact(source: &str) -> String {
    match url::Url::parse(source) {
        Ok(mut u) => {
            let _ = u.set_password(None);
            let _ = u.set_username("");
            let q: Vec<(String, String)> = u
                .query_pairs()
                .map(|(k, v)| {
                    let secret = matches!(k.as_ref(), "password" | "pass" | "pwd" | "token");
                    (
                        k.into_owned(),
                        if secret { "***".into() } else { v.into_owned() },
                    )
                })
                .collect();
            if !q.is_empty() {
                u.query_pairs_mut().clear().extend_pairs(q);
            }
            u.to_string()
        }
        Err(_) => "<invalid url>".into(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn redacts_userinfo_and_password_params() {
        let r = super::redact("http://10.0.0.5/flv?port=1935&user=admin&password=hunter2");
        assert!(!r.contains("hunter2"), "{r}");
        let r = super::redact("rtsp://admin:hunter2@10.0.0.5:554/h264Preview_01_main");
        assert!(!r.contains("hunter2") && !r.contains("admin"), "{r}");
    }

    #[test]
    fn redacts_reolink_flv_urls_from_config() {
        let r = super::redact(
            "http://192.168.100.193/flv?port=1935&app=bcs&stream=channel0_main.bcs&user=homeassistant&password=s3cr%26t%3D",
        );
        assert!(!r.contains("s3cr"), "{r}");
        assert!(r.ends_with("password=***"), "{r}");
        assert!(r.contains("stream=channel0_main.bcs"), "{r}");
    }
}
