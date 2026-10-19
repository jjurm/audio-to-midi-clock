# Audio to MIDI Clock

Listens to live percussion (cajon, drum kit, anything with hits) and sends MIDI clock that follows the player. The percussionist plays freely without a click in their ears, while synths, sequencers and tap delays stay in time with them.

- **Web version:** https://jjurm.github.io/audio-to-midi-clock/ (Chrome, Edge or Firefox; at most two input channels in Chrome and Edge)
- **Desktop app:** macOS, Windows and Linux installers on the [Releases](https://github.com/jjurm/audio-to-midi-clock/releases) page. Uses every channel of your audio interface, including ASIO on Windows and JACK on Linux.

## Using it

1. **Inputs:** pick the audio device, turn on the channels with mics on them, and check the level meters move when you play.
2. **Output:** tick the MIDI ports that should receive the clock. Optionally set a click note to hear where the app thinks the beat is.
3. **Play:** press Start, play a count-in of even hits (four by default), and keep playing. The clock starts on the next beat after the count-in with a MIDI Start, follows your tempo, and sends MIDI Stop when you press Stop or stop playing for a few seconds.
4. **Offset:** with the click on, nudge the offset with + and − until the click sits on your hits. The offset is saved.

### Calibration

Detection works out of the box; **Sensitivity** on the Inputs screen is the everyday knob. To teach the app your sounds, go to **Calibrate** and press **Record hits**. A cursor sweeps across a two-second window; hit once each time it crosses the centre line. The app doesn't need clean recordings: in each sweep the loudest moment near the centre becomes a hit example, and everything else in the sweep, including the drum's ringing and any room noise, becomes a noise example. Record each kind of sound that should count, like bass and slap, and use **Record noise** for sounds that should never count. Each sweep appears under Recent sweeps; tap the right spot if the wrong moment was picked, or delete it.

The calibrated detector compares every audio frame with the saved examples (levels and rises in eight frequency bands per channel) and counts it as a hit when it is closer to some hit example than to any noise example, so any number of different sounds can count.

The **Filters** graph on the same screen shows each channel's live spectrum before and after its filters; drag the high-pass and low-pass handles in from the sides to cut rumble or hiss.

### Latency test

Route the sound of a MIDI instrument into one input, choose that instrument's MIDI output and the listening channel on the Calibrate screen, and run the test. It measures the delay from MIDI out to audio in. When the test runs again on a later day (automatically at startup, if enabled), the offset keeps your manual trim on top of the newly measured latency.

### Controls

Every action (start/stop, mark beat 1, tap tempo, offset ±, click, calibration, latency test) can be bound to a MIDI note or controller, a gamepad button or a key on the **Controls** screen: press Learn, then the control.

## How it works

The engine keeps a probability for every combination of tempo and position within the beat, a grid filter in the family of "bar pointer" beat trackers. There is no separate beat detector: every 2.5 ms frame of audio updates the grid from that frame's hit strength. Cells near a beat gain when the frame is loud, cells between beats and subdivisions lose. Old evidence fades with a half-life (5 s by default, on the Advanced screen), so a wrong guess always fades out, and tempo may drift by a set amount per second. The clock is then phase-locked to the best estimate and sent ahead of time by the offset, so the MIDI events arrive when the beat sounds.

All settings and calibration are saved automatically (desktop: a JSON file in the app data folder; web: browser storage).

## Building

Requirements: Rust (stable), Node.js 22, and for the desktop app the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/). On Linux also `libasound2-dev libjack-jackd2-dev libudev-dev`.

```sh
npm ci
cargo test -p engine --release        # engine tests, including generated drum audio

# Web version
rustup target add wasm32-unknown-unknown
npm run build:wasm                    # engine → ui/public/engine.wasm
npm run dev                           # http://localhost:5173

# Desktop app
npm run tauri dev
npm run tauri build                   # installers in target/release/bundle
```

Layout:

- `crates/engine`: the engine, pure Rust with no I/O (audio front end, tracker, clock scheduler, calibration, controls, JSON RPC)
- `crates/engine-wasm`: the engine as a WebAssembly module with plain exports
- `desktop`: the Tauri host (cpal audio, midir MIDI, gilrs gamepads, settings file)
- `ui`: the interface, shared by both versions (TypeScript, no framework), and the web host

### Releases

Pushing a tag like `v0.1.0` builds the desktop installers and publishes a release. The apps are not code-signed: on macOS, right-click the app and choose Open the first time (or run `xattr -cr "/Applications/Audio to MIDI Clock.app"`); on Windows, choose More info → Run anyway.
