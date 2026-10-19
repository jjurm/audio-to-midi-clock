import "@fontsource-variable/archivo/standard.css";
import "./style.css";
import type { AudioDevice, Backend, EngineState, HostStatus, Persist, Settings } from "./types";
import { h, clear } from "./dom";
import { describeSource } from "./describe";
import { drawScope, drawSpectrum, drawThumb, freqToX, xToFreq, type SpectrumView, type SweepView } from "./canvas";

const isDesktop = "__TAURI_INTERNALS__" in window;
const backend: Backend = isDesktop
  ? await (await import("./desktop")).createDesktopBackend()
  : await (await import("./web/backend")).createWebBackend();

let persist: Persist = await backend.rpc({ cmd: "persist" });
let state: EngineState = await backend.rpc({ cmd: "state" });
let host: HostStatus = await backend.hostStatus();
let devices: AudioDevice[] = [];
const actions: { id: string; label: string }[] = await backend.rpc({ cmd: "actions" });

type ScreenId = "play" | "inputs" | "calibrate" | "output" | "controls" | "advanced";
const SCREENS: [ScreenId, string][] = [
  ["play", "Play"],
  ["inputs", "Inputs"],
  ["calibrate", "Calibrate"],
  ["output", "Output"],
  ["controls", "Controls"],
  ["advanced", "Advanced"],
];
let screen: ScreenId = (localStorage.getItem("screen") as ScreenId) || "play";

/** Called on every status poll. */
let liveUpdaters: (() => void)[] = [];
/** Called when settings change; update controls that aren't being edited. */
let refreshers: (() => void)[] = [];
/** Called when the screen is left (timers, animation loops). */
let cleanups: (() => void)[] = [];
/** Signature of the data the current screen's structure depends on. */
let structureKey = "";

// ----- settings helpers -------------------------------------------------------

type Path = (string | number)[];

function getPath(path: Path): any {
  let v: any = persist.settings;
  for (const p of path) v = v?.[p];
  return v;
}

async function setPath(path: Path, value: unknown) {
  const top = path[0] as keyof Settings;
  const copy = structuredClone(persist.settings[top]) as any;
  if (path.length === 1) {
    await patch({ [top]: value });
    return;
  }
  let o = copy;
  for (const p of path.slice(1, -1)) o = o[p];
  o[path[path.length - 1]] = value;
  await patch({ [top]: copy });
}

async function patch(p: Partial<Settings>) {
  const r = await backend.rpc({ cmd: "set_settings", patch: p });
  if (r?.error) toast(r.error);
  await reloadPersist();
}

async function reloadPersist() {
  persist = await backend.rpc({ cmd: "persist" });
  const key = computeStructureKey();
  if (key !== structureKey) renderScreen();
  else refreshers.forEach((f) => f());
}

function action(name: string) {
  return backend.rpc({ cmd: "action", name });
}

// ----- bound controls ---------------------------------------------------------

function numberInput(path: Path, opts: { min?: number; max?: number; step?: number; unit?: string } = {}) {
  const input = h("input", { type: "number", min: opts.min, max: opts.max, step: opts.step ?? 1 }) as HTMLInputElement;
  const refresh = () => {
    if (document.activeElement !== input) input.value = String(round(getPath(path), 3));
  };
  input.addEventListener("change", () => {
    const v = Number(input.value);
    if (Number.isFinite(v)) setPath(path, v);
  });
  refreshers.push(refresh);
  refresh();
  return opts.unit ? h("span", { class: "with-unit" }, input, h("span", { class: "unit" }, opts.unit)) : input;
}

function checkbox(path: Path, label: string) {
  const input = h("input", { type: "checkbox" }) as HTMLInputElement;
  const refresh = () => (input.checked = !!getPath(path));
  input.addEventListener("change", () => setPath(path, input.checked));
  refreshers.push(refresh);
  refresh();
  return h("label", { class: "check" }, input, h("span", {}, label));
}

function select(path: Path, options: () => [string | number | null, string][], parse: (v: string) => unknown = (v) => v) {
  const el = h("select") as HTMLSelectElement;
  const refresh = () => {
    const cur = getPath(path);
    const opts = options();
    if (!opts.some(([v]) => v === cur) && cur !== undefined) opts.push([cur, `${cur} (not connected)`]);
    clear(el);
    for (const [v, label] of opts) {
      const o = h("option", { value: v === null ? "" : String(v) }, label) as HTMLOptionElement;
      if (v === cur) o.selected = true;
      el.append(o);
    }
  };
  el.addEventListener("change", () => setPath(path, parse(el.value)));
  refreshers.push(refresh);
  refresh();
  return el;
}

function slider(path: Path, min: number, max: number, step: number, fmt: (v: number) => string) {
  const input = h("input", { type: "range", min, max, step }) as HTMLInputElement;
  const out = h("output", {});
  const refresh = () => {
    if (document.activeElement !== input) input.value = String(getPath(path));
    out.textContent = fmt(Number(input.value));
  };
  input.addEventListener("input", () => (out.textContent = fmt(Number(input.value))));
  input.addEventListener("change", () => setPath(path, Number(input.value)));
  refreshers.push(refresh);
  refresh();
  return h("span", { class: "slider" }, input, out);
}

function field(label: string, control: Node, hint?: string) {
  return h("div", { class: "field" }, h("label", { class: "field-label" }, label), h("div", { class: "field-control" }, control, hint ? h("p", { class: "hint" }, hint) : null));
}

function round(v: number, digits = 1) {
  const f = 10 ** digits;
  return Math.round(v * f) / f;
}

const portOptions = (): [string, string][] => [["", "None"], ...host.midi_outputs.map((p): [string, string] => [p, p])];
const channelOptions = (): [number, string][] => Array.from({ length: 16 }, (_, i): [number, string] => [i + 1, `Channel ${i + 1}`]);
const inputName = (i: number) => `Input ${i + 1}`;

// ----- shell ------------------------------------------------------------------

const app = document.getElementById("app")!;
const statusLine = h("div", { class: "host-status" });
const tabs = h("nav", { class: "tabs" });
const main = h("main", { class: "screen" });
const toastEl = h("div", { class: "toast", role: "status" });
app.append(h("header", { class: "top" }, h("h1", { class: "brand" }, "Audio to MIDI Clock"), statusLine), tabs, main, toastEl);

for (const [id, label] of SCREENS) {
  const b = h("button", { class: "tab", "data-id": id }, label);
  b.addEventListener("click", () => {
    screen = id;
    localStorage.setItem("screen", id);
    renderScreen();
  });
  tabs.append(b);
}

let toastTimer = 0;
function toast(msg: string) {
  toastEl.textContent = msg;
  toastEl.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => toastEl.classList.remove("show"), 4000);
}

function computeStructureKey() {
  return JSON.stringify([screen, state.channels, host.midi_outputs, devices.map((d) => d.id), persist.settings.bindings, persist.settings.beats_per_bar]);
}

function renderScreen() {
  cleanups.forEach((f) => f());
  cleanups = [];
  liveUpdaters = [];
  refreshers = [];
  structureKey = computeStructureKey();
  for (const b of tabs.querySelectorAll<HTMLElement>(".tab")) b.classList.toggle("active", b.dataset.id === screen);
  clear(main);
  const render = { play: renderPlay, inputs: renderInputs, calibrate: renderCalibrate, output: renderOutput, controls: renderControls, advanced: renderAdvanced }[screen];
  main.append(render());
  liveUpdaters.forEach((f) => f());
}

function renderHostStatus() {
  const a = host.audio;
  const parts: Node[] = [];
  if (a.error) parts.push(h("span", { class: "bad" }, a.error));
  else if (a.running) parts.push(h("span", { class: "ok" }, `${a.device || "Default input"}: ${a.channels} ch, ${round(a.sample_rate / 1000, 1)} kHz`));
  else parts.push(h("span", { class: "bad" }, "No audio input"));
  const clocks = persist.settings.clock_outputs;
  if (clocks.length === 0) parts.push(h("span", { class: "warn" }, "No clock output"));
  else parts.push(h("span", { class: host.missing_ports.some((p) => clocks.includes(p)) ? "bad" : "ok" }, `Clock to ${clocks.join(", ")}`));
  for (const n of host.notes) parts.push(h("span", { class: "bad" }, n));
  clear(statusLine);
  statusLine.append(...parts);
}

// ----- shared widgets ---------------------------------------------------------

/** Recent sounds on a 3-second timeline, newest on the right. */
function hitStrip() {
  const strip = h("div", { class: "hit-strip", "aria-hidden": "true" });
  liveUpdaters.push(() => {
    clear(strip);
    for (const p of state.peaks) {
      const size = Math.min(1, p.strength / Math.max(1, state.reference));
      const dot = h("span", { class: `dot ${p.accepted ? "hit" : "noise"}` });
      dot.style.left = `${100 - (p.age / 3) * 100}%`;
      dot.style.setProperty("--s", String(0.45 + 0.55 * size));
      strip.append(dot);
    }
  });
  return h(
    "div",
    { class: "strip-wrap" },
    strip,
    h(
      "div",
      { class: "strip-legend" },
      h("span", { class: "key hit" }, "Counted as a hit"),
      h("span", { class: "key noise" }, "Heard, ignored"),
      h("span", { class: "key size" }, "Bigger is louder"),
      h("span", { class: "key now" }, "Now"),
    ),
  );
}

// ----- Play -------------------------------------------------------------------

function renderPlay() {
  const modeLine = h("p", { class: "mode-line" });
  const bpm = h("span", { class: "bpm-value" });
  const lamps = h("div", { class: "lamps" });
  for (let i = 0; i < state.beats_per_bar; i++) lamps.append(h("span", { class: i === 0 ? "lamp one" : "lamp" }));
  const confFill = h("span", { class: "conf-fill" });
  const confText = h("span", { class: "conf-text" });
  const offsetValue = h("strong", {});
  const offsetDetail = h("span", { class: "offset-detail" });
  const startBtn = h("button", { class: "start" }) as HTMLButtonElement;
  startBtn.addEventListener("click", () => action("start_stop"));
  const clickBtn = h("button", { class: "secondary" }, "Click") as HTMLButtonElement;
  clickBtn.addEventListener("click", () => action("click"));
  const tapBtn = h("button", { class: "secondary" }, "Tap tempo");
  tapBtn.addEventListener("click", () => action("tap_tempo"));
  const markBtn = h("button", { class: "secondary" }, "Mark beat 1");
  markBtn.addEventListener("click", () => action("mark_beat_one"));
  const minus = h("button", { class: "nudge", "aria-label": "Offset down" }, "−");
  minus.addEventListener("click", () => action("offset_down"));
  const plus = h("button", { class: "nudge", "aria-label": "Offset up" }, "+");
  plus.addEventListener("click", () => action("offset_up"));

  liveUpdaters.push(() => {
    const s = state;
    const running = s.mode === "running";
    modeLine.textContent =
      s.mode === "count_in"
        ? `Play ${s.count_in.need} even hits to start: ${s.count_in.have} so far`
        : s.mode === "running"
          ? "Following"
          : s.mode === "calibrating"
            ? "Calibrating, clock stopped"
            : s.mode === "latency_test"
              ? "Measuring latency…"
              : "Stopped";
    modeLine.dataset.mode = s.mode;
    const shown = running ? s.bpm : (s.tap_bpm ?? s.bpm);
    bpm.textContent = shown ? shown.toFixed(1) : "No tempo yet";
    bpm.classList.toggle("dim", !running);
    bpm.classList.toggle("empty", !shown);
    const lampEls = lamps.children;
    for (let i = 0; i < lampEls.length; i++) {
      const on = running && s.beat && s.beat.in_bar === i;
      lampEls[i].classList.toggle("on", !!on);
      if (on) (lampEls[i] as HTMLElement).style.setProperty("--glow", String(1 - Math.min(1, s.beat!.fraction * 1.6)));
    }
    const c = s.confidence ?? 0;
    confFill.style.width = `${Math.round(c * 100)}%`;
    confText.textContent = running ? `Confidence ${Math.round(c * 100)}%` : "Confidence";
    offsetValue.textContent = `${s.offset_ms.toFixed(1)} ms`;
    offsetDetail.textContent = `measured ${s.measured_latency_ms.toFixed(1)} + trim ${s.trim_ms >= 0 ? "" : "−"}${Math.abs(s.trim_ms).toFixed(1)}`;
    startBtn.textContent = running || s.mode === "count_in" ? "Stop" : "Start";
    startBtn.classList.toggle("stop", running || s.mode === "count_in");
    clickBtn.classList.toggle("active", s.click);
    clickBtn.setAttribute("aria-pressed", String(s.click));
  });

  return h(
    "section",
    { class: "play" },
    modeLine,
    h("div", { class: "tempo" }, bpm, h("span", { class: "bpm-unit" }, "BPM")),
    lamps,
    h("div", { class: "confidence" }, h("span", { class: "conf-track" }, confFill), confText),
    h("div", { class: "transport" }, startBtn),
    h("div", { class: "offset" }, minus, h("div", { class: "offset-text" }, h("span", {}, "Offset "), offsetValue, offsetDetail), plus),
    h("div", { class: "row-buttons" }, tapBtn, markBtn, clickBtn),
    hitStrip(),
  );
}

// ----- Inputs -----------------------------------------------------------------

function renderInputs() {
  const deviceOptions = (): [string, string][] => [["", "System default"], ...devices.map((d): [string, string] => [d.id, d.name])];
  const sec = h("section", { class: "form" });
  sec.append(h("h2", {}, "Audio input"));
  if (backend.kind === "web")
    sec.append(
      h(
        "p",
        { class: "note" },
        "Browsers give web pages at most two channels of an audio interface (Chrome and Edge), and Safari has no MIDI. For more channels use the desktop app.",
      ),
    );
  sec.append(field("Device", select(["audio_device"], deviceOptions)));
  if (backend.kind === "desktop")
    sec.append(
      field(
        "Buffer size",
        select(["buffer_frames"], () => [[0, "Device default"], [64, "64 frames"], [128, "128 frames"], [256, "256 frames"], [512, "512 frames"], [1024, "1024 frames"]], Number),
        "Smaller buffers lower the latency; raise it if you hear dropouts.",
      ),
    );

  const strengthFill = h("span", { class: "meter-fill" });
  const thresholdMark = h("span", { class: "meter-mark" });
  let held = 0;
  liveUpdaters.push(() => {
    held = Math.max(state.strength, held * 0.9);
    const scale = Math.max(state.reference * 2, 1);
    strengthFill.style.width = `${Math.min(100, (held / scale) * 100)}%`;
    thresholdMark.style.left = `${Math.min(100, (state.threshold / scale) * 100)}%`;
  });
  sec.append(
    field("Sensitivity", slider(["sensitivity"], 0, 100, 1, (v) => `${v}`), "Higher counts softer hits. Calibration sets the starting point."),
    field("Hit strength", h("span", { class: "meter wide" }, strengthFill, thresholdMark), "Sounds above the mark can count as hits."),
  );

  sec.append(h("h2", {}, "Channels"));
  const n = state.channels;
  const table = h("div", { class: "channels" });
  table.append(h("div", { class: "ch-head" }, h("span", {}, "Input"), h("span", {}, "Level"), h("span", {}, "Weight"), h("span", {}, "High-pass"), h("span", {}, "Low-pass")));
  for (let i = 0; i < n; i++) {
    const fill = h("span", { class: "meter-fill" });
    liveUpdaters.push(() => {
      const db = state.meters[i] ?? -120;
      fill.style.width = `${Math.max(0, Math.min(100, ((db + 60) / 60) * 100))}%`;
      fill.classList.toggle("clip", db > -1);
    });
    table.append(
      h(
        "div",
        { class: "ch-row" },
        checkbox(["channels", i, "enabled"], inputName(i)),
        h("span", { class: "meter" }, fill),
        slider(["channels", i, "weight"], 0, 2, 0.05, (v) => v.toFixed(2)),
        numberInput(["channels", i, "highpass_hz"], { min: 0, max: 20000, step: 10, unit: "Hz" }),
        numberInput(["channels", i, "lowpass_hz"], { min: 0, max: 20000, step: 100, unit: "Hz" }),
      ),
    );
  }
  sec.append(table, h("p", { class: "hint" }, "Filters at 0 Hz are off. Weight 0 ignores a channel."));
  return sec;
}

// ----- Calibrate --------------------------------------------------------------

const SWEEP_STATUS: Record<SweepView["status"], string> = {
  accepted: "Hit found",
  chosen: "Hit chosen by you",
  missed: "Nothing stood out. Tap the hit to use it.",
  ambiguous: "Something else was as loud. Tap the hit to use it.",
  noise: "Noise",
};

function renderCalibrate() {
  const sec = h("section", { class: "form" });

  // Sounds: recording takes.
  sec.append(h("h2", {}, "Sounds that count"));
  sec.append(
    h(
      "p",
      { class: "lead" },
      "Press Record hits and hit once each time the cursor crosses the centre line. The loudest moment near the centre becomes a hit example, and everything else in the sweep becomes a noise example. Record each kind of sound you want counted, like bass and slap, in any order. Record noise to add sounds that should never count.",
    ),
  );
  const hitsBtn = h("button", { class: "record hits" }) as HTMLButtonElement;
  hitsBtn.addEventListener("click", () => action("record_hits"));
  const noiseBtn = h("button", { class: "record noise" }) as HTMLButtonElement;
  noiseBtn.addEventListener("click", () => action("record_noise"));
  const clearBtn = h("button", { class: "secondary" }, "Delete all examples");
  clearBtn.addEventListener("click", () => {
    if (confirm("Delete all recorded hit and noise examples?")) action("clear_calibration");
  });
  const scope = h("canvas", { class: "scope", "aria-label": "Recording sweep" }) as HTMLCanvasElement;
  const scopeHint = h("p", { class: "scope-hint" });
  const counts = h("p", { class: "counts" });
  sec.append(h("div", { class: "row-buttons" }, hitsBtn, noiseBtn, clearBtn), h("div", { class: "scope-wrap" }, scope, scopeHint), counts);

  // Scope animation: extrapolate the cursor between status polls.
  let polledAt = performance.now();
  let lastPos = 0;
  let previous: number[] | null = null;
  let lastTrace: number[] = [];
  liveUpdaters.push(() => {
    const c = state.calibration;
    const sw = c.sweep;
    hitsBtn.textContent = c.recording === "hits" ? "Stop recording" : "Record hits";
    noiseBtn.textContent = c.recording === "noise" ? "Stop recording" : "Record noise";
    hitsBtn.classList.toggle("active", c.recording === "hits");
    noiseBtn.classList.toggle("active", c.recording === "noise");
    if (sw) {
      if (sw.position < lastPos) previous = lastTrace;
      lastPos = sw.position;
      lastTrace = sw.trace;
      polledAt = performance.now();
    } else {
      previous = null;
      lastPos = 0;
      lastTrace = [];
    }
    scopeHint.textContent = !c.recording
      ? "Press Record hits or Record noise to start."
      : c.recording === "hits"
        ? "Hit once when the cursor crosses the centre line."
        : "Make the sounds that should be ignored. Every sweep becomes noise examples.";
    const status = c.trained
      ? "The detector is using your examples."
      : `The detector starts using examples once there are ${c.min_hits} hits and ${c.min_noise} noise examples.`;
    counts.textContent = `${c.hits} hit examples, ${c.noise} noise examples. ${status}${c.mismatched ? ` ${c.mismatched} examples were recorded with a different input setup and are not used.` : ""}`;
  });
  let raf = 0;
  const frame = () => {
    const c = state.calibration;
    const sw = c.sweep;
    if (sw && c.recording) {
      const pos = Math.min(1, lastPos + (performance.now() - polledAt) / 1000 / sw.length_s);
      drawScope(scope, { take: c.recording, position: pos, toleranceFrac: sw.tolerance_s / sw.length_s, trace: lastTrace, previous, points: 200, lengthS: sw.length_s });
    } else {
      drawScope(scope, { take: "noise", position: 0, toleranceFrac: 0, trace: [], previous: null, points: 200, lengthS: 2 });
    }
    raf = requestAnimationFrame(frame);
  };
  raf = requestAnimationFrame(frame);
  cleanups.push(() => cancelAnimationFrame(raf));

  // Review of recent sweeps.
  sec.append(h("h2", {}, "Recent sweeps"));
  sec.append(h("p", { class: "lead" }, "The amber line marks the hit that was saved. If it is wrong, tap the hit you meant. Delete sweeps you don't want to keep."));
  const review = h("div", { class: "sweeps" });
  sec.append(review);
  let historyVersion = -1;
  const loadHistory = async () => {
    const list: SweepView[] = await backend.rpc({ cmd: "calib_history" });
    clear(review);
    if (list.length === 0) review.append(h("p", { class: "note" }, "No sweeps recorded since the app started. Saved examples are kept."));
    for (const sw of list) {
      const canvas = h("canvas", { class: "thumb" }) as HTMLCanvasElement;
      let hover: number | null = null;
      const draw = () => drawThumb(canvas, sw, 200, hover);
      if (sw.take === "hits") {
        canvas.classList.add("pickable");
        canvas.addEventListener("pointermove", (e) => {
          hover = e.offsetX / canvas.clientWidth;
          draw();
        });
        canvas.addEventListener("pointerleave", () => {
          hover = null;
          draw();
        });
        canvas.addEventListener("click", (e) => {
          backend.rpc({ cmd: "calib_choose", id: sw.id, position: e.offsetX / canvas.clientWidth });
        });
      }
      const del = h("button", { class: "chip", "aria-label": "Delete this sweep" }, "Delete");
      del.addEventListener("click", () => backend.rpc({ cmd: "calib_delete", id: sw.id }));
      const label = sw.take === "noise" ? "Noise" : SWEEP_STATUS[sw.status];
      review.append(h("figure", { class: `sweep ${sw.status}` }, canvas, h("figcaption", {}, h("span", {}, label), del)));
      requestAnimationFrame(draw);
    }
  };
  liveUpdaters.push(() => {
    if (state.calibration.history_version !== historyVersion) {
      historyVersion = state.calibration.history_version;
      loadHistory();
    }
  });

  // Filters with a live spectrum.
  sec.append(h("h2", {}, "Filters"));
  sec.append(
    h(
      "p",
      { class: "lead" },
      "The grey area is what the input hears, the amber line what is left after the channel's filters. Drag the handles in from the sides to cut rumble or hiss the hits don't need. Drag a handle back to its edge to turn it off.",
    ),
  );
  const savedCh = Number(localStorage.getItem("filterChannel") ?? 0);
  let ch = Math.min(savedCh, Math.max(0, state.channels - 1));
  const chSelect = h("select") as HTMLSelectElement;
  for (let i = 0; i < state.channels; i++) {
    const o = h("option", { value: String(i) }, inputName(i)) as HTMLOptionElement;
    if (i === ch) o.selected = true;
    chSelect.append(o);
  }
  chSelect.addEventListener("change", () => {
    ch = Number(chSelect.value);
    localStorage.setItem("filterChannel", String(ch));
    smoothRaw = null;
    smoothFiltered = null;
  });
  const graph = h("canvas", { class: "spectrum", "aria-label": "Frequency graph with filter handles" }) as HTMLCanvasElement;
  sec.append(field("Channel", chSelect), graph);

  let view: SpectrumView | null = null;
  let smoothRaw: number[] | null = null;
  let smoothFiltered: number[] | null = null;
  let dragging: "hp" | "lp" | null = null;
  let dragHz = 0;
  const filterOf = (which: "hp" | "lp") => persist.settings.channels[ch]?.[which === "hp" ? "highpass_hz" : "lowpass_hz"] ?? 0;
  const redraw = () => {
    if (!view) return;
    view.highpass = dragging === "hp" ? dragHz : filterOf("hp");
    view.lowpass = dragging === "lp" ? dragHz : filterOf("lp");
    drawSpectrum(graph, view, dragging);
  };
  const smooth = (prev: number[] | null, next: number[]) => (prev && prev.length === next.length ? prev.map((p, i) => p + (next[i] - p) * 0.35) : next);
  const pollSpectrum = async () => {
    const r = await backend.rpc({ cmd: "spectrum", channel: ch });
    if (r.error) return;
    smoothRaw = smooth(smoothRaw, r.raw);
    smoothFiltered = smooth(smoothFiltered, r.filtered);
    view = { minHz: r.min_hz, maxHz: r.max_hz, raw: smoothRaw!, filtered: smoothFiltered!, highpass: 0, lowpass: 0 };
    redraw();
  };
  const timer = window.setInterval(pollSpectrum, 80);
  cleanups.push(() => clearInterval(timer));
  refreshers.push(redraw);

  // Dragging the filter edges.
  let lastSent = 0;
  const send = (final: boolean) => {
    if (!dragging) return;
    const now = performance.now();
    if (!final && now - lastSent < 120) return;
    lastSent = now;
    setPath(["channels", ch, dragging === "hp" ? "highpass_hz" : "lowpass_hz"], Math.round(dragHz));
  };
  graph.addEventListener("pointerdown", (e) => {
    if (!view) return;
    const w = graph.clientWidth;
    const hpX = filterOf("hp") > 0 ? freqToX(filterOf("hp"), view, w) : 0;
    const lpX = filterOf("lp") > 0 ? freqToX(filterOf("lp"), view, w) : w;
    dragging = Math.abs(e.offsetX - hpX) <= Math.abs(e.offsetX - lpX) ? "hp" : "lp";
    graph.setPointerCapture(e.pointerId);
    graph.dispatchEvent(new PointerEvent("pointermove", e));
  });
  graph.addEventListener("pointermove", (e) => {
    if (!dragging || !view) return;
    const w = graph.clientWidth;
    const x = Math.max(0, Math.min(w, e.offsetX));
    // Within a few pixels of its own edge, a handle turns the filter off.
    const off = dragging === "hp" ? x < 6 : x > w - 6;
    dragHz = off ? 0 : xToFreq(x, view, w);
    redraw();
    send(false);
  });
  const end = () => {
    send(true);
    dragging = null;
    redraw();
  };
  graph.addEventListener("pointerup", end);
  graph.addEventListener("pointercancel", end);

  // Latency test.
  sec.append(h("h2", {}, "Latency"));
  sec.append(
    h(
      "p",
      { class: "lead" },
      "Sends a few notes to a sound source and listens for them on one input channel, to measure the delay from MIDI out to audio in. Route the sound into that input.",
    ),
  );
  const listenOptions = (): [number | null, string][] => [[null, "Off"], ...Array.from({ length: state.channels }, (_, i): [number, string] => [i, inputName(i)])];
  const runBtn = h("button", { class: "secondary" }, "Run latency test");
  runBtn.addEventListener("click", () => action("latency_test"));
  const result = h("p", { class: "counts" });
  liveUpdaters.push(() => {
    result.textContent = `Measured latency ${state.measured_latency_ms.toFixed(1)} ms. Offset ${state.offset_ms.toFixed(1)} ms.`;
  });
  sec.append(
    field("MIDI output", select(["latency_test", "output", "port"], portOptions)),
    field("Channel", select(["latency_test", "output", "channel"], channelOptions, Number)),
    field("Note", numberInput(["latency_test", "output", "note"], { min: 0, max: 127 })),
    field("Listening channel", select(["latency_test", "listen_channel"], listenOptions, (v) => (v === "" ? null : Number(v)))),
    field("", checkbox(["latency_test", "run_on_startup"], "Run when the app starts")),
    h("div", { class: "row-buttons" }, runBtn),
    result,
  );
  return sec;
}

// ----- Output -----------------------------------------------------------------

function renderOutput() {
  const sec = h("section", { class: "form" });
  sec.append(h("h2", {}, "MIDI clock"));
  const list = h("div", { class: "port-list" });
  const ports = [...new Set([...host.midi_outputs, ...persist.settings.clock_outputs])];
  if (ports.length === 0) list.append(h("p", { class: "note" }, "No MIDI outputs found. Connect a device or start a virtual MIDI port."));
  for (const p of ports) {
    const input = h("input", { type: "checkbox" }) as HTMLInputElement;
    const refresh = () => (input.checked = persist.settings.clock_outputs.includes(p));
    input.addEventListener("change", () => {
      const cur = persist.settings.clock_outputs.filter((x) => x !== p);
      if (input.checked) cur.push(p);
      patch({ clock_outputs: cur });
    });
    refreshers.push(refresh);
    refresh();
    const missing = !host.midi_outputs.includes(p);
    list.append(h("label", { class: "check" }, input, h("span", {}, missing ? `${p} (not connected)` : p)));
  }
  sec.append(list, h("p", { class: "hint" }, "The clock goes to every ticked port, with Start and Stop."));

  sec.append(h("h2", {}, "Click"));
  sec.append(
    h("p", { class: "lead" }, "A note on every beat, timed with the offset. Use it to tune the offset by ear."),
    field("", checkbox(["click", "enabled"], "Send click")),
    field("MIDI output", select(["click", "output", "port"], portOptions)),
    field("Channel", select(["click", "output", "channel"], channelOptions, Number)),
    field("Note", numberInput(["click", "output", "note"], { min: 0, max: 127 })),
    field("Velocity", numberInput(["click", "output", "velocity"], { min: 1, max: 127 })),
    field("Velocity on beat 1", numberInput(["click", "accent_velocity"], { min: 1, max: 127 })),
  );
  return sec;
}

// ----- Controls ---------------------------------------------------------------

function renderControls() {
  const sec = h("section", { class: "form" });
  sec.append(h("h2", {}, "Controls"));
  sec.append(h("p", { class: "lead" }, "Bind any MIDI note or controller, gamepad button or key to an action. Press Learn, then the control."));
  const list = h("div", { class: "bindings" });
  for (const a of actions) {
    const chips = h("div", { class: "chips" });
    for (const b of persist.settings.bindings.filter((b) => b.action === a.id)) {
      const x = h("button", { class: "chip", "aria-label": `Remove ${describeSource(b.source)}` }, describeSource(b.source), h("span", { class: "x" }, "×"));
      x.addEventListener("click", () => backend.rpc({ cmd: "unbind", action: a.id, source: b.source }).then(reloadPersist));
      chips.append(x);
    }
    const learn = h("button", { class: "secondary learn" }, "Learn") as HTMLButtonElement;
    learn.addEventListener("click", () => backend.rpc(state.learning === a.id ? { cmd: "learn_cancel" } : { cmd: "learn", action: a.id }));
    liveUpdaters.push(() => {
      const on = state.learning === a.id;
      learn.textContent = on ? "Press a control… (cancel)" : "Learn";
      learn.classList.toggle("active", on);
    });
    list.append(h("div", { class: "binding-row" }, h("span", { class: "action-label" }, a.label), chips, learn));
  }
  sec.append(list, h("p", { class: "hint" }, "Keys only work while this window is focused. MIDI controls and gamepads work in the background."));
  return sec;
}

// ----- Advanced ---------------------------------------------------------------

function renderAdvanced() {
  const sec = h("section", { class: "form" });
  sec.append(h("h2", {}, "Following"));
  sec.append(
    field("Half-life", numberInput(["half_life_s"], { min: 0.5, max: 60, step: 0.5, unit: "s" }), "How long what you played keeps counting. Shorter follows changes faster; longer is steadier."),
    field("Tempo drift", numberInput(["tempo_drift_pct"], { min: 0, max: 50, step: 0.5, unit: "%/s" }), "How fast the tempo may change."),
    field("Lowest tempo", numberInput(["tempo_min_bpm"], { min: 20, max: 400, unit: "BPM" })),
    field("Highest tempo", numberInput(["tempo_max_bpm"], { min: 20, max: 400, unit: "BPM" })),
    field("Beats per bar", numberInput(["beats_per_bar"], { min: 1, max: 32 })),
  );
  sec.append(h("h2", {}, "Starting and stopping"));
  sec.append(
    field("Count-in hits", numberInput(["count_in_hits"], { min: 2, max: 32 }), "Even hits that set the tempo. The clock starts on the next beat."),
    field("", checkbox(["auto_stop"], "Stop after silence")),
    field("Silence before stop", numberInput(["auto_stop_after_s"], { min: 0.5, max: 120, step: 0.5, unit: "s" })),
  );
  sec.append(h("h2", {}, "Timing"));
  sec.append(
    field("Offset trim", numberInput(["offset_trim_ms"], { min: -500, max: 500, step: 0.5, unit: "ms" }), "Added to the measured latency. The + and − actions change this."),
    field("Offset step", numberInput(["offset_step_ms"], { min: 0.1, max: 50, step: 0.5, unit: "ms" })),
    field("Lookahead", numberInput(["lookahead_ms"], { min: 5, max: 500, step: 5, unit: "ms" }), "How early MIDI events are queued. Raise it if the clock stutters."),
  );
  return sec;
}

// ----- input from the keyboard ---------------------------------------------------

window.addEventListener("keydown", (e) => {
  const t = e.target as HTMLElement;
  if (t.closest("input, select, textarea") || e.repeat || e.metaKey || e.ctrlKey || e.altKey) return;
  if (state.learning && e.code === "Escape") {
    backend.rpc({ cmd: "learn_cancel" });
    return;
  }
  const source = `key:${e.code}`;
  if (state.learning || persist.settings.bindings.some((b) => b.source === source)) {
    e.preventDefault();
    // Keep a bound key (e.g. Space) from also pressing the focused button.
    (document.activeElement as HTMLElement | null)?.blur();
    backend.rpc({ cmd: "control", source });
  }
});

window.addEventListener("keyup", (e) => {
  if (persist.settings.bindings.some((b) => b.source === `key:${e.code}`)) e.preventDefault();
});

// ----- polling --------------------------------------------------------------------

let lastMessage = "";
async function poll() {
  try {
    state = await backend.rpc({ cmd: "state" });
    if (state.version !== lastVersion) {
      lastVersion = state.version;
      await reloadPersist();
    }
    if (state.message && state.message !== lastMessage) {
      toast(state.message);
      backend.rpc({ cmd: "clear_message" });
    }
    lastMessage = state.message;
    liveUpdaters.forEach((f) => f());
  } catch (err) {
    console.error(err);
  }
}
let lastVersion = state.version;

async function pollHost() {
  try {
    host = await backend.hostStatus();
    devices = await backend.listAudioDevices();
    renderHostStatus();
    if (computeStructureKey() !== structureKey) renderScreen();
  } catch (err) {
    console.error(err);
  }
}

async function startAudio() {
  await backend.start();
  await pollHost();
  // Measure latency once audio is up, if the test is set up.
  const lt = persist.settings.latency_test;
  if (lt.run_on_startup && lt.listen_channel !== null && lt.output.port) {
    for (let i = 0; i < 40 && !host.audio.running; i++) {
      await new Promise((r) => setTimeout(r, 250));
      host = await backend.hostStatus();
    }
    if (host.audio.running && host.midi_outputs.includes(lt.output.port)) await action("latency_test");
  }
}

renderHostStatus();
renderScreen();
setInterval(poll, 50);
setInterval(pollHost, 1500);

if (backend.needsGesture) {
  const overlay = h(
    "div",
    { class: "gate" },
    h("div", { class: "gate-card" }, h("h2", {}, "Audio to MIDI Clock"), h("p", {}, "Turns live percussion into MIDI clock that follows the player. The page needs your microphone or audio interface and MIDI access."), h("button", { class: "start" }, "Turn on audio and MIDI")),
  );
  overlay.querySelector("button")!.addEventListener("click", async () => {
    overlay.remove();
    try {
      await startAudio();
    } catch (err) {
      toast(String(err));
    }
  });
  document.body.append(overlay);
} else {
  startAudio().catch((err) => toast(String(err)));
}
