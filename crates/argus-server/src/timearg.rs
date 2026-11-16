//! Times given on the command line.

use anyhow::{Context, Result, bail};

/// Parses an RFC 3339 time (`2026-11-16T21:30:00Z`), Unix milliseconds, or
/// `now` with an optional offset (`now-90s`, `now+2m`, `now-1h`). Returns
/// Unix milliseconds.
pub fn parse(s: &str, now_ms: i64) -> Result<i64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("now") {
        if rest.is_empty() {
            return Ok(now_ms);
        }
        let (sign, amount) = match rest.split_at(1) {
            ("-", a) => (-1, a),
            ("+", a) => (1, a),
            _ => bail!("expected now, now-N or now+N with s, m or h: {s:?}"),
        };
        let (n, unit) = amount.split_at(amount.len().saturating_sub(1));
        let n: i64 = n.parse().with_context(|| format!("bad offset in {s:?}"))?;
        let unit_ms = match unit {
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            _ => bail!("offset unit must be s, m or h: {s:?}"),
        };
        return Ok(now_ms + sign * n * unit_ms);
    }
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    let t = chrono::DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("{s:?} is not RFC 3339, Unix ms or now[-+]N[smh]"))?;
    Ok(t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_all_forms() {
        let now = 1_800_000_000_000;
        assert_eq!(parse("now", now).unwrap(), now);
        assert_eq!(parse("now-90s", now).unwrap(), now - 90_000);
        assert_eq!(parse("now+2m", now).unwrap(), now + 120_000);
        assert_eq!(parse("now-1h", now).unwrap(), now - 3_600_000);
        assert_eq!(parse("1794863500370", now).unwrap(), 1_794_863_500_370);
        assert_eq!(
            parse("2026-11-16T21:30:00Z", now).unwrap(),
            1_794_864_600_000
        );
        assert_eq!(
            parse("2026-11-17T10:30:00+13:00", now).unwrap(),
            1_794_864_600_000
        );
        for bad in ["now-5", "now*5s", "yesterday", "now-xs"] {
            assert!(parse(bad, now).is_err(), "{bad}");
        }
    }
}
