//! Audio-to-MIDI-clock engine.
//!
//! Pure Rust with no I/O: the host feeds interleaved audio, the engine returns
//! timestamped MIDI events. Times are seconds on the input timeline (the
//! number of processed samples divided by the sample rate); hosts map them to
//! their own clocks. Everything else goes through a small JSON RPC so the
//! desktop app and the web version talk to the engine the same way.

pub mod calibration;
pub mod controls;
pub mod dsp;
pub mod settings;
pub mod tracker;

use std::collections::VecDeque;

use serde_json::{Value, json};

use calibration::Classifier;
use controls::{ACTIONS, MidiParser};
use dsp::{FEATURES_PER_CHANNEL, FrontEnd};
use settings::{Binding, ChannelSettings, Example, Persist, Settings};
use tracker::Tracker;

/// MIDI clock pulses per quarter note.
pub const PPQN: u64 = 24;
/// Hit strength (dB rise) of a typical hit before calibration.
const DEFAULT_REFERENCE_DB: f32 = 12.0;
/// Hits closer together than this are one hit.
const MIN_PEAK_GAP_S: f64 = 0.05;
/// Phase correction speed of the clock: share of the error fixed per tick.
const CLOCK_CORRECTION: f64 = 0.125;
/// Largest change of a tick interval relative to the nominal one.
const CLOCK_MAX_STRETCH: f64 = 0.15;
const LATENCY_TEST_NOTES: usize = 8;
const LATENCY_TEST_GAP_S: f64 = 0.45;
const LATENCY_TEST_WINDOW_S: f64 = 0.4;

/// Where an event goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Dest {
    /// All clock outputs.
    Clock = 0,
    /// The click output.
    Click = 1,
    /// The latency-test output.
    Latency = 2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutEvent {
    /// When to send it, in seconds on the input timeline.
    pub time: f64,
    pub dest: Dest,
    pub len: u8,
    pub bytes: [u8; 3],
}

impl OutEvent {
    fn new(time: f64, dest: Dest, bytes: &[u8]) -> Self {
        let mut b = [0u8; 3];
        b[..bytes.len()].copy_from_slice(bytes);
        Self { time, dest, len: bytes.len() as u8, bytes: b }
    }

    pub fn data(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Stopped,
    CountIn,
    Running,
    Calibrating,
    LatencyTest,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Stopped => "stopped",
            Mode::CountIn => "count_in",
            Mode::Running => "running",
            Mode::Calibrating => "calibrating",
            Mode::LatencyTest => "latency_test",
        }
    }
}

#[derive(Clone, Debug)]
struct Clock {
    /// Index of the next tick to send.
    tick: u64,
    /// Send time of the previous tick.
    last_send: f64,
    /// Input-timeline time of beat 1.
    beat_one: f64,
    /// Send time of the most recent beat (tick multiple of 24) and its index.
    last_beat: Option<(f64, u64)>,
}

#[derive(Clone, Debug)]
struct Peak {
    time: f64,
    strength: f32,
    accepted: bool,
    label: Option<bool>,
}

#[derive(Clone, Debug, Default)]
struct PeakPicker {
    s1: f32,
    s2: f32,
    y1: f32,
    p1: f32,
    feats1: Vec<f32>,
    last_peak: f64,
}

#[derive(Clone, Debug)]
struct LatencyTest {
    sends: Vec<f64>,
    detections: Vec<f64>,
    end: f64,
    picker: PeakPicker,
}

pub struct Engine {
    persist: Persist,
    version: u64,
    fe: FrontEnd,
    tracker: Option<Tracker>,
    classifier: Option<Classifier>,
    reference_db: f32,
    mode: Mode,
    count_in: VecDeque<f64>,
    clock: Option<Clock>,
    bar_offset: i64,
    taps: Vec<f64>,
    tap_period: Option<f64>,
    last_loud: f64,
    last_bpm: Option<f64>,
    recording: Option<bool>,
    picker: PeakPicker,
    peaks: VecDeque<Peak>,
    latency: Option<LatencyTest>,
    learning: Option<String>,
    midi: MidiParser,
    events: Vec<OutEvent>,
    message: String,
    strength_meter: f32,
    feats: Vec<f32>,
}

impl Engine {
    pub fn new(sample_rate: f32, channels: usize) -> Self {
        let mut persist = Persist::default();
        persist.settings.channels.resize(channels, ChannelSettings::default());
        let fe = FrontEnd::new(sample_rate, &persist.settings.channels);
        let mut e = Self {
            persist,
            version: 0,
            fe,
            tracker: None,
            classifier: None,
            reference_db: DEFAULT_REFERENCE_DB,
            mode: Mode::Stopped,
            count_in: VecDeque::new(),
            clock: None,
            bar_offset: 0,
            taps: Vec::new(),
            tap_period: None,
            last_loud: 0.0,
            last_bpm: None,
            recording: None,
            picker: PeakPicker::default(),
            peaks: VecDeque::new(),
            latency: None,
            learning: None,
            midi: MidiParser::default(),
            events: Vec::new(),
            message: String::new(),
            strength_meter: 0.0,
            feats: Vec::new(),
        };
        e.retrain();
        e
    }

    // ----- public host API -------------------------------------------------

    pub fn sample_rate(&self) -> f32 {
        self.fe.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.fe.channel_count()
    }

    /// Current time on the input timeline (end of the last complete frame).
    pub fn now(&self) -> f64 {
        self.fe.frame_index as f64 * self.fe.hop_seconds()
    }

    /// Time of the last sample fed in, in seconds. Use this, not `now`, to
    /// map engine times onto a host clock.
    pub fn sample_time(&self) -> f64 {
        self.fe.samples() as f64 / self.fe.sample_rate as f64
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn settings(&self) -> &Settings {
        &self.persist.settings
    }

    /// Increments whenever something that should be saved changes.
    pub fn persist_version(&self) -> u64 {
        self.version
    }

    pub fn export_persist(&self) -> String {
        serde_json::to_string(&self.persist).unwrap_or_default()
    }

    pub fn load_persist(&mut self, json: &str) -> Result<(), String> {
        let mut p: Persist = serde_json::from_str(json).map_err(|e| e.to_string())?;
        p.settings.sanitize();
        self.stop();
        self.mode = Mode::Stopped;
        self.persist = p;
        let n = self.channels();
        self.persist.settings.channels.resize(n, ChannelSettings::default());
        self.fe = FrontEnd::new(self.fe.sample_rate, &self.persist.settings.channels);
        self.retrain();
        self.version += 1;
        Ok(())
    }

    /// Tell the engine about a new audio format. Resets audio state.
    pub fn configure(&mut self, sample_rate: f32, channels: usize) {
        if sample_rate == self.fe.sample_rate && channels == self.channels() {
            return;
        }
        self.stop();
        if matches!(self.mode, Mode::LatencyTest) {
            self.mode = Mode::Stopped;
            self.latency = None;
        }
        let s = &mut self.persist.settings;
        if s.channels.len() != channels {
            s.channels.resize(channels, ChannelSettings::default());
            self.version += 1;
        }
        self.fe = FrontEnd::new(sample_rate, &self.persist.settings.channels);
        self.picker = PeakPicker::default();
        self.peaks.clear();
        self.retrain();
    }

    /// Feed interleaved audio. Generated events are collected in an internal
    /// queue; fetch them with `drain_events`.
    pub fn process(&mut self, interleaved: &[f32]) {
        let c = self.channels().max(1);
        for frame in interleaved.chunks_exact(c) {
            if self.fe.push(frame) {
                self.on_frame();
            }
        }
        if self.mode == Mode::Running {
            self.schedule_clock();
        }
        if self.mode == Mode::LatencyTest {
            self.check_latency_test();
        }
    }

    pub fn drain_events(&mut self) -> Vec<OutEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn drain_events_into(&mut self, out: &mut Vec<OutEvent>) {
        out.append(&mut self.events);
    }

    /// Raw MIDI from a control surface.
    pub fn midi_in(&mut self, bytes: &[u8]) {
        if let Some(src) = self.midi.parse(bytes) {
            self.control(&src);
        }
    }

    /// A control (MIDI, gamepad button, key) was pressed.
    pub fn control(&mut self, source: &str) {
        if let Some(action) = self.learning.take() {
            let b = &mut self.persist.settings.bindings;
            b.retain(|x| x.source != source);
            b.push(Binding { action: action.clone(), source: source.to_string() });
            self.version += 1;
            self.message = format!(
                "{} → {}",
                controls::describe(source),
                ACTIONS.iter().find(|a| a.0 == action).map(|a| a.1).unwrap_or(&action)
            );
            return;
        }
        let action = self
            .persist
            .settings
            .bindings
            .iter()
            .find(|b| b.source == source)
            .map(|b| b.action.clone());
        if let Some(a) = action {
            self.action(&a);
        }
    }

    pub fn action(&mut self, name: &str) {
        match name {
            "start_stop" => {
                if matches!(self.mode, Mode::Running | Mode::CountIn) {
                    self.stop();
                } else {
                    self.start();
                }
            }
            "start" => self.start(),
            "stop" => self.stop(),
            "mark_beat_one" => self.mark_beat_one(),
            "tap_tempo" => self.tap(),
            "offset_up" => self.nudge_offset(1.0),
            "offset_down" => self.nudge_offset(-1.0),
            "click" => {
                let c = &mut self.persist.settings.click;
                c.enabled = !c.enabled;
                self.version += 1;
            }
            "calibration" => {
                if self.mode == Mode::Calibrating {
                    self.leave_calibration();
                } else {
                    self.enter_calibration();
                }
            }
            "calibration_enter" => self.enter_calibration(),
            "calibration_leave" => self.leave_calibration(),
            "record_hits" => self.toggle_recording(true),
            "record_noise" => self.toggle_recording(false),
            "record_stop" => self.recording = None,
            "clear_calibration" => {
                self.persist.examples.clear();
                self.retrain();
                self.version += 1;
                self.message = "Calibration cleared".into();
            }
            "latency_test" => self.start_latency_test(),
            _ => self.message = format!("Unknown action {name}"),
        }
    }

    // ----- RPC -------------------------------------------------------------

    pub fn rpc(&mut self, request: &str) -> String {
        let v: Value = match serde_json::from_str(request) {
            Ok(v) => v,
            Err(e) => return json!({ "error": e.to_string() }).to_string(),
        };
        self.rpc_value(&v).to_string()
    }

    pub fn rpc_value(&mut self, v: &Value) -> Value {
        let cmd = v.get("cmd").and_then(Value::as_str).unwrap_or("");
        let str_arg = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        match cmd {
            "state" => self.state(),
            "persist" => serde_json::to_value(&self.persist).unwrap_or(Value::Null),
            "version" => json!(self.version),
            "load" => match v.get("persist").map(|p| self.load_persist(&p.to_string())) {
                Some(Ok(())) => json!({ "ok": true }),
                Some(Err(e)) => json!({ "error": e }),
                None => json!({ "error": "missing persist" }),
            },
            "set_settings" => match v.get("patch") {
                Some(p) => match self.patch_settings(p) {
                    Ok(()) => json!({ "ok": true }),
                    Err(e) => json!({ "error": e }),
                },
                None => json!({ "error": "missing patch" }),
            },
            "action" => {
                self.action(&str_arg("name"));
                json!({ "ok": true })
            }
            "control" => {
                self.control(&str_arg("source"));
                json!({ "ok": true })
            }
            "midi_in" => {
                let bytes: Vec<u8> = v
                    .get("bytes")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|x| x.as_u64().map(|b| b as u8)).collect())
                    .unwrap_or_default();
                self.midi_in(&bytes);
                json!({ "ok": true })
            }
            "learn" => {
                let a = str_arg("action");
                if controls::is_action(&a) {
                    self.learning = Some(a);
                    json!({ "ok": true })
                } else {
                    json!({ "error": "unknown action" })
                }
            }
            "learn_cancel" => {
                self.learning = None;
                json!({ "ok": true })
            }
            "unbind" => {
                let (a, s) = (str_arg("action"), str_arg("source"));
                self.persist.settings.bindings.retain(|b| !(b.action == a && b.source == s));
                self.version += 1;
                json!({ "ok": true })
            }
            "actions" => Value::Array(
                ACTIONS.iter().map(|(id, label)| json!({ "id": id, "label": label })).collect(),
            ),
            "describe" => json!(controls::describe(&str_arg("source"))),
            "configure" => {
                let sr = v.get("sample_rate").and_then(Value::as_f64).unwrap_or(48000.0) as f32;
                let ch = v.get("channels").and_then(Value::as_u64).unwrap_or(1) as usize;
                self.configure(sr, ch.max(1));
                json!({ "ok": true })
            }
            "clear_message" => {
                self.message.clear();
                json!({ "ok": true })
            }
            _ => json!({ "error": format!("unknown cmd {cmd}") }),
        }
    }

    fn patch_settings(&mut self, patch: &Value) -> Result<(), String> {
        let mut cur = serde_json::to_value(&self.persist.settings).map_err(|e| e.to_string())?;
        let (Some(obj), Some(p)) = (cur.as_object_mut(), patch.as_object()) else {
            return Err("patch must be an object".into());
        };
        for (k, val) in p {
            if k == "bindings" {
                continue; // changed only through learn / unbind
            }
            obj.insert(k.clone(), val.clone());
        }
        let mut s: Settings = serde_json::from_value(cur).map_err(|e| e.to_string())?;
        s.sanitize();
        s.channels.resize(self.channels(), ChannelSettings::default());
        let old = std::mem::replace(&mut self.persist.settings, s);
        self.apply_settings_change(&old);
        self.version += 1;
        Ok(())
    }

    fn apply_settings_change(&mut self, old: &Settings) {
        let new = self.persist.settings.clone();
        if old.channels.iter().map(|c| (c.highpass_hz, c.lowpass_hz)).ne(new.channels.iter().map(|c| (c.highpass_hz, c.lowpass_hz))) {
            self.fe.update_filters(&new.channels);
        }
        if let Some(tr) = self.tracker.as_mut() {
            if old.tempo_min_bpm != new.tempo_min_bpm || old.tempo_max_bpm != new.tempo_max_bpm {
                let est = tr.estimate();
                let mut t = self.new_tracker();
                let period = est.period.clamp(60.0 / new.tempo_max_bpm as f64, 60.0 / new.tempo_min_bpm as f64);
                t.init(est.phase, period, 0.02, 0.03);
                self.tracker = Some(t);
            } else if old.half_life_s != new.half_life_s || old.tempo_drift_pct != new.tempo_drift_pct {
                tr.set_dynamics(new.half_life_s as f64, new.tempo_drift_pct as f64);
            }
        }
    }

    // ----- frame processing ------------------------------------------------

    fn sensitivity_factor(&self) -> f32 {
        2f32.powf((50.0 - self.persist.settings.sensitivity) / 25.0)
    }

    fn thresholds(&self) -> (f32, f32) {
        let r = self.reference_db * self.sensitivity_factor();
        (r, 0.35 * r)
    }

    fn on_frame(&mut self) {
        let t = self.now();
        let (s_ref, s_min) = self.thresholds();
        let mut s = 0.0f32;
        for (f, cs) in self.fe.frames.iter().zip(&self.persist.settings.channels) {
            if !cs.enabled {
                continue;
            }
            let gate = ((f.above_floor - 3.0) / 6.0).clamp(0.0, 1.0);
            s = s.max(cs.weight * f.rise[0] * gate);
        }
        self.strength_meter = self.strength_meter.max(s);
        let rec_threshold = if self.mode == Mode::Calibrating { 0.5 * s_min } else { s_min };
        let mut p_hit = 1.0;
        let mut y = 0.0;
        if s >= rec_threshold {
            self.fe.features(&mut self.feats);
            if let Some(c) = &self.classifier {
                p_hit = c.prob(&self.feats);
            }
            if s >= s_min {
                y = (s / s_ref).min(2.0) * p_hit;
            }
        }

        // Peak at the previous frame?
        let pk = &self.picker;
        let is_peak = pk.s1 >= rec_threshold && pk.s1 >= pk.s2 && pk.s1 > s && t - pk.last_peak >= MIN_PEAK_GAP_S;
        if is_peak {
            let hop = self.fe.hop_seconds();
            let (ps, py, pp) = (pk.s1, pk.y1, pk.p1);
            let feats = std::mem::take(&mut self.picker.feats1);
            self.picker.last_peak = t - hop;
            self.on_peak(t - hop, ps, py, pp, &feats);
            self.picker.feats1 = feats;
        }
        self.picker.s2 = self.picker.s1;
        self.picker.s1 = s;
        self.picker.y1 = y;
        self.picker.p1 = p_hit;
        if s >= rec_threshold {
            self.picker.feats1.clear();
            self.picker.feats1.extend_from_slice(&self.feats);
        }

        match self.mode {
            Mode::Running => {
                if let Some(tr) = self.tracker.as_mut() {
                    tr.step(y);
                }
                if y >= 0.3 {
                    self.last_loud = t;
                }
                let st = &self.persist.settings;
                if st.auto_stop && t - self.last_loud > st.auto_stop_after_s as f64 {
                    self.stop();
                    self.message = "Stopped after silence".into();
                }
            }
            Mode::LatencyTest => self.latency_frame(t),
            _ => {}
        }
    }

    fn on_peak(&mut self, time: f64, strength: f32, y: f32, p_hit: f32, feats: &[f32]) {
        let (_, s_min) = self.thresholds();
        let accepted = strength >= s_min && p_hit >= 0.5 && y > 0.0;
        let mut label = None;
        if self.mode == Mode::Calibrating {
            if let Some(hit) = self.recording {
                label = Some(hit);
                self.persist.examples.push(Example { hit, strength, features: feats.to_vec() });
                calibration::trim(&mut self.persist.examples);
                self.retrain();
                self.version += 1;
            }
        }
        self.peaks.push_back(Peak { time, strength, accepted, label });
        while self.peaks.len() > 64 {
            self.peaks.pop_front();
        }
        if self.mode == Mode::CountIn && accepted {
            self.count_in_hit(time);
        }
    }

    // ----- clock -----------------------------------------------------------

    fn offset_s(&self) -> f64 {
        (self.persist.measured_latency_ms + self.persist.settings.offset_trim_ms) as f64 / 1000.0
    }

    fn period_range(&self) -> (f64, f64) {
        let s = &self.persist.settings;
        (60.0 / s.tempo_max_bpm as f64, 60.0 / s.tempo_min_bpm as f64)
    }

    fn new_tracker(&self) -> Tracker {
        let s = &self.persist.settings;
        Tracker::new(
            self.fe.hop_seconds(),
            s.tempo_min_bpm as f64,
            s.tempo_max_bpm as f64,
            s.half_life_s as f64,
            s.tempo_drift_pct as f64,
        )
    }

    /// Fold a period into the tempo range, preferring the octave closest to a hint.
    fn fold_period(&self, period: f64, hint: Option<f64>) -> f64 {
        let (lo, hi) = self.period_range();
        let mut candidates: Vec<f64> = Vec::new();
        for k in -3..=3 {
            let p = period * 2f64.powi(k);
            if p >= lo * 0.999 && p <= hi * 1.001 {
                candidates.push(p);
            }
        }
        if candidates.is_empty() {
            return period.clamp(lo, hi);
        }
        let target = hint.unwrap_or(period);
        candidates.into_iter().min_by(|a, b| (a / target).ln().abs().total_cmp(&(b / target).ln().abs())).unwrap()
    }

    fn start(&mut self) {
        match self.mode {
            Mode::Running | Mode::CountIn => return,
            Mode::LatencyTest => {
                self.message = "Wait for the latency test to finish".into();
                return;
            }
            Mode::Calibrating => self.leave_calibration(),
            Mode::Stopped => {}
        }
        self.count_in.clear();
        self.mode = Mode::CountIn;
        self.message.clear();
    }

    fn count_in_hit(&mut self, time: f64) {
        if let Some(&last) = self.count_in.back() {
            if time - last > 2.5 {
                self.count_in.clear();
            }
        }
        self.count_in.push_back(time);
        let need = self.persist.settings.count_in_hits as usize;
        while self.count_in.len() > need {
            self.count_in.pop_front();
        }
        if self.count_in.len() < need {
            return;
        }
        let v: Vec<f64> = self.count_in.iter().copied().collect();
        let mut iv: Vec<f64> = v.windows(2).map(|w| w[1] - w[0]).collect();
        iv.sort_by(f64::total_cmp);
        let med = iv[iv.len() / 2];
        if iv.iter().any(|x| (x - med).abs() > 0.2 * med) {
            return;
        }
        // Least-squares fit of the count-in grid for period and last beat time.
        let n = v.len() as f64;
        let mean_i = (n - 1.0) / 2.0;
        let mean_t = v.iter().sum::<f64>() / n;
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for (i, t) in v.iter().enumerate() {
            sxy += (i as f64 - mean_i) * (t - mean_t);
            sxx += (i as f64 - mean_i).powi(2);
        }
        let fitted = sxy / sxx;
        let last = mean_t + (n - 1.0 - mean_i) * fitted;
        let period = self.fold_period(fitted, self.tap_period);
        self.start_running(last, period);
    }

    fn start_running(&mut self, last_beat: f64, period: f64) {
        let now = self.now();
        let mut tr = self.new_tracker();
        tr.init(((now - last_beat) / period).rem_euclid(1.0), period, 0.015, 0.03);
        self.tracker = Some(tr);
        let offset = self.offset_s();
        let mut beat_one = last_beat + period;
        while beat_one - offset < now + 0.003 {
            beat_one += period;
        }
        self.clock = Some(Clock { tick: 0, last_send: 0.0, beat_one, last_beat: None });
        self.bar_offset = 0;
        self.last_loud = now;
        self.mode = Mode::Running;
        self.last_bpm = Some(60.0 / period);
        self.count_in.clear();
    }

    fn schedule_clock(&mut self) {
        let now = self.now();
        let offset = self.offset_s();
        let horizon = now + self.persist.settings.lookahead_ms as f64 / 1000.0;
        let Some(tr) = self.tracker.as_mut() else { return };
        let est = tr.estimate();
        self.last_bpm = Some(est.bpm());
        let period = est.period;
        let nominal = period / PPQN as f64;
        let click = self.persist.settings.click.clone();
        let per_bar = self.persist.settings.beats_per_bar as i64;
        let Some(clock) = self.clock.as_mut() else { return };
        loop {
            let send = if clock.tick == 0 {
                clock.beat_one - offset
            } else {
                let b = clock.tick as f64 / PPQN as f64;
                let frac = b - b.floor();
                let expected = clock.last_send + offset + nominal;
                let m = ((expected - now) / period - frac + est.phase).round();
                let ideal = now + (m + frac - est.phase) * period - offset;
                let err = ideal - (clock.last_send + nominal);
                let lim = CLOCK_MAX_STRETCH * nominal;
                clock.last_send + nominal + (err * CLOCK_CORRECTION).clamp(-lim, lim)
            };
            if send > horizon {
                break;
            }
            if clock.tick == 0 {
                self.events.push(OutEvent::new(send - 0.0005, Dest::Clock, &[0xFA]));
            }
            self.events.push(OutEvent::new(send, Dest::Clock, &[0xF8]));
            if clock.tick % PPQN == 0 {
                let beat = clock.tick / PPQN;
                clock.last_beat = Some((send, beat));
                if click.enabled && !click.output.port.is_empty() {
                    let in_bar = (beat as i64 - self.bar_offset).rem_euclid(per_bar);
                    let vel = if in_bar == 0 { click.accent_velocity } else { click.output.velocity };
                    let o = &click.output;
                    self.events.push(OutEvent::new(send, Dest::Click, &[o.status_on(), o.note, vel]));
                    self.events.push(OutEvent::new(send + 0.03, Dest::Click, &[o.status_off(), o.note, 0]));
                }
            }
            clock.last_send = send;
            clock.tick += 1;
        }
    }

    /// Clock position in beats at the current time (send timeline).
    fn clock_position(&self) -> Option<f64> {
        let c = self.clock.as_ref()?;
        if c.tick == 0 {
            return None;
        }
        let period = 60.0 / self.last_bpm.unwrap_or(120.0);
        let pos = (c.tick - 1) as f64 / PPQN as f64 - (c.last_send - self.now()) / period;
        Some(pos.max(0.0))
    }

    fn stop(&mut self) {
        if self.mode == Mode::Running {
            let mut t = self.now();
            if let Some(c) = &self.clock {
                if c.tick > 0 {
                    t = t.max(c.last_send + 0.0005);
                }
            }
            self.events.push(OutEvent::new(t, Dest::Clock, &[0xFC]));
        }
        if matches!(self.mode, Mode::Running | Mode::CountIn) {
            self.mode = Mode::Stopped;
        }
        self.clock = None;
        self.tracker = None;
        self.count_in.clear();
    }

    fn mark_beat_one(&mut self) {
        match self.clock_position() {
            Some(p) => {
                self.bar_offset = p.round() as i64;
                self.message = "Beat 1 marked".into();
            }
            None => self.message = "Start the clock first".into(),
        }
    }

    fn tap(&mut self) {
        let now = self.now();
        if let Some(&last) = self.taps.last() {
            if now - last > 2.0 {
                self.taps.clear();
            }
        }
        self.taps.push(now);
        if self.taps.len() > 6 {
            self.taps.remove(0);
        }
        if self.taps.len() >= 2 {
            let mut iv: Vec<f64> = self.taps.windows(2).map(|w| w[1] - w[0]).collect();
            iv.sort_by(f64::total_cmp);
            let p = self.fold_period(iv[iv.len() / 2], None);
            self.tap_period = Some(p);
            if let Some(tr) = self.tracker.as_mut() {
                tr.hint_tempo(p, 0.04);
            }
        }
    }

    fn nudge_offset(&mut self, sign: f32) {
        let s = &mut self.persist.settings;
        s.offset_trim_ms = (s.offset_trim_ms + sign * s.offset_step_ms).clamp(-500.0, 500.0);
        self.version += 1;
    }

    // ----- calibration -----------------------------------------------------

    fn feature_dims(&self) -> usize {
        self.channels() * FEATURES_PER_CHANNEL
    }

    fn retrain(&mut self) {
        let dims = self.feature_dims();
        self.classifier = Classifier::train(&self.persist.examples, dims);
        let mut hits: Vec<f32> =
            self.persist.examples.iter().filter(|e| e.hit && e.features.len() == dims).map(|e| e.strength).collect();
        self.reference_db = if hits.len() >= calibration::MIN_PER_CLASS {
            hits.sort_by(f32::total_cmp);
            hits[hits.len() / 2].max(2.0)
        } else {
            DEFAULT_REFERENCE_DB
        };
    }

    fn enter_calibration(&mut self) {
        if self.mode == Mode::LatencyTest {
            self.message = "Wait for the latency test to finish".into();
            return;
        }
        self.stop();
        self.mode = Mode::Calibrating;
    }

    fn leave_calibration(&mut self) {
        if self.mode == Mode::Calibrating {
            self.mode = Mode::Stopped;
        }
        self.recording = None;
    }

    fn toggle_recording(&mut self, hits: bool) {
        if self.mode != Mode::Calibrating {
            self.enter_calibration();
            if self.mode != Mode::Calibrating {
                return;
            }
        }
        self.recording = if self.recording == Some(hits) { None } else { Some(hits) };
    }

    // ----- latency test ----------------------------------------------------

    fn start_latency_test(&mut self) {
        let lt = self.persist.settings.latency_test.clone();
        let Some(ch) = lt.listen_channel.filter(|&c| c < self.channels()) else {
            self.message = "Choose the listening channel for the latency test".into();
            return;
        };
        let _ = ch;
        if lt.output.port.is_empty() {
            self.message = "Choose the MIDI output for the latency test".into();
            return;
        }
        match self.mode {
            Mode::Running | Mode::CountIn => {
                self.message = "Stop the clock before the latency test".into();
                return;
            }
            Mode::LatencyTest => return,
            Mode::Calibrating => self.leave_calibration(),
            Mode::Stopped => {}
        }
        let o = lt.output;
        let first = self.now() + self.persist.settings.lookahead_ms as f64 / 1000.0 + 0.2;
        let mut sends = Vec::new();
        for i in 0..LATENCY_TEST_NOTES {
            let t = first + i as f64 * LATENCY_TEST_GAP_S;
            self.events.push(OutEvent::new(t, Dest::Latency, &[o.status_on(), o.note, o.velocity]));
            self.events.push(OutEvent::new(t + 0.05, Dest::Latency, &[o.status_off(), o.note, 0]));
            sends.push(t);
        }
        let end = first + LATENCY_TEST_NOTES as f64 * LATENCY_TEST_GAP_S + 0.3;
        self.latency = Some(LatencyTest { sends, detections: Vec::new(), end, picker: PeakPicker::default() });
        self.mode = Mode::LatencyTest;
        self.message = "Latency test running…".into();
    }

    fn latency_frame(&mut self, t: f64) {
        let Some(ch) = self.persist.settings.latency_test.listen_channel else { return };
        let Some(f) = self.fe.frames.get(ch) else { return };
        let gate = ((f.above_floor - 3.0) / 6.0).clamp(0.0, 1.0);
        let s = f.rise[0] * gate;
        let hop = self.fe.hop_seconds();
        let Some(lt) = self.latency.as_mut() else { return };
        let p = &mut lt.picker;
        if p.s1 >= 6.0 && p.s1 >= p.s2 && p.s1 > s && t - p.last_peak >= 0.1 {
            p.last_peak = t - hop;
            lt.detections.push(t - hop);
        }
        p.s2 = p.s1;
        p.s1 = s;
    }

    fn check_latency_test(&mut self) {
        let now = self.now();
        let Some(lt) = &self.latency else {
            self.mode = Mode::Stopped;
            return;
        };
        if now < lt.end {
            return;
        }
        let mut lat: Vec<f64> = lt
            .sends
            .iter()
            .filter_map(|&s| lt.detections.iter().find(|&&d| d >= s && d <= s + LATENCY_TEST_WINDOW_S).map(|d| d - s))
            .collect();
        lat.sort_by(f64::total_cmp);
        self.latency = None;
        self.mode = Mode::Stopped;
        if lat.len() < LATENCY_TEST_NOTES / 2 + 1 {
            self.message = format!(
                "Latency test failed: heard {} of {} notes. Check the output, note and listening channel.",
                lat.len(),
                LATENCY_TEST_NOTES
            );
            return;
        }
        let med = lat[lat.len() / 2];
        // Spread of the middle half of the measurements.
        let q1 = lat[lat.len() / 4];
        let q3 = lat[(lat.len() * 3) / 4];
        if q3 - q1 > 0.012 {
            self.message = format!(
                "Latency test unstable ({:.0}–{:.0} ms). Try again with less background sound.",
                q1 * 1000.0,
                q3 * 1000.0
            );
            return;
        }
        let old = self.persist.measured_latency_ms;
        self.persist.measured_latency_ms = (med * 1000.0) as f32;
        self.version += 1;
        self.message = format!("Latency {:.1} ms (was {:.1} ms)", med * 1000.0, old);
    }

    // ----- status ----------------------------------------------------------

    pub fn state(&mut self) -> Value {
        let now = self.now();
        let (s_ref, s_min) = self.thresholds();
        let s = &self.persist.settings;
        let confidence = self.tracker.as_mut().map(|t| t.confidence());
        let phase = self.tracker.as_mut().map(|t| t.estimate().phase);
        let per_bar = s.beats_per_bar as i64;
        let beat = self.clock_position().map(|p| {
            let b = p.floor() as i64 - self.bar_offset;
            json!({
                "position": p,
                "in_bar": b.rem_euclid(per_bar),
                "bar": b.div_euclid(per_bar),
                "fraction": p - p.floor(),
            })
        });
        let dims = self.feature_dims();
        let hits = self.persist.examples.iter().filter(|e| e.hit && e.features.len() == dims).count();
        let noise = self.persist.examples.iter().filter(|e| !e.hit && e.features.len() == dims).count();
        let mismatched = self.persist.examples.iter().filter(|e| e.features.len() != dims).count();
        let peaks: Vec<Value> = self
            .peaks
            .iter()
            .filter(|p| now - p.time < 3.0)
            .map(|p| json!({ "age": now - p.time, "accepted": p.accepted, "label": p.label, "strength": p.strength }))
            .collect();
        let meters = self.fe.take_peaks();
        let strength = std::mem::take(&mut self.strength_meter);
        json!({
            "mode": self.mode.name(),
            "time": now,
            "sample_rate": self.fe.sample_rate,
            "channels": self.channels(),
            "count_in": { "have": self.count_in.len(), "need": s.count_in_hits },
            "bpm": self.last_bpm,
            "tap_bpm": self.tap_period.map(|p| 60.0 / p),
            "phase": phase,
            "confidence": confidence,
            "beat": beat,
            "beats_per_bar": s.beats_per_bar,
            "offset_ms": self.offset_s() * 1000.0,
            "measured_latency_ms": self.persist.measured_latency_ms,
            "trim_ms": s.offset_trim_ms,
            "meters": meters,
            "strength": strength,
            "threshold": s_min,
            "reference": s_ref,
            "peaks": peaks,
            "calibration": {
                "hits": hits,
                "noise": noise,
                "mismatched": mismatched,
                "trained": self.classifier.is_some(),
                "recording": self.recording.map(|h| if h { "hits" } else { "noise" }),
            },
            "learning": self.learning,
            "message": self.message,
            "click": s.click.enabled,
            "version": self.version,
        })
    }
}

#[cfg(test)]
mod sim;
