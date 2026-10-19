//! Audio front end: per-channel filtering, envelopes and per-frame features.
//!
//! Audio is cut into short frames ("hops", about 2.5 ms). For every frame and
//! channel we compute the level of the whole signal and of eight frequency
//! bands, and how much each level rose compared with the recent past. The
//! full-band rise is the default hit strength; the band rises and band shape
//! are the features the calibrated detector compares.

use crate::settings::ChannelSettings;

/// Target frame duration in seconds.
pub const HOP_SECONDS: f64 = 0.0025;
/// Window the rise is measured against, in seconds.
const RISE_WINDOW_S: f64 = 0.015;
/// Centre frequencies of the analysis bands (about one octave apart).
pub const BAND_HZ: [f32; 8] = [60.0, 120.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0];
/// Number of analysis bands per channel.
pub const BANDS: usize = BAND_HZ.len();
/// Envelopes per channel: full band plus the analysis bands.
const ENVS: usize = BANDS + 1;
/// Detector features per channel: band rises, band shape, level above floor.
pub const FEATURES_PER_CHANNEL: usize = 2 * BANDS + 1;
/// Samples kept for the spectrum view.
pub const SPECTRUM_SIZE: usize = 4096;

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
        let (cos, alpha) = Self::params(sr, hz, std::f64::consts::FRAC_1_SQRT_2);
        Self::new((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    pub fn highpass(sr: f32, hz: f32) -> Self {
        let (cos, alpha) = Self::params(sr, hz, std::f64::consts::FRAC_1_SQRT_2);
        Self::new((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    /// Band-pass with 0 dB peak gain.
    pub fn bandpass(sr: f32, hz: f32, q: f64) -> Self {
        let (cos, alpha) = Self::params(sr, hz, q);
        Self::new(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    fn params(sr: f32, hz: f32, q: f64) -> (f64, f64) {
        let hz = (hz as f64).clamp(5.0, sr as f64 * 0.45);
        let w = 2.0 * std::f64::consts::PI * hz / sr as f64;
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
    fn new(sr: f32, release_s: f32) -> Self {
        Self {
            value: 0.0,
            attack: 1.0 - (-1.0 / (0.0005 * sr)).exp(),
            release: (-1.0 / (release_s * sr)).exp(),
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
    bands: [Biquad; BANDS],
    env: [Envelope; ENVS],
    /// Recent frame levels, ring buffer.
    history: Vec<[f32; ENVS]>,
    history_pos: usize,
    noise_floor: f32,
    initialised: bool,
    peak: f32,
}

/// Features of one channel for the most recent frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelFrame {
    /// Full-band level in dB.
    pub level: f32,
    /// Rise of the full-band level over the recent past, in dB, never negative.
    pub rise: f32,
    /// Band levels in dB.
    pub band_level: [f32; BANDS],
    /// Band level rises in dB.
    pub band_rise: [f32; BANDS],
    /// Full-band level above the channel's noise floor, in dB.
    pub above_floor: f32,
}

impl ChannelFrame {
    pub fn features(&self, out: &mut Vec<f32>) {
        out.extend_from_slice(&self.band_rise);
        let top = self.band_level.iter().copied().fold(f32::MIN, f32::max);
        out.extend(self.band_level.iter().map(|l| (l - top).max(-60.0)));
        out.push(self.above_floor);
    }

    /// Sum of the band rises: how much "started" in this frame.
    pub fn flux(&self) -> f32 {
        self.band_rise.iter().sum()
    }
}

/// Raw and filtered samples of one channel, for the spectrum view.
#[derive(Clone, Debug)]
pub struct SpectrumTap {
    pub channel: usize,
    pub raw: Vec<f32>,
    pub filtered: Vec<f32>,
    pub pos: usize,
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
    pub tap: Option<SpectrumTap>,
}

impl FrontEnd {
    pub fn new(sample_rate: f32, channel_settings: &[ChannelSettings]) -> Self {
        let hop = ((sample_rate as f64 * HOP_SECONDS).round() as usize).max(8);
        let hist = ((RISE_WINDOW_S * sample_rate as f64 / hop as f64).ceil() as usize).max(1);
        let channels = channel_settings
            .iter()
            .map(|cs| {
                let mut env = [Envelope::new(sample_rate, 0.030); ENVS];
                // Low bands need a longer release to stay smooth.
                for (i, hz) in BAND_HZ.iter().enumerate() {
                    env[i + 1] = Envelope::new(sample_rate, (3.0 / hz).max(0.010));
                }
                ChannelDsp {
                    highpass: (cs.highpass_hz > 0.0).then(|| Biquad::highpass(sample_rate, cs.highpass_hz)),
                    lowpass: (cs.lowpass_hz > 0.0).then(|| Biquad::lowpass(sample_rate, cs.lowpass_hz)),
                    bands: BAND_HZ.map(|hz| Biquad::bandpass(sample_rate, hz, 1.4)),
                    env,
                    history: vec![[-120.0; ENVS]; hist],
                    history_pos: 0,
                    noise_floor: -120.0,
                    initialised: false,
                    peak: 0.0,
                }
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
            tap: None,
        }
    }

    pub fn hop_seconds(&self) -> f64 {
        self.hop as f64 / self.sample_rate as f64
    }

    /// Total samples (per channel) pushed so far.
    pub fn samples(&self) -> u64 {
        self.frame_index * self.hop as u64 + self.counter as u64
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

    /// Start or move the spectrum tap.
    pub fn set_tap(&mut self, channel: usize) {
        if channel >= self.channels.len() {
            self.tap = None;
        } else if self.tap.as_ref().is_none_or(|t| t.channel != channel) {
            self.tap = Some(SpectrumTap {
                channel,
                raw: vec![0.0; SPECTRUM_SIZE],
                filtered: vec![0.0; SPECTRUM_SIZE],
                pos: 0,
            });
        }
    }

    /// Feed one sample per channel. Returns true when a frame completed; the
    /// frame's features are then in `self.frames`.
    #[inline]
    pub fn push(&mut self, samples: &[f32]) -> bool {
        for (ci, (ch, &raw)) in self.channels.iter_mut().zip(samples).enumerate() {
            let raw = if raw.is_finite() { raw } else { 0.0 };
            let mut x = raw;
            if let Some(f) = ch.highpass.as_mut() {
                x = f.process(x);
            }
            if let Some(f) = ch.lowpass.as_mut() {
                x = f.process(x);
            }
            ch.env[0].process(x);
            for b in 0..BANDS {
                let y = ch.bands[b].process(x);
                ch.env[b + 1].process(y);
            }
            let a = x.abs();
            if a > ch.peak {
                ch.peak = a;
            }
            if let Some(t) = self.tap.as_mut() {
                if t.channel == ci {
                    t.raw[t.pos] = raw;
                    t.filtered[t.pos] = x;
                    t.pos = (t.pos + 1) % SPECTRUM_SIZE;
                }
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
            let mut level = [0.0f32; ENVS];
            for (l, e) in level.iter_mut().zip(&ch.env) {
                *l = 20.0 * (e.value + 1e-6).log10();
            }
            if !ch.initialised {
                ch.initialised = true;
                ch.noise_floor = level[0];
                for h in ch.history.iter_mut() {
                    *h = level;
                }
            }
            let mut rise = [0.0f32; ENVS];
            for b in 0..ENVS {
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
            let mut band_level = [0.0; BANDS];
            let mut band_rise = [0.0; BANDS];
            band_level.copy_from_slice(&level[1..]);
            band_rise.copy_from_slice(&rise[1..]);
            *out = ChannelFrame {
                level: level[0],
                rise: rise[0],
                band_level,
                band_rise,
                above_floor: level[0] - ch.noise_floor,
            };
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

    /// Detector features of all channels for the current frame.
    pub fn features(&self, out: &mut Vec<f32>) {
        out.clear();
        for f in &self.frames {
            f.features(out);
        }
    }
}

/// Magnitude spectrum in dB on `bins` log-spaced frequencies from `lo` to `hi`
/// Hz, from a ring buffer whose oldest sample is at `pos`.
pub fn spectrum(ring: &[f32], pos: usize, sample_rate: f32, lo: f32, hi: f32, bins: usize) -> Vec<f32> {
    let n = ring.len();
    let mut re: Vec<f32> = (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos();
            ring[(pos + i) % n] * w
        })
        .collect();
    let mut im = vec![0.0f32; n];
    fft(&mut re, &mut im);
    let power: Vec<f32> = (0..n / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect();
    let hz_per_bin = sample_rate / n as f32;
    let ratio = (hi / lo).powf(1.0 / bins as f32);
    // Window gain correction so a full-scale sine reads near 0 dB.
    let norm = 16.0 / (n as f32 * n as f32);
    (0..bins)
        .map(|b| {
            let f0 = lo * ratio.powi(b as i32);
            let f1 = f0 * ratio;
            let k0 = ((f0 / hz_per_bin).floor() as usize).clamp(1, n / 2 - 1);
            let k1 = ((f1 / hz_per_bin).ceil() as usize).clamp(k0 + 1, n / 2);
            let p = power[k0..k1].iter().copied().fold(0.0f32, f32::max);
            10.0 * (p * norm + 1e-14).log10()
        })
        .collect()
}

/// In-place radix-2 FFT; the length must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let (wr, wi) = (ang.cos() as f32, ang.sin() as f32);
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
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
        let mut max_flux = 0.0f32;
        for i in 0..(sr as usize) {
            let t = i as f32 / sr;
            let x = if (0.5..0.55).contains(&t) {
                (t * 2000.0).sin() * (-(t - 0.5) * 60.0).exp()
            } else {
                0.0005 * ((i * 7919 % 1000) as f32 / 500.0 - 1.0)
            };
            if fe.push(&[x]) {
                max_rise = max_rise.max(fe.frames[0].rise);
                max_flux = max_flux.max(fe.frames[0].flux());
            }
        }
        assert!(max_rise > 20.0, "rise {max_rise}");
        assert!(max_flux > 20.0, "flux {max_flux}");
    }

    #[test]
    fn spectrum_finds_a_tone() {
        let sr = 48000.0;
        let ring: Vec<f32> = (0..SPECTRUM_SIZE).map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin()).collect();
        let s = spectrum(&ring, 0, sr, 20.0, 20000.0, 90);
        let (imax, vmax) = s.iter().enumerate().fold((0, f32::MIN), |a, (i, &v)| if v > a.1 { (i, v) } else { a });
        let f = 20.0 * (1000.0f32).powf(imax as f32 / 90.0);
        assert!((f / 1000.0 - 1.0).abs() < 0.1, "peak at {f} Hz");
        assert!(vmax > -6.0 && vmax < 3.0, "level {vmax}");
    }
}
