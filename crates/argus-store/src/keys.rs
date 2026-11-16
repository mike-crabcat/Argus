use chrono::{DateTime, Datelike, Timelike, Utc};

/// Object key for a chunk starting at `start_ms` (UTC-partitioned by hour).
pub fn chunk_key(camera: &str, stream: &str, start_ms: i64) -> String {
    let t: DateTime<Utc> = DateTime::from_timestamp_millis(start_ms).unwrap_or_default();
    format!(
        "rec/{camera}/{stream}/{:04}/{:02}/{:02}/{:02}/{start_ms}.m4s",
        t.year(),
        t.month(),
        t.day(),
        t.hour()
    )
}

pub fn init_key(camera: &str, stream: &str, fingerprint: u64) -> String {
    format!("init/{camera}/{stream}/{fingerprint:016x}.mp4")
}

/// Camera and stream names become path segments, so keep them boring.
pub fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_keys_partition_by_utc_hour() {
        // 2026-11-16T13:14:32.480Z
        let k = chunk_key("driveway", "main", 1_794_834_872_480);
        assert_eq!(k, "rec/driveway/main/2026/11/16/13/1794834872480.m4s");
    }

    #[test]
    fn names_are_path_safe() {
        assert!(is_valid_name("front_door-2"));
        assert!(!is_valid_name("../etc"));
        assert!(!is_valid_name(""));
    }
}
