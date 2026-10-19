//! Desktop host: audio in (cpal), MIDI in/out (midir), gamepads (gilrs),
//! settings file, and the web UI through Tauri.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod host;
mod midi;
mod pads;

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use tauri::{Manager, State};

use host::{HostStatus, Shared};

type AppState = Arc<Shared>;

#[tauri::command(async)]
fn rpc(state: State<'_, AppState>, request: String) -> String {
    let res = state.with_engine(|e| e.rpc(&request));
    // Device or port settings may have changed.
    if request.contains("set_settings") || request.contains("\"load\"") {
        state.sync_io();
    }
    res
}

/// Device enumeration can be slow, so the list is refreshed at most every few seconds.
#[tauri::command(async)]
fn list_audio_devices(state: State<'_, AppState>) -> Vec<audio::DeviceInfo> {
    let mut cache = state.device_cache.lock().unwrap();
    let stale = cache.as_ref().is_none_or(|(t, _)| t.elapsed() > Duration::from_secs(5));
    if stale {
        *cache = Some((std::time::Instant::now(), audio::list_devices()));
    }
    cache.as_ref().map(|(_, d)| d.clone()).unwrap_or_default()
}

#[tauri::command(async)]
fn host_status(state: State<'_, AppState>) -> HostStatus {
    state.status()
}

/// Start audio and MIDI. Called by the UI once it is ready; safe to call twice.
#[tauri::command]
fn start_host(state: State<'_, AppState>) {
    let shared = state.inner().clone();
    {
        let mut started = shared.started.lock().unwrap();
        if *started {
            return;
        }
        *started = true;
    }
    let (atx, arx) = channel();
    let (mtx, mrx) = channel();
    *shared.audio_tx.lock().unwrap() = Some(atx);
    *shared.midi_tx.lock().unwrap() = Some(mtx);
    let s = shared.clone();
    std::thread::Builder::new().name("audio".into()).spawn(move || audio::run(s, arx)).ok();
    let s = shared.clone();
    std::thread::Builder::new().name("midi".into()).spawn(move || midi::run(s, mrx)).ok();
    let s = shared.clone();
    std::thread::Builder::new().name("gamepads".into()).spawn(move || pads::run(s)).ok();
    shared.sync_io();
}

/// Save settings whenever they change.
fn autosave(shared: Arc<Shared>) {
    let mut saved = u64::MAX;
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let (v, json) = {
            let e = shared.engine.lock().unwrap();
            let v = e.persist_version();
            if v == saved {
                continue;
            }
            (v, e.export_persist())
        };
        let Some(path) = shared.settings_path.lock().unwrap().clone() else { continue };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path)).is_ok() {
            saved = v;
            // Ports and devices may have changed through a control action.
            shared.sync_io();
        }
    }
}

fn main() {
    let shared = Shared::new();
    tauri::Builder::default()
        .manage(shared.clone())
        .setup(move |app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("settings.json");
            if let Ok(json) = std::fs::read_to_string(&path) {
                if let Err(err) = shared.engine.lock().unwrap().load_persist(&json) {
                    eprintln!("Ignoring unreadable settings file: {err}");
                }
            }
            *shared.settings_path.lock().unwrap() = Some(path);
            let s = shared.clone();
            std::thread::Builder::new().name("autosave".into()).spawn(move || autosave(s))?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![rpc, list_audio_devices, host_status, start_host])
        .run(tauri::generate_context!())
        .expect("error while running the app");
}
