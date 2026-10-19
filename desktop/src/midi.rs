//! MIDI: one thread owns all connections, keeps a queue of timestamped
//! events and sends each at its time (sleep, then spin for the last moment).

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use engine::{Dest, OutEvent};
use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};

use crate::host::{MidiStatus, Shared};

const CLIENT: &str = "Audio to MIDI Clock";
const SPIN: Duration = Duration::from_micros(1200);
const SCAN_EVERY: Duration = Duration::from_secs(2);

pub enum MidiCmd {
    Event { at: Instant, event: OutEvent },
    Ports { clock: Vec<String>, click: String, latency: String },
}

struct Pending {
    at: Instant,
    seq: u64,
    event: OutEvent,
}

impl PartialEq for Pending {
    fn eq(&self, o: &Self) -> bool {
        self.at == o.at && self.seq == o.seq
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, o: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(o))
    }
}
impl Ord for Pending {
    // Reversed so the heap pops the earliest event first.
    fn cmp(&self, o: &Self) -> CmpOrdering {
        o.at.cmp(&self.at).then(o.seq.cmp(&self.seq))
    }
}

struct Manager {
    shared: Arc<Shared>,
    queue: BinaryHeap<Pending>,
    seq: u64,
    outputs: HashMap<String, MidiOutputConnection>,
    inputs: HashMap<String, MidiInputConnection<()>>,
    clock: Vec<String>,
    click: String,
    latency: String,
    last_scan: Option<Instant>,
}

pub fn run(shared: Arc<Shared>, rx: Receiver<MidiCmd>) {
    let mut m = Manager {
        shared,
        queue: BinaryHeap::new(),
        seq: 0,
        outputs: HashMap::new(),
        inputs: HashMap::new(),
        clock: Vec::new(),
        click: String::new(),
        latency: String::new(),
        last_scan: None,
    };
    loop {
        // Wait for work: block when idle, otherwise sleep until just before the next event.
        match m.queue.peek().map(|p| p.at.saturating_duration_since(Instant::now())) {
            None => match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(cmd) => m.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            },
            Some(d) if d > SPIN => std::thread::sleep((d - SPIN).min(Duration::from_millis(2))),
            Some(_) => {}
        }
        loop {
            match rx.try_recv() {
                Ok(cmd) => m.handle(cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        m.send_due();
        if m.last_scan.is_none_or(|t| t.elapsed() > SCAN_EVERY) {
            m.scan();
        }
    }
}

impl Manager {
    fn handle(&mut self, cmd: MidiCmd) {
        match cmd {
            MidiCmd::Event { at, event } => {
                self.seq += 1;
                self.queue.push(Pending { at, seq: self.seq, event });
            }
            MidiCmd::Ports { clock, click, latency } => {
                if clock != self.clock || click != self.click || latency != self.latency {
                    self.clock = clock;
                    self.click = click;
                    self.latency = latency;
                    self.last_scan = None;
                }
            }
        }
    }

    fn send_due(&mut self) {
        while let Some(p) = self.queue.peek() {
            let now = Instant::now();
            if p.at > now + SPIN {
                break;
            }
            let p = self.queue.pop().unwrap();
            while Instant::now() < p.at {
                std::hint::spin_loop();
            }
            let ports: Vec<String> = match p.event.dest {
                Dest::Clock => self.clock.clone(),
                Dest::Click => vec![self.click.clone()],
                Dest::Latency => vec![self.latency.clone()],
            };
            for name in ports {
                if let Some(conn) = self.outputs.get_mut(&name) {
                    if conn.send(p.event.data()).is_err() {
                        self.outputs.remove(&name);
                        self.last_scan = None;
                    }
                }
            }
        }
    }

    fn wanted_outputs(&self) -> Vec<String> {
        let mut v = self.clock.clone();
        v.push(self.click.clone());
        v.push(self.latency.clone());
        v.retain(|s| !s.is_empty());
        v.sort();
        v.dedup();
        v
    }

    fn scan(&mut self) {
        self.last_scan = Some(Instant::now());
        let wanted = self.wanted_outputs();
        let mut out_names = Vec::new();
        if let Ok(out) = MidiOutput::new(CLIENT) {
            for port in out.ports() {
                if let Ok(name) = out.port_name(&port) {
                    out_names.push(name);
                }
            }
        }
        self.outputs.retain(|name, _| wanted.contains(name) && out_names.contains(name));
        for name in &wanted {
            if self.outputs.contains_key(name) || !out_names.contains(name) {
                continue;
            }
            let Ok(out) = MidiOutput::new(CLIENT) else { continue };
            let port = out.ports().into_iter().find(|p| out.port_name(p).as_deref() == Ok(name.as_str()));
            if let Some(port) = port {
                if let Ok(conn) = out.connect(&port, "clock") {
                    self.outputs.insert(name.clone(), conn);
                }
            }
        }

        let mut in_names = Vec::new();
        if let Ok(inp) = MidiInput::new(CLIENT) {
            for port in inp.ports() {
                if let Ok(name) = inp.port_name(&port) {
                    in_names.push(name);
                }
            }
        }
        // Listen to every input for controls, except ports we also send to,
        // which would echo our own notes back (e.g. a MIDI through port).
        self.inputs.retain(|name, _| in_names.contains(name) && !wanted.contains(name));
        for name in &in_names {
            if self.inputs.contains_key(name) || wanted.contains(name) {
                continue;
            }
            let Ok(mut inp) = MidiInput::new(CLIENT) else { continue };
            inp.ignore(Ignore::All);
            let port = inp.ports().into_iter().find(|p| inp.port_name(p).as_deref() == Ok(name.as_str()));
            let Some(port) = port else { continue };
            let sh = self.shared.clone();
            let conn = inp.connect(
                &port,
                "controls",
                move |_, msg, _| {
                    if msg.first().is_some_and(|&b| b < 0xF0) {
                        sh.with_engine(|e| e.midi_in(msg));
                    }
                },
                (),
            );
            if let Ok(c) = conn {
                self.inputs.insert(name.clone(), c);
            }
        }

        let missing = wanted.iter().filter(|n| !self.outputs.contains_key(*n)).cloned().collect();
        *self.shared.midi_status.lock().unwrap() = MidiStatus { outputs: out_names, inputs: in_names, missing };
    }
}
