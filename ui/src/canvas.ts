// Canvas drawing for the calibration screen: the sweep scope, review
// thumbnails and the filter graph.

export const colors = {
  sunken: "#121931",
  line: "#34406a",
  text: "#e8ecf8",
  muted: "#8d97b8",
  led: "#ffb23e",
  ledSoft: "rgba(255, 178, 62, 0.14)",
  ok: "#5fd3c4",
  bad: "#ec6c84",
};

const FONT = '12px "Archivo Variable", system-ui, sans-serif';

/** Size the canvas backing store to its CSS size and return a 2D context in CSS pixels. */
export function fit(canvas: HTMLCanvasElement): { ctx: CanvasRenderingContext2D; w: number; h: number } {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }
  const ctx = canvas.getContext("2d")!;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  return { ctx, w, h };
}

/** Level in dB to a y position: -80 dB at the bottom, 0 dB at the top. */
function levelY(db: number, h: number, pad = 6) {
  const v = Math.max(0, Math.min(1, (db + 80) / 80));
  return h - pad - v * (h - 2 * pad);
}

function drawTrace(ctx: CanvasRenderingContext2D, trace: number[], points: number, w: number, h: number, from = 0, to = trace.length) {
  if (to - from < 2) return;
  ctx.beginPath();
  for (let i = from; i < to; i++) {
    const x = (i / points) * w;
    const y = levelY(trace[i], h);
    if (i === from) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

export interface ScopeState {
  take: "hits" | "noise";
  /** Cursor position in the sweep, 0..1. */
  position: number;
  toleranceFrac: number;
  trace: number[];
  previous: number[] | null;
  points: number;
  lengthS: number;
}

/** The sweep: a scrolling level trace with a cursor and, for hits, the centre to aim for. */
export function drawScope(canvas: HTMLCanvasElement, s: ScopeState) {
  const { ctx, w, h } = fit(canvas);
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = colors.sunken;
  ctx.fillRect(0, 0, w, h);
  const cx = w / 2;
  const cursor = s.position * w;
  if (s.take === "hits") {
    const zone = s.toleranceFrac * w;
    ctx.fillStyle = colors.ledSoft;
    ctx.fillRect(cx - zone, 0, 2 * zone, h);
  }
  // Last sweep's trace, still visible ahead of the cursor like a scope's persistence.
  if (s.previous) {
    ctx.strokeStyle = "rgba(141, 151, 184, 0.35)";
    ctx.lineWidth = 1.5;
    const start = Math.min(s.points, Math.ceil(s.position * s.points) + 3);
    drawTrace(ctx, s.previous, s.points, w, h, start, s.previous.length);
  }
  ctx.strokeStyle = colors.text;
  ctx.lineWidth = 2;
  drawTrace(ctx, s.trace, s.points, w, h);
  if (s.take === "hits") {
    // The centre line lights up while the cursor passes it.
    const dist = Math.abs(cursor - cx) / w;
    const glow = Math.max(0, 1 - dist / 0.04);
    ctx.strokeStyle = glow > 0 ? colors.led : "rgba(255, 178, 62, 0.55)";
    ctx.lineWidth = 2 + glow * 4;
    ctx.shadowColor = colors.led;
    ctx.shadowBlur = glow * 24;
    ctx.beginPath();
    ctx.moveTo(cx, 0);
    ctx.lineTo(cx, h);
    ctx.stroke();
    ctx.shadowBlur = 0;
    if (glow > 0) {
      ctx.fillStyle = colors.led;
      ctx.beginPath();
      ctx.arc(cx, 14, 6 + glow * 5, 0, Math.PI * 2);
      ctx.fill();
    }
  }
  ctx.strokeStyle = colors.ok;
  ctx.lineWidth = 2;
  ctx.beginPath();
  ctx.moveTo(cursor, 0);
  ctx.lineTo(cursor, h);
  ctx.stroke();
}

export interface SweepView {
  id: number;
  take: "hits" | "noise";
  status: "accepted" | "chosen" | "missed" | "ambiguous" | "noise";
  winner: number | null;
  trace: number[];
}

/** A small review picture of one sweep, with the chosen hit marked. */
export function drawThumb(canvas: HTMLCanvasElement, s: SweepView, points: number, hover: number | null) {
  const { ctx, w, h } = fit(canvas);
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = colors.sunken;
  ctx.fillRect(0, 0, w, h);
  const saved = s.status === "accepted" || s.status === "chosen";
  ctx.strokeStyle = s.take === "noise" ? colors.bad : saved ? colors.text : colors.muted;
  ctx.globalAlpha = saved || s.take === "noise" ? 1 : 0.6;
  ctx.lineWidth = 1.25;
  drawTrace(ctx, s.trace, points, w, h);
  ctx.globalAlpha = 1;
  if (s.winner !== null) {
    const x = s.winner * w;
    ctx.strokeStyle = saved ? colors.led : colors.muted;
    ctx.setLineDash(saved ? [] : [3, 3]);
    ctx.lineWidth = 2;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, h);
    ctx.stroke();
    ctx.setLineDash([]);
  }
  if (hover !== null && s.take === "hits") {
    ctx.strokeStyle = colors.ok;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(hover * w, 0);
    ctx.lineTo(hover * w, h);
    ctx.stroke();
  }
}

export interface SpectrumView {
  minHz: number;
  maxHz: number;
  raw: number[];
  filtered: number[];
  highpass: number;
  lowpass: number;
}

/** Shown dB range; the top follows the loudest part of the raw spectrum. */
const DB_RANGE = 72;

export function freqToX(f: number, v: SpectrumView, w: number) {
  return (Math.log(f / v.minHz) / Math.log(v.maxHz / v.minHz)) * w;
}

export function xToFreq(x: number, v: SpectrumView, w: number) {
  return v.minHz * Math.pow(v.maxHz / v.minHz, Math.max(0, Math.min(1, x / w)));
}

export function formatHz(f: number) {
  return f >= 1000 ? `${(f / 1000).toFixed(f >= 10000 ? 0 : 1)} kHz` : `${Math.round(f)} Hz`;
}

/** Live spectrum before and after the channel's filters, with draggable filter edges. */
export function drawSpectrum(canvas: HTMLCanvasElement, v: SpectrumView, active: "hp" | "lp" | null) {
  const { ctx, w, h } = fit(canvas);
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = colors.sunken;
  ctx.fillRect(0, 0, w, h);
  const top = 22;
  const bottom = h - 20;
  const loudest = v.raw.reduce((m, d) => Math.max(m, d), -120);
  const dbTop = Math.min(0, Math.ceil((loudest + 6) / 6) * 6);
  const dbBottom = dbTop - DB_RANGE;
  const y = (db: number) => top + ((dbTop - Math.max(dbBottom, Math.min(dbTop, db))) / DB_RANGE) * (bottom - top);

  // Frequency grid.
  ctx.font = FONT;
  ctx.fillStyle = colors.muted;
  ctx.strokeStyle = "rgba(52, 64, 106, 0.6)";
  ctx.lineWidth = 1;
  for (const f of [50, 100, 200, 500, 1000, 2000, 5000, 10000]) {
    if (f < v.minHz || f > v.maxHz) continue;
    const x = freqToX(f, v, w);
    ctx.beginPath();
    ctx.moveTo(x, top);
    ctx.lineTo(x, bottom);
    ctx.stroke();
    ctx.fillText(formatHz(f).replace(" ", ""), x + 3, h - 6);
  }

  const n = v.raw.length;
  const bx = (i: number) => ((i + 0.5) / n) * w;
  // Raw input as a filled area.
  ctx.fillStyle = "rgba(141, 151, 184, 0.28)";
  ctx.beginPath();
  ctx.moveTo(0, bottom);
  v.raw.forEach((db, i) => ctx.lineTo(bx(i), y(db)));
  ctx.lineTo(w, bottom);
  ctx.closePath();
  ctx.fill();
  // After the filters.
  ctx.strokeStyle = colors.led;
  ctx.lineWidth = 2;
  ctx.beginPath();
  v.filtered.forEach((db, i) => (i === 0 ? ctx.moveTo(bx(i), y(db)) : ctx.lineTo(bx(i), y(db))));
  ctx.stroke();

  // Cut regions and handles.
  const hpX = v.highpass > 0 ? freqToX(v.highpass, v, w) : 0;
  const lpX = v.lowpass > 0 ? freqToX(v.lowpass, v, w) : w;
  ctx.fillStyle = "rgba(18, 25, 49, 0.55)";
  ctx.fillRect(0, top, hpX, bottom - top);
  ctx.fillRect(lpX, top, w - lpX, bottom - top);
  const handle = (x: number, label: string, on: boolean, isActive: boolean, alignRight: boolean) => {
    ctx.strokeStyle = on || isActive ? colors.ok : colors.muted;
    ctx.lineWidth = isActive ? 3 : 2;
    ctx.beginPath();
    ctx.moveTo(x, top - 4);
    ctx.lineTo(x, bottom);
    ctx.stroke();
    ctx.fillStyle = on || isActive ? colors.ok : colors.muted;
    ctx.beginPath();
    ctx.arc(x, top - 4, isActive ? 8 : 7, 0, Math.PI * 2);
    ctx.fill();
    ctx.font = FONT;
    ctx.fillStyle = colors.text;
    const tw = ctx.measureText(label).width;
    const tx = alignRight ? Math.max(4, x - tw - 12) : Math.min(w - tw - 4, x + 12);
    ctx.fillText(label, tx, top);
  };
  handle(Math.max(hpX, 2), v.highpass > 0 ? `High-pass ${formatHz(v.highpass)}` : "High-pass off", v.highpass > 0, active === "hp", false);
  handle(Math.min(lpX, w - 2), v.lowpass > 0 ? `Low-pass ${formatHz(v.lowpass)}` : "Low-pass off", v.lowpass > 0, active === "lp", true);
}
