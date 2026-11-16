use anyhow::{Context, Result, anyhow, bail};
use argus_media::{Codec, VideoConfig};
use bytes::Bytes;
use futures::StreamExt;
use retina::client::{Credentials, PlayOptions, Session, SessionOptions, SetupOptions};
use retina::codec::{CodecItem, FrameFormat, ParametersRef, VideoParameters};
use tokio::sync::mpsc;
use url::Url;

use crate::{RawFrame, SourceEvent};

pub async fn run(mut url: Url, tx: mpsc::Sender<SourceEvent>) -> Result<()> {
    let creds = (!url.username().is_empty()).then(|| Credentials {
        username: decode(url.username()),
        password: decode(url.password().unwrap_or("")),
    });
    let _ = url.set_username("");
    let _ = url.set_password(None);

    let mut session = Session::describe(
        url,
        SessionOptions::default()
            .creds(creds)
            .user_agent("Argus NVR".into()),
    )
    .await
    .context("RTSP DESCRIBE")?;
    let vi = session
        .streams()
        .iter()
        .position(|s| s.media() == "video" && matches!(s.encoding_name(), "h264" | "h265"))
        .ok_or_else(|| anyhow!("no H.264/H.265 video stream offered"))?;
    session
        .setup(vi, SetupOptions::default().frame_format(FrameFormat::MP4))
        .await
        .context("RTSP SETUP")?;
    let clock_hz = i64::from(session.streams()[vi].clock_rate_hz());
    let mut demuxed = session
        .play(PlayOptions::default())
        .await
        .context("RTSP PLAY")?
        .demuxed()?;

    if let Some(ParametersRef::Video(v)) = demuxed.streams()[vi].parameters() {
        tx.send(SourceEvent::Config(config_from(v)?)).await?;
    }
    while let Some(item) = demuxed.next().await {
        let CodecItem::VideoFrame(f) = item? else {
            continue;
        };
        if f.stream_id() != vi {
            continue;
        }
        if f.has_new_parameters() {
            if let Some(ParametersRef::Video(v)) = demuxed.streams()[vi].parameters() {
                tx.send(SourceEvent::Config(config_from(v)?)).await?;
            }
        }
        let pts_90k = f.timestamp().elapsed() * 90_000 / clock_hz;
        let keyframe = f.is_random_access_point();
        let frame = RawFrame {
            pts_90k,
            keyframe,
            data: Bytes::from(f.into_data()),
        };
        if tx.send(SourceEvent::Frame(frame)).await.is_err() {
            return Ok(()); // receiver gone: shutting down
        }
    }
    bail!("RTSP stream ended")
}

fn config_from(v: &VideoParameters) -> Result<VideoConfig> {
    let codec = match v.rfc6381_codec().get(..4) {
        Some("avc1" | "avc3") => Codec::H264,
        Some("hvc1" | "hev1") => Codec::H265,
        other => bail!("unsupported codec {other:?}"),
    };
    let (w, h) = v.pixel_dimensions();
    Ok(VideoConfig {
        codec,
        width: u16::try_from(w)?,
        height: u16::try_from(h)?,
        rfc6381: v.rfc6381_codec().to_owned(),
        sample_entry: v.mp4_sample_entry().build()?.into(),
    })
}

/// Percent-decodes URL userinfo (`+` is literal here, unlike in query strings).
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_userinfo() {
        assert_eq!(super::decode("p%40ss+w%2"), "p@ss+w%2");
    }
}
