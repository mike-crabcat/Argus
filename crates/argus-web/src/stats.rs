//! Per-tile statistics: frame rate and a smoothed latency.

use std::collections::VecDeque;

/// Events per second over the last `window_ms`.
#[derive(Clone, Debug)]
pub struct Rate {
    window_ms: f64,
    times: VecDeque<f64>,
}

impl Rate {
    pub fn new(window_ms: f64) -> Self {
        Self {
            window_ms,
            times: VecDeque::new(),
        }
    }

    pub fn tick(&mut self, now_ms: f64) {
        self.times.push_back(now_ms);
        self.trim(now_ms);
    }

    pub fn per_sec(&mut self, now_ms: f64) -> f64 {
        self.trim(now_ms);
        self.times.len() as f64 * 1000.0 / self.window_ms
    }

    fn trim(&mut self, now_ms: f64) {
        while self
            .times
            .front()
            .is_some_and(|&t| t <= now_ms - self.window_ms)
        {
            self.times.pop_front();
        }
    }
}

/// Exponential moving average, for numbers shown to people.
#[derive(Clone, Copy, Debug, Default)]
pub struct Smoothed {
    value: Option<f64>,
}

impl Smoothed {
    pub fn add(&mut self, x: f64) {
        self.value = Some(match self.value {
            None => x,
            Some(v) => v + (x - v) * 0.1,
        });
    }

    pub fn get(&self) -> Option<f64> {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_counts_the_last_window() {
        let mut r = Rate::new(1000.0);
        for i in 0..40 {
            r.tick(i as f64 * 50.0); // 20 fps for 2 s
        }
        assert_eq!(r.per_sec(1950.0), 20.0);
        assert_eq!(r.per_sec(10_000.0), 0.0);
    }

    #[test]
    fn smoothing_converges() {
        let mut s = Smoothed::default();
        assert_eq!(s.get(), None);
        s.add(100.0);
        assert_eq!(s.get(), Some(100.0));
        for _ in 0..100 {
            s.add(50.0);
        }
        assert!((s.get().unwrap() - 50.0).abs() < 0.01);
    }
}
