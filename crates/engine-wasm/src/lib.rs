//! WebAssembly entry points for the web version.
//!
//! The module imports nothing, so it can be instantiated anywhere, including
//! inside an AudioWorklet. Data moves through buffers in the module's memory:
//! the host asks for a buffer pointer, writes into it, then calls the function.

use std::cell::RefCell;

use engine::Engine;

struct State {
    engine: Engine,
    input: Vec<f32>,
    events: Vec<f64>,
    rpc_in: Vec<u8>,
    rpc_out: Vec<u8>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.get_or_insert_with(|| State {
            engine: Engine::new(48000.0, 1),
            input: Vec::new(),
            events: Vec::new(),
            rpc_in: Vec::new(),
            rpc_out: Vec::new(),
        });
        f(st)
    })
}

/// Set the audio format. Safe to call again when the input changes.
#[unsafe(no_mangle)]
pub extern "C" fn configure(sample_rate: f32, channels: u32) {
    with(|s| s.engine.configure(sample_rate, channels.max(1) as usize));
}

/// Pointer to a buffer for `samples` interleaved f32 samples.
#[unsafe(no_mangle)]
pub extern "C" fn input_buffer(samples: u32) -> *mut f32 {
    with(|s| {
        s.input.resize(samples as usize, 0.0);
        s.input.as_mut_ptr()
    })
}

/// Process the first `samples` values of the input buffer. Returns the number
/// of events; each takes four f64 at `events_ptr()`: time, destination,
/// length, bytes packed little-endian.
#[unsafe(no_mangle)]
pub extern "C" fn process(samples: u32) -> u32 {
    with(|s| {
        let n = (samples as usize).min(s.input.len());
        s.engine.process(&s.input[..n]);
        collect_events(s)
    })
}

/// Events generated outside `process` (e.g. by an RPC call).
#[unsafe(no_mangle)]
pub extern "C" fn take_events() -> u32 {
    with(collect_events)
}

fn collect_events(s: &mut State) -> u32 {
    s.events.clear();
    let evs = s.engine.drain_events();
    for e in &evs {
        let packed = e.bytes[0] as u32 | (e.bytes[1] as u32) << 8 | (e.bytes[2] as u32) << 16;
        s.events.extend_from_slice(&[e.time, e.dest as u8 as f64, e.len as f64, packed as f64]);
    }
    evs.len() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn events_ptr() -> *const f64 {
    with(|s| s.events.as_ptr())
}

/// Pointer to a buffer for a UTF-8 JSON request of `len` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn rpc_buffer(len: u32) -> *mut u8 {
    with(|s| {
        s.rpc_in.resize(len as usize, 0);
        s.rpc_in.as_mut_ptr()
    })
}

/// Run the request in the RPC buffer; returns the response length.
#[unsafe(no_mangle)]
pub extern "C" fn rpc() -> u32 {
    with(|s| {
        let req = String::from_utf8_lossy(&s.rpc_in).into_owned();
        s.rpc_out = s.engine.rpc(&req).into_bytes();
        s.rpc_out.len() as u32
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rpc_result() -> *const u8 {
    with(|s| s.rpc_out.as_ptr())
}

/// Engine time in seconds (input timeline).
#[unsafe(no_mangle)]
pub extern "C" fn now() -> f64 {
    with(|s| s.engine.now())
}

#[unsafe(no_mangle)]
pub extern "C" fn persist_version() -> f64 {
    with(|s| s.engine.persist_version() as f64)
}
