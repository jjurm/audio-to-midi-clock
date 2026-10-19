//! Audio input: one thread owns the cpal stream and reopens it when the
//! device setting changes or the device disappears.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample, StreamConfig, SupportedBufferSize};
use serde::Serialize;

use crate::host::{AudioStatus, Shared};

pub enum AudioCmd {
    Open { device: String, buffer_frames: u32 },
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// All input devices of all available audio systems (e.g. WASAPI and ASIO).
pub fn list_devices() -> Vec<DeviceInfo> {
    let hosts = cpal::available_hosts();
    let multi = hosts.len() > 1;
    let mut out = Vec::new();
    for id in hosts {
        let Ok(host) = cpal::host_from_id(id) else { continue };
        let Ok(devices) = host.devices() else { continue };
        for d in devices {
            if !d.supports_input() {
                continue;
            }
            let Ok(dev_id) = d.id() else { continue };
            let mut name = d.description().map(|x| x.to_string()).unwrap_or_else(|_| dev_id.to_string());
            if let Ok(cfg) = d.supported_input_configs() {
                if let Some(ch) = cfg.map(|c| c.channels()).max() {
                    name = format!("{name} ({ch} ch)");
                }
            }
            if multi {
                name = format!("{}: {name}", id.name());
            }
            out.push(DeviceInfo { id: dev_id.to_string(), name });
        }
    }
    out
}

fn find_device(id: &str) -> Option<cpal::Device> {
    if id.is_empty() {
        return cpal::default_host().default_input_device();
    }
    for hid in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(hid) else { continue };
        let Ok(devices) = host.devices() else { continue };
        for d in devices {
            if d.id().map(|x| x.to_string()).as_deref() == Ok(id) {
                return Some(d);
            }
        }
    }
    None
}

fn format_rank(f: SampleFormat) -> u8 {
    match f {
        SampleFormat::F32 => 5,
        SampleFormat::I32 => 4,
        SampleFormat::I24 => 3,
        SampleFormat::I16 => 2,
        _ => 1,
    }
}

pub fn run(shared: Arc<Shared>, rx: Receiver<AudioCmd>) {
    let failed = Arc::new(AtomicBool::new(false));
    let mut stream: Option<cpal::Stream> = None;
    let mut current: Option<(String, u32)> = None;
    let mut last_try = Instant::now();
    loop {
        let wanted = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(AudioCmd::Open { device, buffer_frames }) => {
                let w = (device, buffer_frames);
                if current.as_ref() == Some(&w) && stream.is_some() && !failed.load(Ordering::Relaxed) {
                    continue;
                }
                Some(w)
            }
            Err(RecvTimeoutError::Timeout) => {
                // Retry a lost or missing device every few seconds.
                let lost = failed.load(Ordering::Relaxed) || stream.is_none();
                if lost && current.is_some() && last_try.elapsed() > Duration::from_secs(3) {
                    current.clone()
                } else {
                    None
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let Some((device, buffer)) = wanted else { continue };
        stream = None;
        last_try = Instant::now();
        failed.store(false, Ordering::Relaxed);
        current = Some((device.clone(), buffer));
        match open(&shared, &device, buffer, failed.clone()) {
            Ok((s, status)) => {
                stream = Some(s);
                *shared.audio_status.lock().unwrap() = status;
            }
            Err(e) => {
                let mut st = shared.audio_status.lock().unwrap();
                st.running = false;
                st.error = Some(e);
            }
        }
    }
}

fn open(shared: &Arc<Shared>, id: &str, buffer: u32, failed: Arc<AtomicBool>) -> Result<(cpal::Stream, AudioStatus), String> {
    let device = find_device(id).ok_or_else(|| if id.is_empty() { "No audio input found".to_string() } else { "Audio device not found".to_string() })?;
    let name = device.description().map(|d| d.to_string()).unwrap_or_default();
    let default = device.default_input_config().map_err(|e| e.to_string())?;
    let rate = default.sample_rate();
    // Use every channel the device offers at its default sample rate. ALSA
    // software devices (default, pulse, pipewire) accept any channel count by
    // copying channels, and JACK offers as many ports as asked for, so those
    // use their default channel count (JACK: the physical capture ports).
    let dev_id = device.id().map(|i| i.to_string()).unwrap_or_default();
    let alsa_soft = (dev_id.starts_with("alsa:") && !dev_id.contains("hw:")) || dev_id.starts_with("jack:");
    let best = device
        .supported_input_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| c.min_sample_rate() <= rate && rate <= c.max_sample_rate())
        .max_by_key(|c| (c.channels(), format_rank(c.sample_format())))
        .map(|c| c.with_sample_rate(rate));
    let supported = match best {
        Some(b) if !alsa_soft && b.channels() <= 64 => b,
        _ => default,
    };
    let channels = supported.channels() as usize;
    let buffer_size = match (buffer, supported.buffer_size()) {
        (0, _) => BufferSize::Default,
        (n, SupportedBufferSize::Range { min, max }) => BufferSize::Fixed(n.clamp(*min, *max)),
        (n, SupportedBufferSize::Unknown) => BufferSize::Fixed(n),
    };
    let config = StreamConfig { channels: supported.channels(), sample_rate: rate, buffer_size };
    shared.with_engine(|e| e.configure(rate as f32, channels));

    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, shared, failed),
        SampleFormat::I16 => build::<i16>(&device, &config, shared, failed),
        SampleFormat::I32 => build::<i32>(&device, &config, shared, failed),
        SampleFormat::I24 => build::<cpal::I24>(&device, &config, shared, failed),
        SampleFormat::U16 => build::<u16>(&device, &config, shared, failed),
        SampleFormat::F64 => build::<f64>(&device, &config, shared, failed),
        SampleFormat::I8 => build::<i8>(&device, &config, shared, failed),
        SampleFormat::U8 => build::<u8>(&device, &config, shared, failed),
        f => return Err(format!("Unsupported sample format {f}")),
    }
    .or_else(|e| {
        // Some drivers refuse a fixed buffer size; fall back to the default.
        if matches!(config.buffer_size, BufferSize::Fixed(_)) {
            let c = StreamConfig { buffer_size: BufferSize::Default, ..config.clone() };
            match supported.sample_format() {
                SampleFormat::F32 => build::<f32>(&device, &c, shared, Arc::new(AtomicBool::new(false))),
                SampleFormat::I16 => build::<i16>(&device, &c, shared, Arc::new(AtomicBool::new(false))),
                SampleFormat::I32 => build::<i32>(&device, &c, shared, Arc::new(AtomicBool::new(false))),
                _ => Err(e),
            }
        } else {
            Err(e)
        }
    })
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, AudioStatus { device: name, running: true, channels, sample_rate: rate as f32, error: None }))
}

fn build<T>(device: &cpal::Device, config: &StreamConfig, shared: &Arc<Shared>, failed: Arc<AtomicBool>) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let sh = shared.clone();
    let sh_err = shared.clone();
    let mut buf: Vec<f32> = Vec::new();
    device.build_input_stream::<T, _, _>(
        config.clone(),
        move |data: &[T], _| {
            let now = Instant::now();
            buf.clear();
            buf.extend(data.iter().map(|&s| <f32 as FromSample<T>>::from_sample_(s)));
            let events = {
                let mut e = sh.engine.lock().unwrap();
                e.process(&buf);
                sh.timemap.lock().unwrap().observe(now, e.sample_time());
                e.drain_events()
            };
            sh.dispatch(events);
        },
        move |err| {
            let kind = err.kind();
            if matches!(kind, cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied) {
                return;
            }
            failed.store(true, Ordering::Relaxed);
            let mut st = sh_err.audio_status.lock().unwrap();
            st.running = false;
            st.error = Some(format!("Audio stopped: {err}"));
        },
        None,
    )
}
