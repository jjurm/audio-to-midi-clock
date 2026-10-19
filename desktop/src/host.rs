//! State shared by the desktop host's threads.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine::{Engine, OutEvent};
use serde::Serialize;

use crate::audio::AudioCmd;
use crate::midi::MidiCmd;

#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioStatus {
    pub device: String,
    pub running: bool,
    pub channels: usize,
    pub sample_rate: f32,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MidiStatus {
    pub outputs: Vec<String>,
    pub inputs: Vec<String>,
    pub missing: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HostStatus {
    pub audio: AudioStatus,
    pub midi_outputs: Vec<String>,
    pub midi_inputs: Vec<String>,
    pub missing_ports: Vec<String>,
    pub notes: Vec<String>,
}

/// Maps engine time (seconds of audio fed in) to the system clock.
///
/// Each audio callback gives one pair (now, engine time). Delivery is only
/// ever late, never early, so the smallest difference over a sliding window
/// is the best estimate of the true relation.
pub struct TimeMap {
    base: Instant,
    window: Vec<f64>,
    pos: usize,
    last_engine: f64,
    estimate: Option<f64>,
}

const WINDOW: usize = 256;

impl TimeMap {
    pub fn new() -> Self {
        Self { base: Instant::now(), window: Vec::with_capacity(WINDOW), pos: 0, last_engine: 0.0, estimate: None }
    }

    pub fn observe(&mut self, now: Instant, engine_time: f64) {
        if engine_time < self.last_engine {
            // The engine was reset; start over.
            self.window.clear();
            self.pos = 0;
        }
        self.last_engine = engine_time;
        let d = now.duration_since(self.base).as_secs_f64() - engine_time;
        if self.window.len() < WINDOW {
            self.window.push(d);
        } else {
            self.window[self.pos] = d;
            self.pos = (self.pos + 1) % WINDOW;
        }
        self.estimate = self.window.iter().copied().reduce(f64::min);
    }

    pub fn to_instant(&self, engine_time: f64) -> Instant {
        match self.estimate {
            Some(d) => {
                let secs = engine_time + d;
                if secs <= 0.0 { self.base } else { self.base + Duration::from_secs_f64(secs) }
            }
            None => Instant::now(),
        }
    }
}

pub struct Shared {
    pub engine: Mutex<Engine>,
    pub timemap: Mutex<TimeMap>,
    pub audio_status: Mutex<AudioStatus>,
    pub midi_status: Mutex<MidiStatus>,
    pub audio_tx: Mutex<Option<Sender<AudioCmd>>>,
    pub midi_tx: Mutex<Option<Sender<MidiCmd>>>,
    pub settings_path: Mutex<Option<PathBuf>>,
    pub started: Mutex<bool>,
    pub device_cache: Mutex<Option<(Instant, Vec<crate::audio::DeviceInfo>)>>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            engine: Mutex::new(Engine::new(48000.0, 1)),
            timemap: Mutex::new(TimeMap::new()),
            audio_status: Mutex::new(AudioStatus::default()),
            midi_status: Mutex::new(MidiStatus::default()),
            audio_tx: Mutex::new(None),
            midi_tx: Mutex::new(None),
            settings_path: Mutex::new(None),
            started: Mutex::new(false),
            device_cache: Mutex::new(None),
        })
    }

    /// Hand events to the MIDI thread with their send times.
    pub fn dispatch(&self, events: Vec<OutEvent>) {
        if events.is_empty() {
            return;
        }
        let tm = self.timemap.lock().unwrap();
        let tx = self.midi_tx.lock().unwrap();
        if let Some(tx) = tx.as_ref() {
            for e in events {
                let _ = tx.send(MidiCmd::Event { at: tm.to_instant(e.time), event: e });
            }
        }
    }

    /// Run an engine call, then dispatch any events it produced.
    pub fn with_engine<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let (r, events) = {
            let mut e = self.engine.lock().unwrap();
            let r = f(&mut e);
            (r, e.drain_events())
        };
        self.dispatch(events);
        r
    }

    /// Push settings that the I/O threads care about.
    pub fn sync_io(&self) {
        let s = self.engine.lock().unwrap().settings().clone();
        if let Some(tx) = self.audio_tx.lock().unwrap().as_ref() {
            let _ = tx.send(AudioCmd::Open { device: s.audio_device.clone(), buffer_frames: s.buffer_frames });
        }
        if let Some(tx) = self.midi_tx.lock().unwrap().as_ref() {
            let _ = tx.send(MidiCmd::Ports {
                clock: s.clock_outputs.clone(),
                click: s.click.output.port.clone(),
                latency: s.latency_test.output.port.clone(),
            });
        }
    }

    pub fn status(&self) -> HostStatus {
        let audio = self.audio_status.lock().unwrap().clone();
        let midi = self.midi_status.lock().unwrap().clone();
        HostStatus {
            audio,
            midi_outputs: midi.outputs,
            midi_inputs: midi.inputs,
            missing_ports: midi.missing,
            notes: Vec::new(),
        }
    }
}
