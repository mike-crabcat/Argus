/// Maps a camera's media timestamps onto wall-clock time.
///
/// The first frame anchors `pts` to its arrival time; later frames are placed
/// by pts delta, which is far less jittery than arrival time. If the estimate
/// drifts from arrival time by more than `max_drift_us` (camera clock running
/// fast/slow, or a pts jump), the clock re-anchors. RTCP sender reports can
/// later replace the arrival-time anchor for cameras with good NTP.
#[derive(Debug, Clone)]
pub struct WallClock {
    anchor: Option<(i64, i64)>,
    max_drift_us: i64,
    reanchors: u64,
}

impl WallClock {
    pub fn new(max_drift_us: i64) -> Self {
        Self {
            anchor: None,
            max_drift_us,
            reanchors: 0,
        }
    }

    /// Returns the wall-clock time (µs since epoch) for a frame with this pts
    /// that arrived at `arrival_us`.
    pub fn map(&mut self, pts_90k: i64, arrival_us: i64) -> i64 {
        if let Some((apts, awall)) = self.anchor {
            let est = awall + (pts_90k - apts) * 100 / 9; // 90 kHz → µs
            if (est - arrival_us).abs() <= self.max_drift_us {
                return est;
            }
            self.reanchors += 1;
        }
        self.anchor = Some((pts_90k, arrival_us));
        arrival_us
    }

    /// How often the clock had to re-anchor; a health signal per camera.
    pub fn reanchors(&self) -> u64 {
        self.reanchors
    }
}

impl Default for WallClock {
    fn default() -> Self {
        Self::new(2_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_pts_and_ignores_arrival_jitter() {
        let mut c = WallClock::new(500_000);
        assert_eq!(c.map(0, 1_000_000), 1_000_000);
        // 1 s later in pts, arrives 40 ms late: stays on the pts timeline.
        assert_eq!(c.map(90_000, 2_040_000), 2_000_000);
        assert_eq!(c.reanchors(), 0);
    }

    #[test]
    fn reanchors_on_large_drift() {
        let mut c = WallClock::new(500_000);
        c.map(0, 0);
        assert_eq!(c.map(90_000, 5_000_000), 5_000_000);
        assert_eq!(c.reanchors(), 1);
        assert_eq!(c.map(180_000, 6_000_000), 6_000_000);
    }
}
