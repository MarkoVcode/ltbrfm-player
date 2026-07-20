//! Spectrum analyser: a sliding-window FFT over the post-EQ mono signal,
//! reduced to 20 log-spaced magnitude bars (0..1) to drive the LED display.

use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::sync::Arc;

pub const BARS: usize = 20;
const SIZE: usize = 2048;
const HOP: usize = 1024;

// dB window mapped onto the 0..1 bar range.
const MIN_DB: f32 = -72.0;
const MAX_DB: f32 = -12.0;

pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    ring: Vec<f32>,
    write: usize,
    since_hop: usize,
    scratch: Vec<Complex<f32>>,
    edges: [usize; BARS + 1],
}

impl Spectrum {
    pub fn new(fs: f32) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(SIZE);

        // Hann window.
        let window = (0..SIZE)
            .map(|n| {
                let x = std::f32::consts::PI * n as f32 / (SIZE as f32 - 1.0);
                x.sin().powi(2)
            })
            .collect();

        // Log-spaced bar edges.
        let nyq = fs / 2.0;
        let bins = (SIZE / 2) as f32;
        let lo = 40.0f32;
        let hi = 16_000.0f32.min(nyq * 0.95);
        let mut edges = [0usize; BARS + 1];
        for i in 0..=BARS {
            let f = lo * (hi / lo).powf(i as f32 / BARS as f32);
            edges[i] = ((f / nyq) * bins).round().clamp(0.0, bins - 1.0) as usize;
        }

        Spectrum {
            fft,
            window,
            ring: vec![0.0; SIZE],
            write: 0,
            since_hop: 0,
            scratch: vec![Complex::new(0.0, 0.0); SIZE],
            edges,
        }
    }

    /// Feed one mono sample. Returns a fresh set of bars once per hop.
    #[inline]
    pub fn push(&mut self, sample: f32) -> Option<[f32; BARS]> {
        self.ring[self.write] = sample;
        self.write = (self.write + 1) % SIZE;
        self.since_hop += 1;
        if self.since_hop >= HOP {
            self.since_hop = 0;
            Some(self.compute())
        } else {
            None
        }
    }

    fn compute(&mut self) -> [f32; BARS] {
        // Copy the ring into the FFT scratch in chronological order, windowed.
        for k in 0..SIZE {
            let s = self.ring[(self.write + k) % SIZE];
            self.scratch[k] = Complex::new(s * self.window[k], 0.0);
        }
        self.fft.process(&mut self.scratch);

        let mut bars = [0.0f32; BARS];
        let norm = SIZE as f32 * 0.25;
        for i in 0..BARS {
            let a = self.edges[i];
            let b = self.edges[i + 1].max(a + 1);
            let mut peak = 0.0f32;
            for bin in a..b {
                let m = self.scratch[bin].norm();
                if m > peak {
                    peak = m;
                }
            }
            let db = 20.0 * (peak / norm + 1e-9).log10();
            bars[i] = ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0);
        }
        bars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_tone_in_the_right_bar() {
        const FS: f32 = 44_100.0;
        let freq = 1000.0f32;
        let mut spec = Spectrum::new(FS);
        let mut last: Option<[f32; BARS]> = None;
        for i in 0..(SIZE * 8) {
            let x = (2.0 * std::f32::consts::PI * freq * i as f32 / FS).sin();
            if let Some(bars) = spec.push(x) {
                last = Some(bars);
            }
        }
        let bars = last.expect("no spectrum frame produced");

        // argmax
        let (peak_idx, peak_val) = bars
            .iter()
            .enumerate()
            .fold((0usize, 0.0f32), |(bi, bv), (i, &v)| {
                if v > bv {
                    (i, v)
                } else {
                    (bi, bv)
                }
            });

        // Expected bar for 1 kHz on the log scale.
        let nyq = FS / 2.0;
        let lo = 40.0f32;
        let hi = 16_000.0f32.min(nyq * 0.95);
        let expected = (BARS as f32 * (freq / lo).ln() / (hi / lo).ln()).round() as usize;

        assert!(peak_val > 0.5, "tone not strong: {peak_val}");
        assert!(
            (peak_idx as i32 - expected as i32).abs() <= 2,
            "tone in bar {peak_idx}, expected ~{expected}"
        );
    }
}
