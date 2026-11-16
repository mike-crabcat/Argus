use bytes::Bytes;

use crate::mp4box::write_box;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
        }
    }
}

/// Everything a decoder needs before the first frame: the codec, the display
/// size and a complete ISO BMFF `VisualSampleEntry` box (`avc1`/`hvc1`)
/// carrying the parameter sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoConfig {
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    /// RFC 6381 codec string, e.g. `avc1.640028`. This is what WebCodecs takes.
    pub rfc6381: String,
    /// A complete `avc1`/`hvc1` sample entry box, including its header.
    pub sample_entry: Bytes,
}

impl VideoConfig {
    /// Builds a config from an H.264 `AVCDecoderConfigurationRecord` (the body
    /// of an `avcC` box, which is also what an FLV AVC sequence header carries).
    pub fn from_avcc(record: &[u8], width: u16, height: u16) -> Result<Self, ConfigError> {
        if record.len() < 7 || record[0] != 1 {
            return Err(ConfigError::BadRecord("avcC"));
        }
        let rfc6381 = format!("avc1.{:02X}{:02X}{:02X}", record[1], record[2], record[3]);
        Ok(Self {
            codec: Codec::H264,
            width,
            height,
            rfc6381,
            sample_entry: visual_sample_entry(b"avc1", width, height, b"avcC", record).into(),
        })
    }

    /// Stable identity of this configuration. A change (e.g. after a camera
    /// reboot with new settings) starts a new init segment version.
    pub fn fingerprint(&self) -> u64 {
        // FNV-1a: stable across Rust releases, unlike `DefaultHasher`.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.sample_entry.iter() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("malformed {0} decoder configuration record")]
    BadRecord(&'static str),
}

/// ISO/IEC 14496-12 §12.1.3 VisualSampleEntry with one codec config child box.
pub(crate) fn visual_sample_entry(
    fourcc: &[u8; 4],
    width: u16,
    height: u16,
    config_fourcc: &[u8; 4],
    record: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(86 + 8 + record.len());
    write_box(&mut buf, fourcc, |b| {
        b.extend_from_slice(&[0; 6]); // reserved
        b.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
        b.extend_from_slice(&[0; 16]); // pre_defined + reserved
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
        b.extend_from_slice(&0x0048_0000u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // reserved
        b.extend_from_slice(&1u16.to_be_bytes()); // frame_count
        b.extend_from_slice(&[0; 32]); // compressorname
        b.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
        b.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined
        write_box(b, config_fourcc, |b| b.extend_from_slice(record));
    });
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_config_has_codec_string_and_entry() {
        let record = [1, 0x64, 0x00, 0x28, 0xff, 0xe0, 0x00];
        let cfg = VideoConfig::from_avcc(&record, 1920, 1080).unwrap();
        assert_eq!(cfg.rfc6381, "avc1.640028");
        assert_eq!(&cfg.sample_entry[4..8], b"avc1");
        let len = u32::from_be_bytes(cfg.sample_entry[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, cfg.sample_entry.len());
        assert_eq!(
            &cfg.sample_entry[len - record.len() - 4..len - record.len()],
            b"avcC"
        );
    }

    #[test]
    fn fingerprint_changes_with_parameters() {
        let a = VideoConfig::from_avcc(&[1, 0x64, 0, 0x28, 0xff, 0xe0, 0], 640, 360).unwrap();
        let b = VideoConfig::from_avcc(&[1, 0x64, 0, 0x29, 0xff, 0xe0, 0], 640, 360).unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), a.clone().fingerprint());
    }
}
