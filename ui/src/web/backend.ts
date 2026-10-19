import type { AudioDevice, Backend, HostStatus, Settings } from "../types";
import { WasmEngine, type EngineEvent } from "./engine";

const STORAGE_KEY = "audio-to-midi-clock/persist";
/** Gamepad button names, matching the desktop app (standard mapping order). */
const PAD_BUTTONS = [
  "South", "East", "West", "North", "LeftTrigger", "RightTrigger", "LeftTrigger2", "RightTrigger2",
  "Select", "Start", "LeftThumb", "RightThumb", "DPadUp", "DPadDown", "DPadLeft", "DPadRight", "Mode",
];

/**
 * Backend for the web version: the engine runs as WebAssembly on the page,
 * audio comes from an AudioWorklet, MIDI goes out through Web MIDI with
 * timestamps so ticks leave at the right moment.
 */
export async function createWebBackend(): Promise<Backend> {
  const engine = await WasmEngine.load("./engine.wasm");
  const saved = localStorage.getItem(STORAGE_KEY);
  if (saved) {
    try {
      engine.rpc({ cmd: "load", persist: JSON.parse(saved) });
    } catch {
      /* ignore broken saved settings */
    }
  }
  let savedVersion = engine.persistVersion();
  let settings: Settings = engine.rpc({ cmd: "persist" }).settings;

  let ctx: AudioContext | null = null;
  let stream: MediaStream | null = null;
  let source: MediaStreamAudioSourceNode | null = null;
  let node: AudioWorkletNode | null = null;
  let openDevice: string | null = null;
  let audioError: string | null = null;
  let channels = 0;
  let midi: MIDIAccess | null = null;
  let midiError: string | null = null;

  // Map engine time -> audio context time -> performance.now() time.
  let anchorEngine = 0;
  let anchorCtx = 0;
  let perfOffset: number | null = null;

  function updatePerfOffset() {
    if (!ctx) return;
    const ts = ctx.getOutputTimestamp();
    if (ts.contextTime === undefined || ts.performanceTime === undefined || ts.performanceTime === 0) return;
    const d = ts.performanceTime / 1000 - ts.contextTime;
    perfOffset = perfOffset === null ? d : perfOffset + (d - perfOffset) * 0.05;
  }

  function dispatch(events: EngineEvent[]) {
    if (!midi || events.length === 0) return;
    for (const e of events) {
      const ctxTime = anchorCtx + (e.time - anchorEngine);
      const when = perfOffset === null ? performance.now() : (ctxTime + perfOffset) * 1000;
      const ports =
        e.dest === 0 ? settings.clock_outputs : e.dest === 1 ? [settings.click.output.port] : [settings.latency_test.output.port];
      for (const name of ports) {
        const out = findOutput(name);
        try {
          out?.send(e.data, when);
        } catch (err) {
          console.warn(err);
        }
      }
    }
  }

  function findOutput(name: string): MIDIOutput | undefined {
    if (!midi || !name) return undefined;
    for (const o of midi.outputs.values()) if (o.name === name) return o;
    return undefined;
  }

  async function openAudio() {
    const wanted = settings.audio_device;
    openDevice = wanted;
    audioError = null;
    try {
      if (!ctx) ctx = new AudioContext({ latencyHint: 0 });
      await ctx.resume();
      await ctx.audioWorklet.addModule("./capture-worklet.js").catch(() => {});
      stream?.getTracks().forEach((t) => t.stop());
      source?.disconnect();
      node?.disconnect();
      stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          deviceId: wanted ? { exact: wanted } : undefined,
          channelCount: { ideal: 32 },
          echoCancellation: false,
          noiseSuppression: false,
          autoGainControl: false,
        },
      });
      const track = stream.getAudioTracks()[0];
      channels = Math.max(1, track.getSettings().channelCount ?? 1);
      source = ctx.createMediaStreamSource(stream);
      node = new AudioWorkletNode(ctx, "capture", {
        numberOfInputs: 1,
        numberOfOutputs: 1,
        channelCount: channels,
        channelCountMode: "explicit",
        channelInterpretation: "discrete",
      });
      const mute = ctx.createGain();
      mute.gain.value = 0;
      source.connect(node);
      node.connect(mute).connect(ctx.destination);
      engine.configure(ctx.sampleRate, channels);
      const sr = ctx.sampleRate;
      node.port.onmessage = (m: MessageEvent<{ frame: number; channels: number; data: Float32Array }>) => {
        const { frame, data } = m.data;
        if (m.data.channels !== channels) {
          channels = m.data.channels;
          engine.configure(sr, channels);
        }
        updatePerfOffset();
        const events = engine.process(data);
        anchorEngine = engine.sampleTime();
        anchorCtx = (frame + data.length / channels) / sr;
        dispatch(events);
        maybeSave();
      };
    } catch (err) {
      const name = err instanceof Error ? err.name : "";
      audioError =
        name === "NotAllowedError"
          ? "Audio input blocked. Allow the microphone for this page in the browser's site settings, then reload."
          : name === "NotFoundError" || name === "OverconstrainedError"
            ? "No audio input found. Connect one, then choose it on the Inputs screen."
            : `Audio input failed: ${err instanceof Error ? err.message : String(err)}`;
    }
  }

  async function openMidi() {
    if (!navigator.requestMIDIAccess) {
      midiError = "This browser has no Web MIDI. Use Chrome, Edge or Firefox, or the desktop app.";
      return;
    }
    try {
      midi = await navigator.requestMIDIAccess({ sysex: false });
      const listen = () => {
        for (const input of midi!.inputs.values()) {
          input.onmidimessage = (m) => {
            const d = m.data;
            if (!d || d.length === 0 || d[0] >= 0xf0) return;
            call({ cmd: "midi_in", bytes: Array.from(d) });
          };
        }
      };
      listen();
      midi.onstatechange = listen;
    } catch (err) {
      midiError = `MIDI access refused: ${err instanceof Error ? err.message : String(err)}`;
    }
  }

  // Gamepads: poll the standard mapping and report presses.
  const padPrev = new Map<number, boolean[]>();
  function pollPads() {
    for (const pad of navigator.getGamepads?.() ?? []) {
      if (!pad) continue;
      const prev = padPrev.get(pad.index) ?? [];
      pad.buttons.forEach((b, i) => {
        if (b.pressed && !prev[i]) call({ cmd: "control", source: `pad:${PAD_BUTTONS[i] ?? `Button${i}`}` });
      });
      padPrev.set(
        pad.index,
        pad.buttons.map((b) => b.pressed),
      );
    }
    requestAnimationFrame(pollPads);
  }

  function maybeSave() {
    const v = engine.persistVersion();
    if (v !== savedVersion) {
      savedVersion = v;
      const p = engine.rpc({ cmd: "persist" });
      settings = p.settings;
      localStorage.setItem(STORAGE_KEY, JSON.stringify(p));
      if (ctx && settings.audio_device !== openDevice) openAudio();
    }
  }

  function call(request: Record<string, unknown>) {
    const res = engine.rpc(request);
    dispatch(engine.takeEvents());
    maybeSave();
    return res;
  }

  async function listAudioDevices(): Promise<AudioDevice[]> {
    const all = (await navigator.mediaDevices?.enumerateDevices?.()) ?? [];
    return all
      .filter((d) => d.kind === "audioinput" && d.deviceId && d.deviceId !== "default")
      .map((d, i) => ({ id: d.deviceId, name: d.label || `Input device ${i + 1}` }));
  }

  return {
    kind: "web",
    needsGesture: true,
    async start() {
      await openMidi();
      await openAudio();
      requestAnimationFrame(pollPads);
      // Browsers may suspend audio; any click brings it back.
      document.addEventListener("pointerdown", () => ctx?.resume());
    },
    async rpc(request) {
      return call(request);
    },
    listAudioDevices,
    async hostStatus(): Promise<HostStatus> {
      const outs = midi ? [...midi.outputs.values()].map((o) => o.name ?? "") : [];
      const ins = midi ? [...midi.inputs.values()].map((o) => o.name ?? "") : [];
      const configured = [...settings.clock_outputs, settings.click.output.port, settings.latency_test.output.port].filter(Boolean);
      return {
        audio: {
          device: openDevice ? ((await listAudioDevices()).find((d) => d.id === openDevice)?.name ?? "") : "",
          running: !!node && !audioError && ctx?.state === "running",
          channels,
          sample_rate: ctx?.sampleRate ?? 0,
          error: audioError ?? (ctx && ctx.state !== "running" && node ? "Audio is paused. Click anywhere on the page." : null),
        },
        midi_outputs: outs,
        midi_inputs: ins,
        missing_ports: configured.filter((p) => !outs.includes(p)),
        notes: midiError ? [midiError] : [],
      };
    },
  };
}
