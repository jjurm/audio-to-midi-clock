//! Everything the user can configure, plus the state that is saved between
//! sessions. The whole `Persist` value is serialised to one JSON document.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ChannelSettings {
    pub enabled: bool,
    /// Relative importance of this channel, 0 ignores it.
    pub weight: f32,
    /// High-pass cutoff in Hz, 0 = off.
    pub highpass_hz: f32,
    /// Low-pass cutoff in Hz, 0 = off.
    pub lowpass_hz: f32,
}

impl Default for ChannelSettings {
    fn default() -> Self {
        Self { enabled: true, weight: 1.0, highpass_hz: 0.0, lowpass_hz: 0.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NoteOutput {
    /// MIDI output port name, empty = none.
    pub port: String,
    /// MIDI channel 1-16.
    pub channel: u8,
    pub note: u8,
    pub velocity: u8,
}

impl Default for NoteOutput {
    fn default() -> Self {
        Self { port: String::new(), channel: 10, note: 37, velocity: 100 }
    }
}

impl NoteOutput {
    pub fn status_on(&self) -> u8 {
        0x90 | (self.channel.clamp(1, 16) - 1)
    }
    pub fn status_off(&self) -> u8 {
        0x80 | (self.channel.clamp(1, 16) - 1)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClickSettings {
    pub enabled: bool,
    pub output: NoteOutput,
    /// Velocity used on beat 1 of the bar.
    pub accent_velocity: u8,
}

impl Default for ClickSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            output: NoteOutput { port: String::new(), channel: 10, note: 37, velocity: 90 },
            accent_velocity: 127,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LatencyTestSettings {
    /// Note sent during the test.
    pub output: NoteOutput,
    /// Input channel (0-based) that hears the sound of that note, None = test disabled.
    pub listen_channel: Option<usize>,
    /// Run the test automatically when the app starts.
    pub run_on_startup: bool,
}

impl Default for LatencyTestSettings {
    fn default() -> Self {
        Self {
            output: NoteOutput { port: String::new(), channel: 10, note: 37, velocity: 127 },
            listen_channel: None,
            run_on_startup: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Binding {
    pub action: String,
    /// "midi:note:<channel>:<note>", "midi:cc:<channel>:<number>", "pad:<button>", "key:<code>"
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Audio input device id as listed by the host, empty = system default.
    pub audio_device: String,
    /// Requested audio buffer size in frames, 0 = device default (desktop only).
    pub buffer_frames: u32,
    pub channels: Vec<ChannelSettings>,
    /// 0-100, higher detects softer hits.
    pub sensitivity: f32,

    /// MIDI output ports that receive the clock.
    pub clock_outputs: Vec<String>,
    pub click: ClickSettings,
    pub latency_test: LatencyTestSettings,

    /// User trim on top of the measured latency, in ms.
    pub offset_trim_ms: f32,
    /// Step for the offset +/- actions, in ms.
    pub offset_step_ms: f32,
    /// How far ahead MIDI events are queued, in ms.
    pub lookahead_ms: f32,

    /// Time after which the influence of what was played halves, in seconds.
    pub half_life_s: f32,
    /// How fast tempo may change, in percent per second.
    pub tempo_drift_pct: f32,
    pub tempo_min_bpm: f32,
    pub tempo_max_bpm: f32,
    pub beats_per_bar: u32,

    /// Hits needed before the clock starts.
    pub count_in_hits: u32,
    pub auto_stop: bool,
    pub auto_stop_after_s: f32,

    pub bindings: Vec<Binding>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio_device: String::new(),
            buffer_frames: 0,
            channels: Vec::new(),
            sensitivity: 50.0,
            clock_outputs: Vec::new(),
            click: ClickSettings::default(),
            latency_test: LatencyTestSettings::default(),
            offset_trim_ms: 0.0,
            offset_step_ms: 2.0,
            lookahead_ms: 60.0,
            half_life_s: 5.0,
            tempo_drift_pct: 3.0,
            tempo_min_bpm: 60.0,
            tempo_max_bpm: 200.0,
            beats_per_bar: 4,
            count_in_hits: 4,
            auto_stop: true,
            auto_stop_after_s: 4.0,
            bindings: Vec::new(),
        }
    }
}

impl Settings {
    /// Clamp everything into a usable range.
    pub fn sanitize(&mut self) {
        self.sensitivity = self.sensitivity.clamp(0.0, 100.0);
        self.offset_step_ms = self.offset_step_ms.clamp(0.1, 50.0);
        self.offset_trim_ms = self.offset_trim_ms.clamp(-500.0, 500.0);
        self.lookahead_ms = self.lookahead_ms.clamp(5.0, 500.0);
        self.half_life_s = self.half_life_s.clamp(0.5, 60.0);
        self.tempo_drift_pct = self.tempo_drift_pct.clamp(0.0, 50.0);
        self.tempo_min_bpm = self.tempo_min_bpm.clamp(20.0, 400.0);
        self.tempo_max_bpm = self.tempo_max_bpm.clamp(20.0, 400.0);
        if self.tempo_max_bpm < self.tempo_min_bpm * 1.2 {
            self.tempo_max_bpm = (self.tempo_min_bpm * 1.2).min(400.0);
            self.tempo_min_bpm = self.tempo_max_bpm / 1.2;
        }
        self.beats_per_bar = self.beats_per_bar.clamp(1, 32);
        self.count_in_hits = self.count_in_hits.clamp(2, 32);
        self.auto_stop_after_s = self.auto_stop_after_s.clamp(0.5, 120.0);
        for c in &mut self.channels {
            c.weight = c.weight.clamp(0.0, 4.0);
            c.highpass_hz = c.highpass_hz.clamp(0.0, 20000.0);
            c.lowpass_hz = c.lowpass_hz.clamp(0.0, 20000.0);
        }
        for o in [&mut self.click.output, &mut self.latency_test.output] {
            o.channel = o.channel.clamp(1, 16);
            o.note = o.note.min(127);
            o.velocity = o.velocity.clamp(1, 127);
        }
        self.click.accent_velocity = self.click.accent_velocity.clamp(1, 127);
    }
}

/// One labelled calibration example.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Example {
    pub hit: bool,
    /// Hit strength at the time of recording.
    pub strength: f32,
    pub features: Vec<f32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Persist {
    pub settings: Settings,
    pub examples: Vec<Example>,
    /// Latency measured by the most recent successful loopback test, in ms.
    pub measured_latency_ms: f32,
}
