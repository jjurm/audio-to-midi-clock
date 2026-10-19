//! Calibration: recording sweeps and the nearest-match hit detector.
//!
//! Recording works in sweeps of fixed length. In a hit take the player hits
//! once per sweep, when the moving cursor crosses the centre. No peak
//! detection is involved: every frame of the sweep is kept, and afterwards
//! the frame where the most "started" near the centre becomes the hit
//! example. "Started" is the rise of each band, scaled down by how busy that
//! band was at the edges of the same sweep, so noisy bands count less. Frames
//! right around the hit are left out, everything else in the sweep supplies
//! noise examples. A noise take turns whole sweeps into noise examples.
//!
//! The detector keeps the examples as they are and scores a frame by how much
//! closer it is to some hit example than to any noise example, so any number
//! of different sounds can count as hits.

use crate::dsp::{BANDS, FEATURES_PER_CHANNEL};
use crate::settings::Example;

/// Length of one sweep in seconds.
pub const SWEEP_S: f64 = 2.0;
/// Points in a sweep's level trace.
pub const TRACE_POINTS: usize = 200;
/// Default half-width of the zone around the centre where the hit is looked for.
const DEFAULT_TOLERANCE_S: f64 = 0.3;
/// Frames before and after the hit that are neither hit nor noise.
const GUARD_BEFORE: usize = 2;
const GUARD_AFTER: usize = 2;
/// Window around the hit that is ignored when looking for a competing peak.
const COMPETE_BEFORE_S: f64 = 0.02;
const COMPETE_AFTER_S: f64 = 0.06;
/// Noise examples kept per sweep: the busiest frames plus evenly spread ones.
const HARD_NEGATIVES: usize = 15;
const SPREAD_NEGATIVES: usize = 8;
pub const MAX_HITS: usize = 300;
pub const MAX_NOISE: usize = 1500;
/// Examples needed before the detector is used.
pub const MIN_HITS: usize = 3;
pub const MIN_NOISE: usize = 10;
/// Sweeps kept for review.
pub const HISTORY: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    Hits,
    Noise,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepStatus {
    /// A hit was found and saved.
    Accepted,
    /// The player chose the hit by hand.
    Chosen,
    /// Nothing near the centre stood out compared with the other hits of the take.
    Missed,
    /// Something else in the sweep was as strong as the hit.
    Ambiguous,
    /// Part of a noise take.
    Noise,
}

impl SweepStatus {
    pub fn name(self) -> &'static str {
        match self {
            SweepStatus::Accepted => "accepted",
            SweepStatus::Chosen => "chosen",
            SweepStatus::Missed => "missed",
            SweepStatus::Ambiguous => "ambiguous",
            SweepStatus::Noise => "noise",
        }
    }

    pub fn saved(self) -> bool {
        !matches!(self, SweepStatus::Missed | SweepStatus::Ambiguous)
    }
}

#[derive(Clone, Debug)]
pub struct Sweep {
    pub id: u64,
    pub take: Take,
    pub status: SweepStatus,
    /// Frame index of the hit within the sweep.
    pub winner: Option<usize>,
    /// Level trace in dB, `TRACE_POINTS` long.
    pub trace: Vec<f32>,
    feats: Vec<Vec<f32>>,
    flux: Vec<f32>,
    /// Flux scaled by how busy each band was at the sweep's edges.
    norm: Vec<f32>,
}

impl Sweep {
    pub fn frames(&self) -> usize {
        self.feats.len()
    }

    /// Examples this sweep contributes.
    pub fn examples(&self) -> Vec<Example> {
        let mut out = Vec::new();
        if !self.status.saved() {
            return out;
        }
        let n = self.frames();
        let excluded = |i: usize| match self.winner {
            Some(w) => i + GUARD_BEFORE >= w && i <= w + GUARD_AFTER,
            None => false,
        };
        if let Some(w) = self.winner {
            out.push(Example { hit: true, strength: self.flux[w], features: self.feats[w].clone(), sweep: self.id });
        }
        let mut order: Vec<usize> = (0..n).filter(|&i| !excluded(i)).collect();
        order.sort_by(|&a, &b| self.flux[b].total_cmp(&self.flux[a]));
        let hard = if self.take == Take::Noise { 2 * HARD_NEGATIVES } else { HARD_NEGATIVES };
        let mut picked: Vec<usize> = order.iter().copied().take(hard).collect();
        let step = (n / SPREAD_NEGATIVES).max(1);
        for i in (step / 2..n).step_by(step) {
            if !excluded(i) && !picked.contains(&i) {
                picked.push(i);
            }
        }
        for i in picked {
            out.push(Example { hit: false, strength: self.flux[i], features: self.feats[i].clone(), sweep: self.id });
        }
        out
    }
}

/// Collects frames into sweeps during a take.
#[derive(Clone, Debug)]
pub struct Recorder {
    pub take: Take,
    hop_s: f64,
    frames_per_sweep: usize,
    feats: Vec<Vec<f32>>,
    flux: Vec<f32>,
    level: Vec<f32>,
    /// Statistics of this take's accepted sweeps.
    winner_norms: Vec<f32>,
    ratios: Vec<f32>,
    offsets: Vec<f64>,
}

impl Recorder {
    pub fn new(take: Take, hop_s: f64) -> Self {
        Self {
            take,
            hop_s,
            frames_per_sweep: (SWEEP_S / hop_s).round() as usize,
            feats: Vec::new(),
            flux: Vec::new(),
            level: Vec::new(),
            winner_norms: Vec::new(),
            ratios: Vec::new(),
            offsets: Vec::new(),
        }
    }

    /// Progress through the current sweep, 0..1.
    pub fn position(&self) -> f64 {
        self.feats.len() as f64 / self.frames_per_sweep as f64
    }

    /// Half-width of the zone around the centre where the hit is looked
    /// for. It narrows to fit how close to the centre the player actually hits.
    pub fn tolerance_s(&self) -> f64 {
        if self.offsets.len() < 5 {
            return DEFAULT_TOLERANCE_S;
        }
        let n = self.offsets.len() as f64;
        let mean = self.offsets.iter().sum::<f64>() / n;
        let sd = (self.offsets.iter().map(|o| (o - mean).powi(2)).sum::<f64>() / n).sqrt();
        (mean.abs() + 3.0 * sd + 0.03).clamp(0.12, DEFAULT_TOLERANCE_S)
    }

    pub fn trace(&self) -> Vec<f32> {
        make_trace(&self.level, self.frames_per_sweep)
    }

    /// Add a frame; returns the finished sweep at the end of each sweep.
    pub fn push(&mut self, feats: &[f32], flux: f32, level: f32, next_id: &mut u64) -> Option<Sweep> {
        self.feats.push(feats.to_vec());
        self.flux.push(flux);
        self.level.push(level);
        if self.feats.len() < self.frames_per_sweep {
            return None;
        }
        let feats = std::mem::take(&mut self.feats);
        let flux = std::mem::take(&mut self.flux);
        let level = std::mem::take(&mut self.level);
        *next_id += 1;
        let tol = self.tolerance_s();
        let norm = normalized_flux(&feats, self.hop_s, tol);
        let mut sweep = Sweep {
            id: *next_id,
            take: self.take,
            status: SweepStatus::Noise,
            winner: None,
            trace: make_trace(&level, self.frames_per_sweep),
            feats,
            flux,
            norm,
        };
        if self.take == Take::Hits {
            self.judge(&mut sweep, tol);
        }
        Some(sweep)
    }

    fn judge(&mut self, s: &mut Sweep, tol: f64) {
        let n = s.frames();
        let centre = n / 2;
        let half = (tol / self.hop_s) as usize;
        let lo = centre.saturating_sub(half);
        let hi = (centre + half).min(n - 1);
        let w = (lo..=hi).max_by(|&a, &b| s.norm[a].total_cmp(&s.norm[b])).unwrap_or(centre);
        s.winner = Some(w);
        let before = (COMPETE_BEFORE_S / self.hop_s) as usize;
        let after = (COMPETE_AFTER_S / self.hop_s) as usize;
        let rival = (0..n)
            .filter(|&i| i + before < w || i > w + after)
            .map(|i| s.norm[i])
            .fold(0.0f32, f32::max);
        let ratio = s.norm[w] / rival.max(1e-3);
        let missed = self.winner_norms.len() >= MIN_HITS && s.norm[w] < 0.3 * median(&self.winner_norms);
        let ambiguous = ratio < 1.0 || (self.ratios.len() >= MIN_HITS && ratio < 0.5 * median(&self.ratios));
        s.status = if missed {
            SweepStatus::Missed
        } else if ambiguous {
            SweepStatus::Ambiguous
        } else {
            self.winner_norms.push(s.norm[w]);
            self.ratios.push(ratio);
            self.offsets.push((w as f64 - centre as f64) * self.hop_s);
            SweepStatus::Accepted
        };
    }
}

/// Let the player pick the hit: the strongest frame near the given position (0..1).
pub fn choose(s: &mut Sweep, position: f64, hop_s: f64) {
    let n = s.frames();
    let c = ((position.clamp(0.0, 1.0) * n as f64) as usize).min(n - 1);
    let r = (0.03 / hop_s) as usize;
    let w = (c.saturating_sub(r)..=(c + r).min(n - 1)).max_by(|&a, &b| s.norm[a].total_cmp(&s.norm[b])).unwrap_or(c);
    s.winner = Some(w);
    s.status = SweepStatus::Chosen;
}

fn make_trace(level: &[f32], frames_per_sweep: usize) -> Vec<f32> {
    let per = (frames_per_sweep as f64 / TRACE_POINTS as f64).max(1.0);
    let points = ((level.len() as f64 / per).ceil() as usize).min(TRACE_POINTS);
    (0..points)
        .map(|p| {
            let a = (p as f64 * per) as usize;
            let b = (((p + 1) as f64 * per) as usize).min(level.len()).max(a + 1);
            level[a..b].iter().copied().fold(-120.0, f32::max)
        })
        .collect()
}

/// Sum of band rises per frame, each band scaled by its average rise at the
/// edges of the sweep (outside the centre zone).
fn normalized_flux(feats: &[Vec<f32>], hop_s: f64, tol: f64) -> Vec<f32> {
    let n = feats.len();
    let dims = feats.first().map(|f| f.len()).unwrap_or(0);
    let channels = dims / FEATURES_PER_CHANNEL;
    let edge = ((tol + 0.1) / hop_s) as usize;
    let centre = n / 2;
    let is_edge = |i: usize| i + edge < centre || i > centre + edge;
    let mut busy = vec![0.0f32; dims];
    let mut count = 0.0f32;
    for (i, f) in feats.iter().enumerate() {
        if is_edge(i) {
            for c in 0..channels {
                for b in 0..BANDS {
                    let d = c * FEATURES_PER_CHANNEL + b;
                    busy[d] += f[d];
                }
            }
            count += 1.0;
        }
    }
    let scale: Vec<f32> = busy.iter().map(|b| 1.0 / (1.0 + b / count.max(1.0))).collect();
    feats
        .iter()
        .map(|f| {
            let mut s = 0.0;
            for c in 0..channels {
                for b in 0..BANDS {
                    let d = c * FEATURES_PER_CHANNEL + b;
                    s += f[d] * scale[d];
                }
            }
            s
        })
        .collect()
}

fn median(v: &[f32]) -> f32 {
    let mut s = v.to_vec();
    s.sort_by(f32::total_cmp);
    s[s.len() / 2]
}

/// Nearest-match detector over the saved examples.
#[derive(Clone, Debug)]
pub struct Detector {
    mean: Vec<f32>,
    scale: Vec<f32>,
    hits: Vec<Vec<f32>>,
    noise: Vec<Vec<f32>>,
    /// Typical flux of a hit.
    pub reference: f32,
    /// Frames with less flux than this are not scored.
    pub gate: f32,
}

impl Detector {
    pub fn train(examples: &[Example], dims: usize) -> Option<Self> {
        let usable: Vec<&Example> = examples.iter().filter(|e| e.features.len() == dims).collect();
        let hit_count = usable.iter().filter(|e| e.hit).count();
        if dims == 0 || hit_count < MIN_HITS || usable.len() - hit_count < MIN_NOISE {
            return None;
        }
        let n = usable.len() as f32;
        let mut mean = vec![0.0f32; dims];
        for e in &usable {
            for (m, x) in mean.iter_mut().zip(&e.features) {
                *m += x / n;
            }
        }
        let mut var = vec![0.0f32; dims];
        for e in &usable {
            for ((v, x), m) in var.iter_mut().zip(&e.features).zip(&mean) {
                *v += (x - m).powi(2) / n;
            }
        }
        // At least a few dB of spread per dimension, so one quiet dimension
        // can't dominate the distance.
        let scale: Vec<f32> = var.iter().map(|v| 1.0 / v.sqrt().max(3.0)).collect();
        let z = |f: &[f32]| f.iter().zip(&mean).zip(&scale).map(|((x, m), s)| (x - m) * s).collect::<Vec<f32>>();
        let hits: Vec<Vec<f32>> = usable.iter().filter(|e| e.hit).map(|e| z(&e.features)).collect();
        let noise: Vec<Vec<f32>> = usable.iter().filter(|e| !e.hit).map(|e| z(&e.features)).collect();
        let strengths: Vec<f32> = usable.iter().filter(|e| e.hit).map(|e| e.strength).collect();
        let mut sorted = strengths.clone();
        sorted.sort_by(f32::total_cmp);
        let reference = median(&strengths).max(1.0);
        let gate = (0.5 * sorted[sorted.len() / 10]).max(0.5);
        Some(Self { mean, scale, hits, noise, reference, gate })
    }

    /// How hit-like the features are: 0 = like noise, 1 = like a hit, 0.5 = undecided.
    pub fn score(&self, features: &[f32]) -> f32 {
        if features.len() != self.mean.len() {
            return 0.0;
        }
        let z: Vec<f32> = features.iter().zip(&self.mean).zip(&self.scale).map(|((x, m), s)| (x - m) * s).collect();
        let d_hit = nearest(&self.hits, &z);
        let d_noise = nearest(&self.noise, &z);
        d_noise / (d_hit + d_noise).max(1e-6)
    }
}

/// Mean distance to the two nearest examples.
fn nearest(set: &[Vec<f32>], z: &[f32]) -> f32 {
    let (mut a, mut b) = (f32::MAX, f32::MAX);
    for e in set {
        let d: f32 = e.iter().zip(z).map(|(p, q)| (p - q) * (p - q)).sum();
        if d < a {
            b = a;
            a = d;
        } else if d < b {
            b = d;
        }
    }
    if b == f32::MAX { a.sqrt() } else { (a.sqrt() + b.sqrt()) / 2.0 }
}

/// Keep the newest examples within the limits.
pub fn trim(examples: &mut Vec<Example>) {
    for (class, max) in [(true, MAX_HITS), (false, MAX_NOISE)] {
        let count = examples.iter().filter(|e| e.hit == class).count();
        if count > max {
            let mut drop = count - max;
            examples.retain(|e| {
                if drop > 0 && e.hit == class {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(hit: bool, f: Vec<f32>) -> Example {
        Example { hit, strength: 10.0, features: f, sweep: 0 }
    }

    #[test]
    fn nearest_match_handles_two_kinds_of_hits() {
        let mut v = Vec::new();
        for i in 0..6 {
            let j = i as f32 * 0.5;
            v.push(ex(true, vec![30.0 + j, 0.0, 5.0]));
            v.push(ex(true, vec![0.0, 30.0 + j, 5.0]));
        }
        for i in 0..20 {
            let j = i as f32 * 0.4;
            v.push(ex(false, vec![8.0 + j, 8.0 - j * 0.5, 2.0]));
        }
        let d = Detector::train(&v, 3).unwrap();
        assert!(d.score(&[31.0, 0.5, 5.0]) > 0.6);
        assert!(d.score(&[0.5, 31.0, 5.0]) > 0.6);
        assert!(d.score(&[10.0, 7.0, 2.0]) < 0.4);
    }
}
