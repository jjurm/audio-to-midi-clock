//! Gamepad buttons as controls.

use std::sync::Arc;
use std::time::Duration;

use gilrs::{EventType, Gilrs};

use crate::host::Shared;

pub fn run(shared: Arc<Shared>) {
    let Ok(mut gilrs) = Gilrs::new() else { return };
    loop {
        while let Some(ev) = gilrs.next_event_blocking(Some(Duration::from_millis(200))) {
            if let EventType::ButtonPressed(button, _) = ev.event {
                let source = format!("pad:{button:?}");
                shared.with_engine(|e| e.control(&source));
            }
        }
    }
}
