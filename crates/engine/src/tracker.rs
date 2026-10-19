//! Beat tracker: a grid filter over (beat phase, tempo).
//!
//! The state is a log-probability for every combination of tempo and
//! position within the beat. There is no hit detection: every audio frame
//! updates the grid with that frame's hit strength.
//!
//! Each frame:
//! - every cell moves forward by one frame (each tempo row has exactly as many
//!   cells as frames per beat, so this is a pure index rotation),
//! - the evidence of the frame is added: cells near a beat gain when the frame
//!   is loud, cells away from beats and subdivisions lose,
//! - old evidence fades: the log-probabilities are pulled towards a reference
//!   with a half-life ("exponential forgetting"); the reference is flat over
//!   phase and centred on the current tempo, which keeps the tracker from
//!   jumping to double or half tempo.
//! Every few frames tempo diffuses to neighbouring rows ("tempo drift").

/// Width of the timing kernel around a beat, in seconds.
const KERNEL_SIGMA_S: f64 = 0.012;
/// Evidence weight of a hit on the half beat, relative to the beat.
const HALF_WEIGHT: f32 = 0.35;
/// Evidence weight of a hit on a quarter of the beat.
const QUARTER_WEIGHT: f32 = 0.15;
/// Penalty for a hit far from any beat or subdivision.
const OFF_PENALTY: f32 = 0.3;
/// Overall strength of the evidence of one frame (log-likelihood scale).
const EVIDENCE_GAIN: f32 = 2.0;
/// Width of the tempo reference, as log-tempo standard deviation.
const REFERENCE_SIGMA: f64 = 0.06;
/// Tempo rows are spaced at least this ratio apart.
const ROW_RATIO: f64 = 1.007;
/// Frames between tempo diffusion steps.
const DRIFT_EVERY: u32 = 16;
/// Lowest log-probability kept relative to the best cell.
const LOG_FLOOR: f32 = -60.0;
/// Frames with evidence below this are skipped.
const MIN_EVIDENCE: f32 = 0.02;

#[derive(Clone, Debug)]
struct Row {
    /// Frames per beat.
    n: usize,
    /// Start of this row in `cells`.
    start: usize,
    log_period: f64,
    /// Evidence kernel by logical phase index.
    kernel: Vec<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    /// Position within the beat at the current frame, 0..1, 0 = on the beat.
    pub phase: f64,
    /// Beat period in seconds.
    pub period: f64,
}

impl Estimate {
    pub fn bpm(&self) -> f64 {
        60.0 / self.period
    }
}

#[derive(Clone, Debug)]
pub struct Tracker {
    hop_s: f64,
    rows: Vec<Row>,
    cells: Vec<f32>,
    /// Frames since the grid was built; logical phase index of physical cell
    /// `j` in a row with `n` cells is `(j + frame) % n`.
    frame: u64,
    lambda: f64,
    /// Forgetting factor not yet applied to `cells`.
    pending: f64,
    reference: Vec<f32>,
    ref_center: f64,
    drift_weights: Vec<f32>,
    frames_since_drift: u32,
    scratch: Vec<f32>,
    scratch_out: Vec<f32>,
    cached: Option<Estimate>,
    cached_conf: Option<f32>,
}

impl Tracker {
    pub fn new(hop_s: f64, bpm_min: f64, bpm_max: f64, half_life_s: f64, drift_pct: f64) -> Self {
        let n_min = ((60.0 / (bpm_max * hop_s)).floor() as usize).max(4);
        let n_max = ((60.0 / (bpm_min * hop_s)).ceil() as usize).max(n_min + 1);
        let mut rows = Vec::new();
        let mut start = 0;
        let mut last = 0usize;
        for n in n_min..=n_max {
            if last != 0 && (n as f64) < last as f64 * ROW_RATIO {
                continue;
            }
            last = n;
            let period = n as f64 * hop_s;
            let kernel = (0..n).map(|i| kernel_value(i as f64 / n as f64, period)).collect();
            rows.push(Row { n, start, log_period: period.ln(), kernel });
            start += n;
        }
        let cells = vec![0.0; start];
        let mut t = Self {
            hop_s,
            rows,
            cells,
            frame: 0,
            lambda: 1.0,
            pending: 1.0,
            reference: Vec::new(),
            ref_center: 0.0,
            drift_weights: vec![1.0],
            frames_since_drift: 0,
            scratch: vec![0.0; start],
            scratch_out: vec![0.0; start],
            cached: None,
            cached_conf: None,
        };
        t.set_dynamics(half_life_s, drift_pct);
        let mid = (t.rows[0].log_period + t.rows[t.rows.len() - 1].log_period) / 2.0;
        t.set_reference(mid);
        t
    }

    /// Change half-life and tempo drift without losing the state.
    pub fn set_dynamics(&mut self, half_life_s: f64, drift_pct: f64) {
        self.apply_forgetting();
        self.lambda = 0.5f64.powf(self.hop_s / half_life_s.max(0.01));
        let r = self.rows.len();
        let spacing = if r > 1 {
            (self.rows[r - 1].log_period - self.rows[0].log_period) / (r - 1) as f64
        } else {
            1.0
        };
        let dt = DRIFT_EVERY as f64 * self.hop_s;
        let sigma_rows = (drift_pct / 100.0) * dt.sqrt() / spacing;
        if sigma_rows < 0.05 {
            self.drift_weights = vec![1.0];
        } else {
            let k = ((3.0 * sigma_rows).ceil() as usize).clamp(1, 8);
            let w: Vec<f32> = (0..=2 * k)
                .map(|i| {
                    let d = i as f64 - k as f64;
                    (-0.5 * (d / sigma_rows).powi(2)).exp() as f32
                })
                .collect();
            let s: f32 = w.iter().sum();
            self.drift_weights = w.into_iter().map(|v| v / s).collect();
        }
    }

    pub fn bpm_range(&self) -> (f64, f64) {
        let p0 = self.rows[0].log_period.exp();
        let p1 = self.rows[self.rows.len() - 1].log_period.exp();
        (60.0 / p1, 60.0 / p0)
    }

    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    fn set_reference(&mut self, center: f64) {
        self.ref_center = center;
        self.reference = self
            .rows
            .iter()
            .map(|r| (-0.5 * ((r.log_period - center) / REFERENCE_SIGMA).powi(2)).max(LOG_FLOOR as f64) as f32)
            .collect();
    }

    /// Start from a belief concentrated around a phase and period.
    pub fn init(&mut self, phase: f64, period: f64, phase_sigma_s: f64, tempo_sigma: f64) {
        let lp = period.ln();
        let f = self.frame;
        for row in &self.rows {
            let n = row.n;
            let rp = row.log_period.exp();
            let tempo_term = -0.5 * ((row.log_period - lp) / tempo_sigma).powi(2);
            for j in 0..n {
                let i = (j as u64 + f) % n as u64;
                let mut d = i as f64 / n as f64 - phase;
                d -= d.round();
                let v = tempo_term - 0.5 * (d * rp / phase_sigma_s).powi(2);
                self.cells[row.start + j] = (v as f32).max(LOG_FLOOR);
            }
        }
        self.pending = 1.0;
        self.set_reference(lp);
        self.cached = None;
        self.cached_conf = None;
    }

    /// Add a tempo hint (e.g. tap tempo) as extra evidence.
    pub fn hint_tempo(&mut self, period: f64, sigma: f64) {
        self.apply_forgetting();
        let lp = period.ln();
        for row in &self.rows {
            let v = (-0.5 * ((row.log_period - lp) / sigma).powi(2)) as f32;
            for c in &mut self.cells[row.start..row.start + row.n] {
                *c += v.max(LOG_FLOOR);
            }
        }
        self.normalize();
        self.set_reference(lp);
        self.cached = None;
        self.cached_conf = None;
    }

    /// Advance by one frame and apply that frame's hit strength (0 = nothing).
    pub fn step(&mut self, strength: f32) {
        self.frame += 1;
        self.pending *= self.lambda;
        self.cached = None;
        self.cached_conf = None;
        if strength >= MIN_EVIDENCE {
            self.observe(strength);
        }
        self.frames_since_drift += 1;
        if self.frames_since_drift >= DRIFT_EVERY {
            self.frames_since_drift = 0;
            self.drift();
        }
    }

    fn observe(&mut self, strength: f32) {
        let a = self.pending as f32;
        self.pending = 1.0;
        let g = EVIDENCE_GAIN * strength;
        let f = self.frame;
        for (ri, row) in self.rows.iter().enumerate() {
            let b = (1.0 - a) * self.reference[ri];
            let n = row.n;
            let s = (f % n as u64) as usize;
            let cells = &mut self.cells[row.start..row.start + n];
            // Physical j has logical index j + s (wrapping).
            let (first, second) = cells.split_at_mut(n - s);
            for (c, k) in first.iter_mut().zip(&row.kernel[s..]) {
                *c = a * *c + b + g * k;
            }
            for (c, k) in second.iter_mut().zip(&row.kernel[..s]) {
                *c = a * *c + b + g * k;
            }
        }
    }

    fn apply_forgetting(&mut self) {
        if self.pending >= 1.0 {
            return;
        }
        let a = self.pending as f32;
        self.pending = 1.0;
        for (ri, row) in self.rows.iter().enumerate() {
            let b = (1.0 - a) * self.reference[ri];
            for c in &mut self.cells[row.start..row.start + row.n] {
                *c = a * *c + b;
            }
        }
    }

    fn normalize(&mut self) {
        let max = self.cells.iter().copied().fold(f32::MIN, f32::max);
        for c in &mut self.cells {
            *c = (*c - max).max(LOG_FLOOR);
        }
    }

    /// Diffuse probability between neighbouring tempo rows, keeping phase.
    fn drift(&mut self) {
        self.apply_forgetting();
        let max = self.cells.iter().copied().fold(f32::MIN, f32::max);
        let f = self.frame;
        // Probabilities in logical phase order.
        for row in &self.rows {
            let n = row.n;
            let s = (f % n as u64) as usize;
            for j in 0..n {
                let mut i = j + s;
                if i >= n {
                    i -= n;
                }
                self.scratch[row.start + i] = (self.cells[row.start + j] - max).exp();
            }
        }
        let k = (self.drift_weights.len() / 2) as isize;
        let nr = self.rows.len() as isize;
        let mut z = 0.0f64;
        for (ri, row) in self.rows.iter().enumerate() {
            let n = row.n;
            let out = &mut self.scratch_out[row.start..row.start + n];
            if k == 0 {
                out.copy_from_slice(&self.scratch[row.start..row.start + n]);
            } else {
                out.iter_mut().for_each(|v| *v = 0.0);
                let mut wsum = 0.0f32;
                for (wi, &w) in self.drift_weights.iter().enumerate() {
                    let r2 = ri as isize + wi as isize - k;
                    if r2 < 0 || r2 >= nr {
                        continue;
                    }
                    wsum += w;
                    let src = &self.rows[r2 as usize];
                    let m = src.n;
                    let data = &self.scratch[src.start..src.start + m];
                    if m == n {
                        for (o, d) in out.iter_mut().zip(data) {
                            *o += w * d;
                        }
                    } else {
                        // Density per unit phase is value * cells, so rescale by m / n.
                        let scale = w * m as f32 / n as f32;
                        let ratio = m as f64 / n as f64;
                        for (i, o) in out.iter_mut().enumerate() {
                            let p = i as f64 * ratio;
                            let i0 = p.floor() as usize % m;
                            let i1 = (i0 + 1) % m;
                            let fr = (p - p.floor()) as f32;
                            *o += scale * (data[i0] * (1.0 - fr) + data[i1] * fr);
                        }
                    }
                }
                if wsum > 0.0 && wsum < 0.9999 {
                    let inv = 1.0 / wsum;
                    out.iter_mut().for_each(|v| *v *= inv);
                }
            }
            z += out.iter().map(|&v| v as f64).sum::<f64>();
        }
        let _ = z;
        // Back to physical order and log domain; the floor also lands here.
        let floor = LOG_FLOOR.exp();
        for row in &self.rows {
            let n = row.n;
            let s = (f % n as u64) as usize;
            for j in 0..n {
                let mut i = j + s;
                if i >= n {
                    i -= n;
                }
                self.cells[row.start + j] = self.scratch_out[row.start + i].max(floor).ln();
            }
        }
        self.normalize();
        // Keep the tempo reference centred on the current estimate.
        let est = self.estimate();
        self.set_reference(est.period.ln());
    }

    /// Best estimate of phase and tempo at the current frame.
    pub fn estimate(&mut self) -> Estimate {
        if let Some(e) = self.cached {
            return e;
        }
        self.apply_forgetting();
        let (mut best, mut br, mut bj) = (f32::MIN, 0usize, 0usize);
        for (ri, row) in self.rows.iter().enumerate() {
            for (j, &v) in self.cells[row.start..row.start + row.n].iter().enumerate() {
                if v > best {
                    best = v;
                    br = ri;
                    bj = j;
                }
            }
        }
        let f = self.frame;
        let row = &self.rows[br];
        let n = row.n;
        let i_best = ((bj as u64 + f) % n as u64) as i64;
        // Refine phase with the posterior mean around the peak.
        let w = ((0.025 * n as f64 / row.log_period.exp()).ceil() as i64).max(2);
        let (mut sw, mut so) = (0.0f64, 0.0f64);
        for o in -w..=w {
            let i = (i_best + o).rem_euclid(n as i64);
            let j = (i - (f % n as u64) as i64).rem_euclid(n as i64) as usize;
            let v = ((self.cells[row.start + j] - best) as f64).exp();
            sw += v;
            so += v * o as f64;
        }
        let phase = ((i_best as f64 + so / sw.max(1e-30)) / n as f64).rem_euclid(1.0);
        // Refine tempo over neighbouring rows near that phase.
        let (mut tw, mut tl) = (0.0f64, 0.0f64);
        let lo = br.saturating_sub(4);
        let hi = (br + 4).min(self.rows.len() - 1);
        for r in lo..=hi {
            let row = &self.rows[r];
            let m = row.n as i64;
            let c = (phase * m as f64).round() as i64;
            let ww = ((0.025 * m as f64 / row.log_period.exp()).ceil() as i64).max(2);
            let mut mx = f32::MIN;
            for o in -ww..=ww {
                let i = (c + o).rem_euclid(m);
                let j = (i - (f % m as u64) as i64).rem_euclid(m) as usize;
                mx = mx.max(self.cells[row.start + j]);
            }
            let v = ((mx - best) as f64).exp();
            tw += v;
            tl += v * row.log_period;
        }
        let period = (tl / tw.max(1e-30)).exp();
        let e = Estimate { phase, period };
        self.cached = Some(e);
        e
    }

    /// Share of the probability close to the best estimate, 0..1.
    pub fn confidence(&mut self) -> f32 {
        if let Some(c) = self.cached_conf {
            return c;
        }
        let est = self.estimate();
        let max = self.cells.iter().copied().fold(f32::MIN, f32::max);
        let f = self.frame;
        let (mut total, mut near) = (0.0f64, 0.0f64);
        let lp = est.period.ln();
        for row in &self.rows {
            let n = row.n;
            let close_tempo = (row.log_period - lp).abs() < 0.03;
            let s = (f % n as u64) as usize;
            for j in 0..n {
                let v = ((self.cells[row.start + j] - max) as f64).exp();
                total += v;
                if close_tempo {
                    let i = (j + s) % n;
                    let mut d = i as f64 / n as f64 - est.phase;
                    d -= d.round();
                    if (d * row.log_period.exp()).abs() < 0.03 {
                        near += v;
                    }
                }
            }
        }
        let c = (near / total.max(1e-30)) as f32;
        self.cached_conf = Some(c);
        c
    }
}

fn kernel_value(phase: f64, period: f64) -> f32 {
    let sigma = KERNEL_SIGMA_S;
    let g = |d: f64| (-0.5 * (d * period / sigma).powi(2)).exp();
    let beat = g(phase.min(1.0 - phase));
    let half = g((phase - 0.5).abs());
    let quarter = g((phase - 0.25).abs().min((phase - 0.75).abs()));
    let near = beat.max(half).max(quarter);
    (beat + HALF_WEIGHT as f64 * half + QUARTER_WEIGHT as f64 * quarter - OFF_PENALTY as f64 * (1.0 - near)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(tr: &mut Tracker, hop: f64, secs: f64, beat_at: impl Fn(f64) -> Option<f32>) {
        let frames = (secs / hop) as usize;
        for _ in 0..frames {
            let t = tr.frame as f64 * hop;
            tr.step(beat_at(t).unwrap_or(0.0));
        }
    }

    #[test]
    fn locks_to_steady_beat() {
        let hop = 0.0025;
        let mut tr = Tracker::new(hop, 60.0, 200.0, 5.0, 3.0);
        let period = 0.5; // 120 BPM
        tr.init(0.0, 0.52, 0.03, 0.05);
        run(&mut tr, hop, 10.0, |t| {
            let p = (t / period).fract();
            (p < hop / period).then_some(1.0)
        });
        let e = tr.estimate();
        assert!((e.bpm() - 120.0).abs() < 1.0, "bpm {}", e.bpm());
        let mut d = e.phase;
        d -= d.round();
        assert!((d * period).abs() < 0.006, "phase {}", e.phase);
        assert!(tr.confidence() > 0.3, "conf {}", tr.confidence());
    }

    #[test]
    fn eighth_notes_do_not_double_tempo() {
        let hop = 0.0025;
        let mut tr = Tracker::new(hop, 60.0, 200.0, 5.0, 3.0);
        let period = 0.6; // 100 BPM
        tr.init(0.0, period, 0.02, 0.03);
        run(&mut tr, hop, 20.0, |t| {
            let p = (t / (period / 2.0)).fract();
            (p < hop / (period / 2.0)).then_some(1.0)
        });
        let e = tr.estimate();
        assert!((e.bpm() - 100.0).abs() < 2.0, "bpm {}", e.bpm());
    }
}
