// Mirrors the engine's settings and status JSON.

export interface ChannelSettings {
  enabled: boolean;
  weight: number;
  highpass_hz: number;
  lowpass_hz: number;
}

export interface NoteOutput {
  port: string;
  channel: number;
  note: number;
  velocity: number;
}

export interface Binding {
  action: string;
  source: string;
}

export interface Settings {
  audio_device: string;
  buffer_frames: number;
  channels: ChannelSettings[];
  sensitivity: number;
  clock_outputs: string[];
  click: { enabled: boolean; output: NoteOutput; accent_velocity: number };
  latency_test: { output: NoteOutput; listen_channel: number | null; run_on_startup: boolean };
  offset_trim_ms: number;
  offset_step_ms: number;
  lookahead_ms: number;
  half_life_s: number;
  tempo_drift_pct: number;
  tempo_min_bpm: number;
  tempo_max_bpm: number;
  beats_per_bar: number;
  count_in_hits: number;
  auto_stop: boolean;
  auto_stop_after_s: number;
  bindings: Binding[];
}

export interface Persist {
  settings: Settings;
  examples: unknown[];
  measured_latency_ms: number;
}

export type Mode = "stopped" | "count_in" | "running" | "calibrating" | "latency_test";

export interface EngineState {
  mode: Mode;
  time: number;
  sample_rate: number;
  channels: number;
  count_in: { have: number; need: number };
  bpm: number | null;
  tap_bpm: number | null;
  phase: number | null;
  confidence: number | null;
  beat: { position: number; in_bar: number; bar: number; fraction: number } | null;
  beats_per_bar: number;
  offset_ms: number;
  measured_latency_ms: number;
  trim_ms: number;
  meters: number[];
  strength: number;
  threshold: number;
  reference: number;
  peaks: { age: number; accepted: boolean; strength: number }[];
  calibration: {
    hits: number;
    noise: number;
    mismatched: number;
    trained: boolean;
    recording: "hits" | "noise" | null;
    sweep: { position: number; length_s: number; tolerance_s: number; trace: number[] } | null;
    history_version: number;
    min_hits: number;
    min_noise: number;
  };
  learning: string | null;
  message: string;
  click: boolean;
  version: number;
}

export interface AudioDevice {
  id: string;
  name: string;
}

export interface HostStatus {
  audio: { device: string; running: boolean; channels: number; sample_rate: number; error: string | null };
  midi_outputs: string[];
  midi_inputs: string[];
  /** Port names that are configured but could not be opened. */
  missing_ports: string[];
  notes: string[];
}

export interface Backend {
  kind: "desktop" | "web";
  /** Web only: audio and MIDI need a user gesture before they can start. */
  needsGesture: boolean;
  start(): Promise<void>;
  rpc<T = any>(request: Record<string, unknown>): Promise<T>;
  listAudioDevices(): Promise<AudioDevice[]>;
  hostStatus(): Promise<HostStatus>;
}
