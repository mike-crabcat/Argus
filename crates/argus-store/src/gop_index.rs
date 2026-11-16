//! The per-chunk seek table: one fixed-size entry per GOP, packed into a
//! single BLOB so a seek is one row read rather than one row per GOP.

/// Where one GOP (one fMP4 fragment) lives inside its chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GopEntry {
    /// Wall-clock start of the GOP, relative to the chunk's `start_ms`.
    pub start_offset_ms: u32,
    pub byte_offset: u32,
    pub byte_len: u32,
    pub frame_count: u16,
}

const ENTRY_LEN: usize = 14;

pub fn encode(entries: &[GopEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * ENTRY_LEN);
    for e in entries {
        out.extend_from_slice(&e.start_offset_ms.to_le_bytes());
        out.extend_from_slice(&e.byte_offset.to_le_bytes());
        out.extend_from_slice(&e.byte_len.to_le_bytes());
        out.extend_from_slice(&e.frame_count.to_le_bytes());
    }
    out
}

pub fn decode(blob: &[u8]) -> Vec<GopEntry> {
    blob.chunks_exact(ENTRY_LEN)
        .map(|c| GopEntry {
            start_offset_ms: u32::from_le_bytes(c[0..4].try_into().unwrap()),
            byte_offset: u32::from_le_bytes(c[4..8].try_into().unwrap()),
            byte_len: u32::from_le_bytes(c[8..12].try_into().unwrap()),
            frame_count: u16::from_le_bytes(c[12..14].try_into().unwrap()),
        })
        .collect()
}

/// The GOP containing `offset_ms` (the last one starting at or before it).
pub fn find(entries: &[GopEntry], offset_ms: u32) -> Option<usize> {
    match entries.partition_point(|e| e.start_offset_ms <= offset_ms) {
        0 => None,
        n => Some(n - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_find() {
        let e: Vec<_> = (0..60)
            .map(|i| GopEntry {
                start_offset_ms: i * 1000,
                byte_offset: i * 500,
                byte_len: 500,
                frame_count: 15,
            })
            .collect();
        let blob = encode(&e);
        assert_eq!(blob.len(), 60 * 14);
        assert_eq!(decode(&blob), e);
        assert_eq!(find(&e, 0), Some(0));
        assert_eq!(find(&e, 1999), Some(1));
        assert_eq!(find(&e, 59_999), Some(59));
    }
}
