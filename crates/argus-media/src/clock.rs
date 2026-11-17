/// Maps a camera's media timestamps onto wall-clock time.
///
/// Frames are placed by pts delta from an anchor, which is far less jittery
/// than their arrival times. The anchor follows the *earliest* arrivals: a
/// frame can arrive late (network queueing, a burst after connecting) but
/// never before it was captured, so whenever a frame arrives sooner than
/// its estimate the anchor moves earlier to match. If instead every frame
/// in a window arrives later than estimated (the camera clock runs slow, or
/// the path got slower), the anchor moves later by the smallest lag seen.
/// So wall time tracks capture time plus the minimum network delay, and is
/// never ahead of arrival. A jump bigger than `max_drift_us` (camera clock
/// reset) re-anchors outright. RTCP sender reports can later replace the
/// arrival-time anchor for cameras with good NTP.
#[derive(Debug, Clone)]
pub struct WallClock {
    anchor: Option<(i64, i64)>,
    max_drift_us: i64,
    reanchors: u64,
    /// Smallest lag (arrival minus estimate) since `window_start_us`.
    window_min_lag_us: i64,
    window_start_us: i64,
}

/// How often a consistently late clock is allowed to move later.
const WINDOW_US: i64 = 10_000_000;

impl WallClock {
    pub fn new(max_drift_us: i64) -> Self {
        Self {
            anchor: None,
            max_drift_us,
            reanchors: 0,
            window_min_lag_us: i64::MAX,
            window_start_us: 0,
        }
    }

    /// Returns the wall-clock time (µs since epoch) for a frame with this pts
    /// that arrived at `arrival_us`.
    pub fn map(&mut self, pts_90k: i64, arrival_us: i64) -> i64 {
        let Some((apts, awall)) = self.anchor else {
            return self.reanchor(pts_90k, arrival_us);
        };
        let est = awall + (pts_90k - apts) * 100 / 9; // 90 kHz → µs
        let lag = arrival_us - est;
        if lag.abs() > self.max_drift_us {
            self.reanchors += 1;
            return self.reanchor(pts_90k, arrival_us);
        }
        if lag < 0 {
            // Arrived sooner than estimated: the anchor frame was late.
            self.anchor = Some((apts, awall + lag));
            self.window_min_lag_us = 0;
            return arrival_us;
        }
        self.window_min_lag_us = self.window_min_lag_us.min(lag);
        if arrival_us - self.window_start_us >= WINDOW_US {
            let shift = self.window_min_lag_us;
            if shift > 0 && shift != i64::MAX {
                self.anchor = Some((apts, awall + shift));
            }
            self.window_start_us = arrival_us;
            self.window_min_lag_us = i64::MAX;
            return est + shift.clamp(0, lag);
        }
        est
    }

    fn reanchor(&mut self, pts_90k: i64, arrival_us: i64) -> i64 {
        self.anchor = Some((pts_90k, arrival_us));
        self.window_start_us = arrival_us;
        self.window_min_lag_us = i64::MAX;
        arrival_us
    }

    /// Wall-clock time for `pts_90k` on the current anchor, without
    /// anchoring or re-anchoring. Lets audio follow the video's timeline.
    pub fn estimate(&self, pts_90k: i64) -> Option<i64> {
        self.anchor
            .map(|(apts, awall)| awall + (pts_90k - apts) * 100 / 9)
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

    /// 20 fps frames captured at `i * 50 ms`, arriving after `delay_ms`.
    fn run(c: &mut WallClock, delays_ms: &[i64]) -> Vec<i64> {
        delays_ms
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let i = i as i64;
                c.map(i * 4500, 1_000_000_000 + i * 50_000 + d * 1000)
                    - (1_000_000_000 + i * 50_000)
            })
            .collect()
    }

    #[test]
    fn late_first_frame_does_not_push_the_clock_ahead() {
        // The first frame arrives 80 ms late (connection burst); the rest
        // within 5-15 ms. Wall time must settle at capture + 5 ms (the
        // fastest delivery), never ahead of any frame's arrival.
        let mut c = WallClock::default();
        let delays = [80, 12, 5, 9, 15, 7, 5, 11, 6, 13, 5, 10];
        let offsets = run(&mut c, &delays);
        for (o, d) in offsets.iter().zip(delays) {
            assert!(*o <= d * 1000, "wall time ahead of arrival: {offsets:?}");
        }
        assert_eq!(
            offsets[2..].iter().copied().max(),
            Some(5_000),
            "{offsets:?}"
        );
        assert_eq!(c.reanchors(), 0);
    }

    #[test]
    fn steady_slower_path_moves_the_clock_later_once_per_window() {
        // Network path gets 30 ms slower and stays that way: after one
        // window the clock follows, so wall time is still capture + the
        // minimum delay rather than 30 ms early forever.
        let mut c = WallClock::default();
        let mut delays = vec![5; 20];
        delays.extend(std::iter::repeat_n(35, 400)); // 20 s at 20 fps
        let offsets = run(&mut c, &delays);
        assert_eq!(offsets[10], 5_000);
        assert_eq!(*offsets.last().unwrap(), 35_000, "{:?}", &offsets[380..]);
        assert!(offsets.iter().zip(&delays).all(|(o, d)| *o <= d * 1000));
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
