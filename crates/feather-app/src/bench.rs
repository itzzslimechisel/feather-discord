//! In-process benchmark support, active only when FEATHER_BENCH is set. It records the CPU time
//! of every UI frame and can drive repeatable scenarios (channel switching, continuous scroll).

use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Bench {
    pub on: bool,
    pub scenario: String,
    frame_ms: Vec<f32>,
    last_report: Option<Instant>,
    last_action: Option<Instant>,
    pub switch_every: Duration,
    pub scroll_dir: f32,
    switch_idx: usize,
}

impl Bench {
    pub fn from_env() -> Bench {
        let on = feather_core::diag::bench_enabled();
        Bench {
            on,
            scenario: std::env::var("FEATHER_BENCH_SCENARIO").unwrap_or_default(),
            switch_every: Duration::from_millis(std::env::var("FEATHER_BENCH_SWITCH_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(400)),
            scroll_dir: 1.0,
            ..Default::default()
        }
    }

    pub fn frame(&mut self, ms: f32) {
        if !self.on {
            return;
        }
        self.frame_ms.push(ms);
        let now = Instant::now();
        let last = *self.last_report.get_or_insert(now);
        if now.duration_since(last) >= Duration::from_secs(5) {
            self.report();
            self.last_report = Some(now);
        }
    }

    pub fn report(&mut self) {
        if self.frame_ms.is_empty() {
            return;
        }
        let mut v = std::mem::take(&mut self.frame_ms);
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f32| v[((v.len() - 1) as f32 * q) as usize];
        feather_core::diag::stat("frames", v.len());
        feather_core::diag::stat("frame_cpu_ms_p50", format!("{:.3}", p(0.5)));
        feather_core::diag::stat("frame_cpu_ms_p95", format!("{:.3}", p(0.95)));
        feather_core::diag::stat("frame_cpu_ms_max", format!("{:.3}", v[v.len() - 1]));
    }

    /// True when it is time for the next scripted channel switch; returns the index to use.
    pub fn next_switch(&mut self, count: usize) -> Option<usize> {
        if !self.on || self.scenario != "switch" || count == 0 {
            return None;
        }
        let now = Instant::now();
        match self.last_action {
            Some(t) if now.duration_since(t) < self.switch_every => None,
            _ => {
                self.last_action = Some(now);
                self.switch_idx = (self.switch_idx + 1) % count;
                Some(self.switch_idx)
            }
        }
    }
}
