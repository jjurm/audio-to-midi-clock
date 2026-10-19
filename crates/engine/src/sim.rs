//! End-to-end tests with generated percussion audio.

use super::*;

const SR: f32 = 48000.0;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
}

#[derive(Clone, Copy)]
enum Sound {
    Bass,
    Slap,
    Ghost,
    Noise,
}

fn add_sound(buf: &mut [f32], t: f64, sound: Sound, amp: f32, rng: &mut Rng) {
    let start = (t * SR as f64) as usize;
    let len = (0.25 * SR) as usize;
    for i in 0..len {
        let idx = start + i;
        if idx >= buf.len() {
            break;
        }
        let tt = i as f32 / SR;
        let n = rng.next();
        let v = match sound {
            Sound::Bass => {
                (2.0 * std::f32::consts::PI * 85.0 * tt).sin() * (-tt * 18.0).exp() + 0.3 * n * (-tt * 150.0).exp()
            }
            Sound::Slap => 0.8 * n * (-tt * 60.0).exp() + 0.2 * (2.0 * std::f32::consts::PI * 400.0 * tt).sin() * (-tt * 40.0).exp(),
            Sound::Ghost => 0.25 * n * (-tt * 80.0).exp(),
            // A dull knock (e.g. stand or foot), low and without brightness.
            Sound::Noise => (2.0 * std::f32::consts::PI * 140.0 * tt).sin() * (tt / 0.004).min(1.0) * (-tt * 30.0).exp(),
        };
        buf[idx] += amp * v;
    }
}

/// Beat times for a tempo curve.
fn beats(start: f64, end: f64, bpm: impl Fn(f64) -> f64) -> Vec<f64> {
    let mut v = Vec::new();
    let mut t = start;
    while t < end {
        v.push(t);
        t += 60.0 / bpm(t);
    }
    v
}

struct Result {
    /// Send times of each clock beat (tick multiple of 24).
    clock_beats: Vec<f64>,
    stop_at: Option<f64>,
    starts: usize,
}

fn run(audio: &[f32], setup: impl FnOnce(&mut Engine)) -> Result {
    let mut e = Engine::new(SR, 1);
    setup(&mut e);
    e.action("start");
    let mut ticks = 0u64;
    let mut res = Result { clock_beats: Vec::new(), stop_at: None, starts: 0 };
    for block in audio.chunks(256) {
        e.process(block);
        for ev in e.drain_events() {
            match ev.data() {
                [0xFA] => {
                    res.starts += 1;
                    ticks = 0;
                }
                [0xF8] => {
                    if ticks % 24 == 0 {
                        res.clock_beats.push(ev.time);
                    }
                    ticks += 1;
                }
                [0xFC] => res.stop_at = Some(ev.time),
                _ => {}
            }
        }
    }
    res
}

/// Mean and max absolute error of clock beats against the nearest true beat,
/// skipping the first `skip` clock beats.
fn errors(clock: &[f64], truth: &[f64], skip: usize, until: f64) -> (f64, f64, f64) {
    let mut errs = Vec::new();
    for &c in clock.iter().skip(skip) {
        if c > until {
            break;
        }
        let nearest = truth.iter().copied().min_by(|a, b| (a - c).abs().total_cmp(&(b - c).abs())).unwrap();
        errs.push(c - nearest);
    }
    let mean = errs.iter().sum::<f64>() / errs.len() as f64;
    let mae = errs.iter().map(|e| e.abs()).sum::<f64>() / errs.len() as f64;
    let max = errs.iter().map(|e| e.abs()).fold(0.0, f64::max);
    (mean, mae, max)
}

fn groove(audio: &mut [f32], beats: &[f64], count_in: usize, rng: &mut Rng, eighths: bool, ghosts: bool) {
    for (i, w) in beats.windows(2).enumerate() {
        let (b, next) = (w[0], w[1]);
        let jitter = |r: &mut Rng| r.next() as f64 * 0.004;
        if i < count_in {
            add_sound(audio, b + jitter(rng), Sound::Bass, 0.6, rng);
            continue;
        }
        let in_bar = (i - count_in) % 4;
        let accent = if in_bar % 2 == 0 { 0.7 } else { 0.5 };
        add_sound(audio, b + jitter(rng), if in_bar % 2 == 0 { Sound::Bass } else { Sound::Slap }, accent, rng);
        if eighths {
            add_sound(audio, (b + next) / 2.0 + jitter(rng), Sound::Slap, 0.3, rng);
        }
        if ghosts {
            add_sound(audio, b + (next - b) * 0.75 + jitter(rng), Sound::Ghost, 0.5, rng);
        }
    }
}

fn background(audio: &mut [f32], rng: &mut Rng) {
    for s in audio.iter_mut() {
        *s += 0.0005 * rng.next();
    }
}

#[test]
fn steady_groove() {
    let dur = 30.0;
    let mut audio = vec![0.0f32; (dur * SR as f64) as usize];
    let mut rng = Rng(1);
    background(&mut audio, &mut rng);
    let truth = beats(1.0, dur - 0.5, |_| 100.0);
    groove(&mut audio, &truth, 4, &mut rng, true, true);
    let r = run(&audio, |_| {});
    assert_eq!(r.starts, 1);
    let (mean, mae, max) = errors(&r.clock_beats, &truth, 4, dur - 1.0);
    println!("steady: beats {} mean {:.1} ms mae {:.1} ms max {:.1} ms", r.clock_beats.len(), mean * 1e3, mae * 1e3, max * 1e3);
    assert!((r.clock_beats[0] - truth[4]).abs() < 0.03, "beat one at {} vs {}", r.clock_beats[0], truth[4]);
    assert!(mae < 0.006 && max < 0.02);
}

#[test]
fn tempo_change() {
    let dur = 40.0;
    let mut audio = vec![0.0f32; (dur * SR as f64) as usize];
    let mut rng = Rng(2);
    background(&mut audio, &mut rng);
    // 90 BPM, ramp to 120 between 10 s and 30 s.
    let truth = beats(1.0, dur - 0.5, |t| 90.0 + 30.0 * ((t - 10.0) / 20.0).clamp(0.0, 1.0));
    groove(&mut audio, &truth, 4, &mut rng, true, false);
    let r = run(&audio, |_| {});
    let (mean, mae, max) = errors(&r.clock_beats, &truth, 4, dur - 1.0);
    println!("ramp: mean {:.1} ms mae {:.1} ms max {:.1} ms", mean * 1e3, mae * 1e3, max * 1e3);
    assert!(mae < 0.01 && max < 0.035);
}

#[test]
fn pause_and_resume_in_time() {
    let dur = 30.0;
    let mut audio = vec![0.0f32; (dur * SR as f64) as usize];
    let mut rng = Rng(3);
    background(&mut audio, &mut rng);
    let all = beats(1.0, dur - 0.5, |_| 110.0);
    // Silence between 12 and 14.5 s.
    let played: Vec<f64> = all.iter().copied().filter(|&t| !(12.0..14.5).contains(&t)).collect();
    groove(&mut audio, &played, 4, &mut rng, false, false);
    let r = run(&audio, |_| {});
    assert!(r.stop_at.is_none(), "stopped at {:?}", r.stop_at);
    let (mean, mae, max) = errors(&r.clock_beats, &all, 4, dur - 1.0);
    println!("pause: mean {:.1} ms mae {:.1} ms max {:.1} ms", mean * 1e3, mae * 1e3, max * 1e3);
    assert!(mae < 0.008 && max < 0.03);
}

#[test]
fn stops_after_silence() {
    let dur = 20.0;
    let mut audio = vec![0.0f32; (dur * SR as f64) as usize];
    let mut rng = Rng(4);
    background(&mut audio, &mut rng);
    let truth = beats(1.0, 8.0, |_| 120.0);
    groove(&mut audio, &truth, 4, &mut rng, false, false);
    let r = run(&audio, |_| {});
    let stop = r.stop_at.expect("should stop");
    assert!(stop > 11.0 && stop < 13.5, "stop at {stop}");
}

#[test]
fn rejects_noise_after_calibration() {
    // Calibrate with hits and rumble, then check the rumble is rejected.
    let mut e = Engine::new(SR, 1);
    let mut rng = Rng(5);
    e.action("calibration_enter");
    let feed = |e: &mut Engine, sound: Sound, amp: f32, n: usize, rng: &mut Rng| {
        let mut audio = vec![0.0f32; (n as f64 * 0.5 * SR as f64) as usize];
        background(&mut audio, rng);
        for i in 0..n {
            add_sound(&mut audio, i as f64 * 0.5 + 0.1, sound, amp, rng);
        }
        e.process(&audio);
    };
    feed(&mut e, Sound::Ghost, 0.0, 2, &mut rng);
    e.action("record_hits");
    feed(&mut e, Sound::Slap, 0.6, 10, &mut rng);
    e.action("record_noise");
    feed(&mut e, Sound::Noise, 0.6, 10, &mut rng);
    e.action("record_stop");
    let st = e.state();
    assert!(st["calibration"]["trained"].as_bool().unwrap(), "{st}");
    feed(&mut e, Sound::Noise, 0.6, 6, &mut rng);
    let st = e.state();
    let peaks = st["peaks"].as_array().unwrap();
    let accepted = peaks.iter().filter(|p| p["accepted"].as_bool().unwrap()).count();
    assert!(accepted <= 1, "accepted {accepted} of {}", peaks.len());
}

#[test]
fn latency_test_measures_loopback() {
    let mut e = Engine::new(SR, 1);
    e.rpc(r#"{"cmd":"set_settings","patch":{"latency_test":{"output":{"port":"synth","channel":10,"note":37,"velocity":127},"listen_channel":0,"run_on_startup":true}}}"#);
    let mut rng = Rng(6);
    e.process(&vec![0.0; 4800]);
    e.action("latency_test");
    assert_eq!(e.mode(), Mode::LatencyTest);
    let true_latency = 0.037;
    let mut pending: Vec<f64> = Vec::new();
    let block = 256;
    let mut t_sample = 4800usize;
    for _ in 0..(6.0 * SR as f64 / block as f64) as usize {
        for ev in e.drain_events() {
            if ev.dest == Dest::Latency && ev.bytes[0] & 0xF0 == 0x90 {
                pending.push(ev.time + true_latency);
            }
        }
        let mut audio = vec![0.0f32; block];
        for (i, s) in audio.iter_mut().enumerate() {
            *s = 0.0005 * rng.next();
            let t = (t_sample + i) as f64 / SR as f64;
            for &p in &pending {
                if t >= p && t < p + 0.1 {
                    let tt = (t - p) as f32;
                    *s += 0.5 * rng.next() * (-tt * 60.0).exp();
                }
            }
        }
        t_sample += block;
        e.process(&audio);
    }
    assert_eq!(e.mode(), Mode::Stopped);
    let measured = e.persist.measured_latency_ms;
    println!("latency measured {measured} ms, message {}", e.message);
    assert!((measured - 37.0).abs() < 4.0, "{measured}");
}

#[test]
fn controls_learn_and_trigger() {
    let mut e = Engine::new(SR, 1);
    e.rpc(r#"{"cmd":"learn","action":"click"}"#);
    e.midi_in(&[0xB0, 64, 127]);
    assert_eq!(e.settings().bindings.len(), 1);
    let before = e.settings().click.enabled;
    e.midi_in(&[0xB0, 64, 0]);
    e.midi_in(&[0xB0, 64, 127]);
    assert_ne!(e.settings().click.enabled, before);
}

#[test]
fn tracker_cost_is_reasonable() {
    let mut e = Engine::new(SR, 1);
    let tr = e.new_tracker();
    println!("tracker cells {}", tr.cell_count());
    let mut rng = Rng(7);
    let mut audio = vec![0.0f32; (20.0 * SR) as usize];
    background(&mut audio, &mut rng);
    let truth = beats(0.5, 19.5, |_| 120.0);
    groove(&mut audio, &truth, 4, &mut rng, true, true);
    e.action("start");
    let t0 = std::time::Instant::now();
    for block in audio.chunks(256) {
        e.process(block);
        e.drain_events();
    }
    let el = t0.elapsed().as_secs_f64();
    println!("20 s of audio processed in {:.3} s", el);
}
