//! Audio front end: per-channel filtering, envelopes and per-frame features.
//!
//! Audio is cut into short frames ("hops", about 2.5 ms). For every frame and
//! channel we compute the level of three bands (full, low, high) and how much
//! each level rose compared with the recent past. The rise is the raw
//! "hit strength" signal; the levels feed the hit/noise classifier.

use crate::settings::ChannelSettings;

/// Target frame duration in seconds.
pub const HOP_SECONDS: f64 = 0.0025;
/// Window the rise is measured against, in seconds.
const RISE_WINDOW_S: f64 = 0.015;
const LOW_BAND_HZ: f32 = 200.0;
const HIGH_BAND_HZ: f32 = 2500.0;
/// Number of bands per channel.
pub const BANDS: usize = 3;
/// Number of classifier features per channel.
pub const FEATURES_PER_CHANNEL: usize = 6;

#[derive(Clone, Copy, Debug, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn new(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: (b0 / a0) as f32,
            b1: (b1 / a0) as f32,
            b2: (b2 / a0) as f32,
            a1: (a1 / a0) as f32,
            a2: (a2 / a0) as f32,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn lowpass(sr: f32, hz: f32) -> Self {
        let (cos, alpha) = Self::params(sr, hz);
        Self::new((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    pub fn highpass(sr: f32, hz: f32) -> Self {
        let (cos, alpha) = Self::params(sr, hz);
        Self::new((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    fn params(sr: f32, hz: f32) -> (f64, f64) {
        let hz = (hz as f64).clamp(5.0, sr as f64 * 0.45);
        let w = 2.0 * std::f64::consts::PI * hz / sr as f64;
        let q = std::f64::consts::FRAC_1_SQRT_2;
        (w.cos(), w.sin() / (2.0 * q))
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        // Transposed direct form II.
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        // Flush denormals.
        if self.z1.abs() < 1e-20 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-20 {
            self.z2 = 0.0;
        }
        y
    }
}

/// Peak envelope follower: fast attack, slower release.
#[derive(Clone, Copy, Debug, Default)]
struct Envelope {
    value: f32,
    attack: f32,
    release: f32,
}

impl Envelope {
    fn new(sr: f32) -> Self {
        Self {
            value: 0.0,
            attack: 1.0 - (-1.0 / (0.0005 * sr)).exp(),
            release: (-1.0 / (0.030 * sr)).exp(),
        }
    }

    #[inline]
    fn process(&mut self, x: f32) {
        let a = x.abs();
        if a > self.value {
            self.value += self.attack * (a - self.value);
        } else {
            self.value *= self.release;
        }
    }
}

#[derive(Clone, Debug)]
struct ChannelDsp {
    highpass: Option<Biquad>,
    lowpass: Option<Biquad>,
    low_band: Biquad,
    high_band: Biquad,
    env: [Envelope; BANDS],
    /// Recent frame levels per band, ring buffer.
    history: Vec<[f32; BANDS]>,
    history_pos: usize,
    noise_floor: f32,
    initialised: bool,
    peak: f32,
}

/// Features of one channel for the most recent frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelFrame {
    /// Band levels in dB (full, low, high).
    pub level: [f32; BANDS],
    /// Rise of each band level over the recent past, in dB, never negative.
    pub rise: [f32; BANDS],
    /// Full-band level above the channel's noise floor, in dB.
    pub above_floor: f32,
}

impl ChannelFrame {
    pub fn features(&self, out: &mut Vec<f32>) {
        out.extend_from_slice(&self.rise);
        out.push(self.above_floor);
        out.push(self.level[1] - self.level[0]);
        out.push(self.level[2] - self.level[0]);
    }
}

#[derive(Clone, Debug)]
pub struct FrontEnd {
    pub sample_rate: f32,
    /// Frame length in samples.
    pub hop: usize,
    channels: Vec<ChannelDsp>,
    counter: usize,
    /// Number of completed frames.
    pub frame_index: u64,
    pub frames: Vec<ChannelFrame>,
    floor_rise_per_frame: f32,
}

impl FrontEnd {
    pub fn new(sample_rate: f32, channel_settings: &[ChannelSettings]) -> Self {
        let hop = ((sample_rate as f64 * HOP_SECONDS).round() as usize).max(8);
        let hist = ((RISE_WINDOW_S * sample_rate as f64 / hop as f64).ceil() as usize).max(1);
        let channels = channel_settings
            .iter()
            .map(|cs| ChannelDsp {
                highpass: (cs.highpass_hz > 0.0).then(|| Biquad::highpass(sample_rate, cs.highpass_hz)),
                lowpass: (cs.lowpass_hz > 0.0).then(|| Biquad::lowpass(sample_rate, cs.lowpass_hz)),
                low_band: Biquad::lowpass(sample_rate, LOW_BAND_HZ),
                high_band: Biquad::highpass(sample_rate, HIGH_BAND_HZ),
                env: [Envelope::new(sample_rate); BANDS],
                history: vec![[-120.0; BANDS]; hist],
                history_pos: 0,
                noise_floor: -120.0,
                initialised: false,
                peak: 0.0,
            })
            .collect::<Vec<_>>();
        let n = channels.len();
        let hop_s = hop as f32 / sample_rate;
        Self {
            sample_rate,
            hop,
            channels,
            counter: 0,
            frame_index: 0,
            frames: vec![ChannelFrame::default(); n],
            // The floor creeps up by 1 dB per second when the signal stays above it.
            floor_rise_per_frame: hop_s,
        }
    }

    pub fn hop_seconds(&self) -> f64 {
        self.hop as f64 / self.sample_rate as f64
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Update user filters without resetting the rest of the state.
    pub fn update_filters(&mut self, channel_settings: &[ChannelSettings]) {
        let sr = self.sample_rate;
        for (ch, cs) in self.channels.iter_mut().zip(channel_settings) {
            ch.highpass = (cs.highpass_hz > 0.0).then(|| Biquad::highpass(sr, cs.highpass_hz));
            ch.lowpass = (cs.lowpass_hz > 0.0).then(|| Biquad::lowpass(sr, cs.lowpass_hz));
        }
    }

    /// Feed one sample per channel. Returns true when a frame completed; the
    /// frame's features are then in `self.frames`.
    #[inline]
    pub fn push(&mut self, samples: &[f32]) -> bool {
        for (ch, &raw) in self.channels.iter_mut().zip(samples) {
            let mut x = if raw.is_finite() { raw } else { 0.0 };
            if let Some(f) = ch.highpass.as_mut() {
                x = f.process(x);
            }
            if let Some(f) = ch.lowpass.as_mut() {
                x = f.process(x);
            }
            let low = ch.low_band.process(x);
            let high = ch.high_band.process(x);
            ch.env[0].process(x);
            ch.env[1].process(low);
            ch.env[2].process(high);
            let a = x.abs();
            if a > ch.peak {
                ch.peak = a;
            }
        }
        self.counter += 1;
        if self.counter < self.hop {
            return false;
        }
        self.counter = 0;
        self.frame_index += 1;
        let rise_rate = self.floor_rise_per_frame;
        for (ch, out) in self.channels.iter_mut().zip(self.frames.iter_mut()) {
            let mut level = [0.0f32; BANDS];
            for b in 0..BANDS {
                level[b] = 20.0 * (ch.env[b].value + 1e-6).log10();
            }
            if !ch.initialised {
                ch.initialised = true;
                ch.noise_floor = level[0];
                for h in ch.history.iter_mut() {
                    *h = level;
                }
            }
            let mut rise = [0.0f32; BANDS];
            for b in 0..BANDS {
                let past = ch.history.iter().map(|h| h[b]).fold(f32::MIN, f32::max);
                rise[b] = (level[b] - past).max(0.0);
            }
            ch.history[ch.history_pos] = level;
            ch.history_pos = (ch.history_pos + 1) % ch.history.len();
            if level[0] < ch.noise_floor {
                ch.noise_floor += (level[0] - ch.noise_floor) * 0.3;
            } else {
                ch.noise_floor += rise_rate;
            }
            *out = ChannelFrame { level, rise, above_floor: level[0] - ch.noise_floor };
        }
        true
    }

    /// Peak level per channel in dBFS since the last call, then resets.
    pub fn take_peaks(&mut self) -> Vec<f32> {
        self.channels
            .iter_mut()
            .map(|c| {
                let p = c.peak;
                c.peak = 0.0;
                20.0 * (p + 1e-9).log10()
            })
            .collect()
    }

    /// Classifier features of all channels for the current frame.
    pub fn features(&self, out: &mut Vec<f32>) {
        out.clear();
        for f in &self.frames {
            f.features(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rise_follows_a_hit() {
        let sr = 48000.0;
        let mut fe = FrontEnd::new(sr, &[ChannelSettings::default()]);
        let mut max_rise = 0.0f32;
        for i in 0..(sr as usize) {
            let t = i as f32 / sr;
            let x = if (0.5..0.55).contains(&t) {
                (t * 2000.0).sin() * (-(t - 0.5) * 60.0).exp()
            } else {
                0.0005 * ((i * 7919 % 1000) as f32 / 500.0 - 1.0)
            };
            if fe.push(&[x]) {
                max_rise = max_rise.max(fe.frames[0].rise[0]);
            }
        }
        assert!(max_rise > 20.0, "rise {max_rise}");
    }
}
