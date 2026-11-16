use bytes::Bytes;

use crate::mp4box::{write_box, write_full_box};

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
        fnv1a(FNV_OFFSET, &self.sample_entry)
    }

    /// The decoder configuration record (the body of the `avcC`/`hvcC` box):
    /// what WebCodecs `VideoDecoderConfig.description` takes.
    pub fn decoder_record(&self) -> &[u8] {
        // VisualSampleEntry: 8-byte box header + 78 bytes of fields, then
        // the config box's own 8-byte header.
        self.sample_entry.get(8 + 78 + 8..).unwrap_or(&[])
    }
}

/// AAC audio, stored exactly as the camera sent it. `sample_entry` is a
/// complete `mp4a` box whose `esds` carries the AudioSpecificConfig.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    /// Samples per AAC frame: 1024, or 960 when the stream says so.
    pub samples_per_frame: u32,
    /// RFC 6381 codec string, e.g. `mp4a.40.2` for AAC-LC.
    pub rfc6381: String,
    /// A complete `mp4a` sample entry box, including its header.
    pub sample_entry: Bytes,
}

/// AAC sampling frequencies by `samplingFrequencyIndex` (ISO/IEC 14496-3).
const AAC_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

impl AudioConfig {
    /// Builds a config from an AAC AudioSpecificConfig (what an FLV AAC
    /// sequence header and an RTSP `config=` parameter carry). Supports the
    /// plain AAC object types cameras use (Main, LC, SSR, LTP); HE-AAC's
    /// SBR/PS signalling changes frame timing and is rejected.
    pub fn from_aac_asc(asc: &[u8]) -> Result<Self, ConfigError> {
        let mut r = BitReader { data: asc, pos: 0 };
        let mut object_type = r.read(5)?;
        if object_type == 31 {
            object_type = 32 + r.read(6)?;
        }
        if !(1..=4).contains(&object_type) {
            return Err(ConfigError::UnsupportedAudio(format!(
                "AAC object type {object_type}"
            )));
        }
        let sample_rate = match r.read(4)? {
            0xf => r.read(24)?,
            i => *AAC_RATES
                .get(i as usize)
                .ok_or(ConfigError::BadRecord("AudioSpecificConfig"))?,
        };
        let channels = match r.read(4)? {
            0 => {
                return Err(ConfigError::UnsupportedAudio(
                    "AAC channel layout in a PCE".into(),
                ));
            }
            7 => 8,
            c => c as u16,
        };
        let samples_per_frame = if r.read(1)? == 1 { 960 } else { 1024 };
        if sample_rate == 0 {
            return Err(ConfigError::BadRecord("AudioSpecificConfig"));
        }
        Ok(Self {
            sample_rate,
            channels,
            samples_per_frame,
            rfc6381: format!("mp4a.40.{object_type}"),
            sample_entry: mp4a_sample_entry(channels, sample_rate, asc).into(),
        })
    }

    pub fn fingerprint(&self) -> u64 {
        fnv1a(FNV_OFFSET, &self.sample_entry)
    }
}

/// Identity of everything in one init segment. Video-only streams keep the
/// plain video fingerprint, so their init segment keys don't change.
pub fn stream_fingerprint(video: &VideoConfig, audio: Option<&AudioConfig>) -> u64 {
    match audio {
        None => video.fingerprint(),
        Some(a) => fnv1a(video.fingerprint(), &a.sample_entry),
    }
}

// FNV-1a: stable across Rust releases, unlike `DefaultHasher`.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv1a(mut h: u64, data: &[u8]) -> u64 {
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, bits: usize) -> Result<u32, ConfigError> {
        let mut v = 0u32;
        for _ in 0..bits {
            let byte = self
                .data
                .get(self.pos / 8)
                .ok_or(ConfigError::BadRecord("AudioSpecificConfig"))?;
            v = v << 1 | u32::from(byte >> (7 - self.pos % 8) & 1);
            self.pos += 1;
        }
        Ok(v)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("malformed {0} decoder configuration record")]
    BadRecord(&'static str),
    #[error("unsupported audio: {0}")]
    UnsupportedAudio(String),
}

/// ISO/IEC 14496-12 §12.2.3 AudioSampleEntry (`mp4a`) with an `esds` box
/// (ISO/IEC 14496-14) holding the AudioSpecificConfig.
fn mp4a_sample_entry(channels: u16, sample_rate: u32, asc: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + asc.len());
    write_box(&mut buf, b"mp4a", |b| {
        b.extend_from_slice(&[0; 6]); // reserved
        b.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
        b.extend_from_slice(&[0; 8]); // reserved
        b.extend_from_slice(&channels.to_be_bytes());
        b.extend_from_slice(&16u16.to_be_bytes()); // samplesize
        b.extend_from_slice(&[0; 4]); // pre_defined + reserved
        // 16.16 fixed point; rates above 65535 Hz don't fit and the decoder
        // takes the real rate from the esds instead.
        let rate = if sample_rate <= 0xffff {
            sample_rate << 16
        } else {
            0
        };
        b.extend_from_slice(&rate.to_be_bytes());
        write_full_box(b, b"esds", 0, 0, |b| {
            let dsi = descriptor(0x05, asc); // DecoderSpecificInfo
            let mut dcd = vec![
                0x40, // objectTypeIndication: Audio ISO/IEC 14496-3
                0x15, // streamType 5 (audio) << 2 | upStream 0 | reserved 1
                0, 0, 0, // bufferSizeDB
            ];
            dcd.extend_from_slice(&[0; 8]); // maxBitrate, avgBitrate: unknown
            dcd.extend_from_slice(&dsi);
            let mut es = vec![0, 0, 0]; // ES_ID 0, no optional fields
            es.extend_from_slice(&descriptor(0x04, &dcd)); // DecoderConfigDescriptor
            es.extend_from_slice(&descriptor(0x06, &[0x02])); // SLConfigDescriptor (MP4)
            b.extend_from_slice(&descriptor(0x03, &es)); // ES_Descriptor
        });
    });
    buf
}

/// An MPEG-4 descriptor: tag, then the body length in 7-bit groups.
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let mut groups = vec![(body.len() & 0x7f) as u8];
    let mut n = body.len() >> 7;
    while n > 0 {
        groups.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.extend(groups.iter().rev());
    out.extend_from_slice(body);
    out
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
    fn parses_aac_lc_16k_mono() {
        // 00010 (LC) 1000 (16 kHz) 0001 (mono) 000
        let a = AudioConfig::from_aac_asc(&[0x14, 0x08]).unwrap();
        assert_eq!(
            (a.sample_rate, a.channels, a.samples_per_frame),
            (16000, 1, 1024)
        );
        assert_eq!(a.rfc6381, "mp4a.40.2");
        // 48 kHz stereo, 960-sample frames.
        let b = AudioConfig::from_aac_asc(&[0x11, 0x94]).unwrap();
        assert_eq!(
            (b.sample_rate, b.channels, b.samples_per_frame),
            (48000, 2, 960)
        );
        assert!(AudioConfig::from_aac_asc(&[0x2b, 0x92, 0x08, 0x00]).is_err()); // HE-AAC
        assert!(AudioConfig::from_aac_asc(&[0x14]).is_err()); // truncated
    }

    #[test]
    fn mp4a_entry_layout() {
        let asc = [0x14, 0x08];
        let e = AudioConfig::from_aac_asc(&asc).unwrap().sample_entry;
        let len = u32::from_be_bytes(e[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, e.len());
        assert_eq!(&e[4..8], b"mp4a");
        assert_eq!(u16::from_be_bytes([e[14], e[15]]), 1); // data_reference_index
        assert_eq!(u16::from_be_bytes([e[24], e[25]]), 1); // channelcount
        assert_eq!(u16::from_be_bytes([e[26], e[27]]), 16); // samplesize
        assert_eq!(
            u32::from_be_bytes(e[32..36].try_into().unwrap()),
            16000 << 16
        );
        // esds follows the 36-byte AudioSampleEntry body.
        let esds = &e[36..];
        assert_eq!(
            u32::from_be_bytes(esds[0..4].try_into().unwrap()) as usize,
            esds.len()
        );
        assert_eq!(&esds[4..8], b"esds");
        assert_eq!(&esds[8..12], &[0, 0, 0, 0]); // version + flags
        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x03, 25, 0, 0, 0,             // ES_Descriptor, ES_ID 0, flags 0
            0x04, 17, 0x40, 0x15, 0, 0, 0, // DecoderConfigDescriptor: AAC, audio
            0, 0, 0, 0, 0, 0, 0, 0,        // maxBitrate, avgBitrate
            0x05, 2, 0x14, 0x08,           // DecoderSpecificInfo = ASC
            0x06, 1, 0x02,                 // SLConfigDescriptor
        ];
        assert_eq!(&esds[12..], expected);
    }

    #[test]
    fn long_descriptor_lengths_use_continuation_bits() {
        let d = descriptor(0x05, &[0; 200]);
        assert_eq!(&d[..3], &[0x05, 0x81, 0x48]); // 200 = 1 << 7 | 72
        assert_eq!(d.len(), 203);
    }

    #[test]
    fn stream_fingerprint_includes_audio() {
        let v = VideoConfig::from_avcc(&[1, 0x64, 0, 0x28, 0xff, 0xe0, 0], 640, 360).unwrap();
        let a = AudioConfig::from_aac_asc(&[0x14, 0x08]).unwrap();
        let b = AudioConfig::from_aac_asc(&[0x11, 0x90]).unwrap();
        assert_eq!(stream_fingerprint(&v, None), v.fingerprint());
        assert_ne!(stream_fingerprint(&v, Some(&a)), v.fingerprint());
        assert_ne!(
            stream_fingerprint(&v, Some(&a)),
            stream_fingerprint(&v, Some(&b))
        );
    }

    #[test]
    fn decoder_record_is_the_avcc_body() {
        let record = [1, 0x64, 0x00, 0x28, 0xff, 0xe0, 0x00];
        let cfg = VideoConfig::from_avcc(&record, 1920, 1080).unwrap();
        assert_eq!(cfg.decoder_record(), record);
    }

    #[test]
    fn fingerprint_changes_with_parameters() {
        let a = VideoConfig::from_avcc(&[1, 0x64, 0, 0x28, 0xff, 0xe0, 0], 640, 360).unwrap();
        let b = VideoConfig::from_avcc(&[1, 0x64, 0, 0x29, 0xff, 0xe0, 0], 640, 360).unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), a.clone().fingerprint());
    }
}
