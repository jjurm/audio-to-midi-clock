//! Bindable actions and turning raw control input into source descriptors.

use std::collections::HashMap;

/// Actions that can be bound to a MIDI control, gamepad button or key.
pub const ACTIONS: &[(&str, &str)] = &[
    ("start_stop", "Start / stop clock"),
    ("mark_beat_one", "Mark beat 1"),
    ("tap_tempo", "Tap tempo"),
    ("offset_up", "Offset +"),
    ("offset_down", "Offset −"),
    ("click", "Click on / off"),
    ("calibration", "Enter / leave calibration"),
    ("record_hits", "Record hits"),
    ("record_noise", "Record noise"),
    ("clear_calibration", "Clear calibration"),
    ("latency_test", "Run latency test"),
];

pub fn is_action(name: &str) -> bool {
    ACTIONS.iter().any(|(a, _)| *a == name)
}

/// Tracks continuous controllers so a CC only triggers when it crosses the
/// middle upwards (a button sending 127 then 0 triggers once).
#[derive(Default, Debug, Clone)]
pub struct MidiParser {
    cc: HashMap<(u8, u8), u8>,
}

impl MidiParser {
    /// Returns the source descriptor if this message is a "press".
    pub fn parse(&mut self, bytes: &[u8]) -> Option<String> {
        let status = *bytes.first()?;
        if status >= 0xF0 {
            return None;
        }
        let ch = (status & 0x0F) + 1;
        let d1 = bytes.get(1).copied().unwrap_or(0);
        let d2 = bytes.get(2).copied().unwrap_or(0);
        match status & 0xF0 {
            0x90 if d2 > 0 => Some(format!("midi:note:{ch}:{d1}")),
            0xB0 => {
                let prev = self.cc.insert((ch, d1), d2).unwrap_or(0);
                (d2 >= 64 && prev < 64).then(|| format!("midi:cc:{ch}:{d1}"))
            }
            0xC0 => Some(format!("midi:pc:{ch}:{d1}")),
            _ => None,
        }
    }
}

/// Human-readable form of a source descriptor.
pub fn describe(source: &str) -> String {
    let parts: Vec<&str> = source.split(':').collect();
    match parts.as_slice() {
        ["midi", "note", ch, n] => format!("MIDI note {n} (ch {ch})"),
        ["midi", "cc", ch, n] => format!("MIDI CC {n} (ch {ch})"),
        ["midi", "pc", ch, n] => format!("MIDI program {n} (ch {ch})"),
        ["pad", b] => format!("Gamepad {b}"),
        ["key", k] => format!("Key {}", k.trim_start_matches("Key")),
        _ => source.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cc_triggers_on_rising_edge() {
        let mut p = MidiParser::default();
        assert_eq!(p.parse(&[0xB0, 20, 127]).as_deref(), Some("midi:cc:1:20"));
        assert_eq!(p.parse(&[0xB0, 20, 127]), None);
        assert_eq!(p.parse(&[0xB0, 20, 0]), None);
        assert_eq!(p.parse(&[0xB0, 20, 100]).as_deref(), Some("midi:cc:1:20"));
        assert_eq!(p.parse(&[0x99, 36, 0]), None);
        assert_eq!(p.parse(&[0x99, 36, 90]).as_deref(), Some("midi:note:10:36"));
        assert_eq!(p.parse(&[0xF8]), None);
    }
}
