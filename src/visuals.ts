// ---------------------------------------------------------------------------
// visuals.ts — dot-matrix scroller + segmented LED spectrum analyser.
//
// The Rust engine computes 20 log-spaced spectrum magnitudes (0..1) from the
// post-EQ signal and pushes them over IPC. This module keeps the *visual*
// smoothing (attack/decay + peak caps) and the LED rendering, so the display
// looks identical to the reference design regardless of IPC frame rate.
// ---------------------------------------------------------------------------

const BARS = 20;
const SEGS = 22;

const peaks = new Float32Array(BARS);
const levels = new Float32Array(BARS);
const targets = new Float32Array(BARS); // latest bars from the backend (0..1)

let playing = false;
let tuning = false;

// ---- canvases --------------------------------------------------------------

const spec = document.getElementById("spectrum") as HTMLCanvasElement;
const sctx = spec.getContext("2d")!;
const scr = document.getElementById("scroller") as HTMLCanvasElement;
const rctx = scr.getContext("2d")!;

let specDims = { w: 196, h: 78 };
let scrDims = { w: 300, h: 46 };

function fitCanvas(cv: HTMLCanvasElement, cssW: number) {
  const dpr = window.devicePixelRatio || 1;
  const w = cssW || cv.clientWidth;
  const h = parseInt(cv.getAttribute("height")!, 10);
  cv.width = Math.round(w * dpr);
  cv.height = Math.round(h * dpr);
  cv.style.height = h + "px";
  const c = cv.getContext("2d")!;
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  return { w, h };
}

export function resize() {
  const specParentW = spec.parentElement!.clientWidth - 22;
  specDims = fitCanvas(spec, specParentW > 400 ? 196 : Math.min(196, specParentW));
  spec.style.width = specDims.w + "px";
  scrDims = fitCanvas(scr, scr.parentElement!.clientWidth - 22);
}

// ---- spectrum --------------------------------------------------------------

export function setSpectrum(bars: number[] | Float32Array) {
  const n = Math.min(BARS, bars.length);
  for (let i = 0; i < n; i++) targets[i] = bars[i];
}

export function setPlaying(v: boolean) {
  playing = v;
  if (!v) targets.fill(0);
}
export function setTuning(v: boolean) {
  tuning = v;
}

export function clearSpectrum() {
  targets.fill(0);
  levels.fill(0);
  peaks.fill(0);
}

function drawSpectrum() {
  const { w, h } = specDims;
  sctx.clearRect(0, 0, w, h);

  const gapX = 2;
  const barW = (w - gapX * (BARS - 1)) / BARS;
  const segH = h / SEGS;
  const segFill = Math.max(1, segH - 1.4);

  for (let i = 0; i < BARS; i++) {
    // slight upward tilt so the top end isn't permanently dark
    const tilt = 1 + (i / BARS) * 0.35;
    const v = Math.min(1, targets[i] * tilt);

    // attack fast, decay slow — classic analyser ballistics
    levels[i] += (v - levels[i]) * (v > levels[i] ? 0.55 : 0.16);
    peaks[i] = Math.max(peaks[i] - 0.011, levels[i]);

    const x = i * (barW + gapX);
    const lit = Math.round(levels[i] * SEGS);

    for (let s = 0; s < SEGS; s++) {
      const y = h - (s + 1) * segH;
      const on = s < lit;
      const hot = s >= SEGS - 3;
      if (on) sctx.fillStyle = hot ? "#ff3f1c" : (s >= SEGS - 7 ? "#ffc247" : "#ff9b21");
      else sctx.fillStyle = hot ? "#2a0d06" : "#22160a";
      sctx.fillRect(x, y, barW, segFill);
    }

    // peak cap
    const ps = Math.min(SEGS - 1, Math.round(peaks[i] * SEGS));
    if (ps > 0) {
      sctx.fillStyle = "#fff0d2";
      sctx.fillRect(x, h - (ps + 1) * segH, barW, Math.max(1, segFill * 0.5));
    }
  }
}

// ---- dot-matrix scroller ---------------------------------------------------

const ROWS = 7;
let matrix: Uint8Array | null = null;
let matrixW = 0;
let scrollX = 0;
let scrollText = "";

const off = document.createElement("canvas");
const octx = off.getContext("2d", { willReadFrequently: true })!;

function rasterise(text: string) {
  const t = "   " + text + "   ";
  octx.font = `${ROWS}px ui-monospace, "Courier New", monospace`;
  const w = Math.max(1, Math.ceil(octx.measureText(t).width));
  off.width = w;
  off.height = ROWS;
  const c = off.getContext("2d", { willReadFrequently: true })!;
  c.clearRect(0, 0, w, ROWS);
  c.font = `${ROWS}px ui-monospace, "Courier New", monospace`;
  c.textBaseline = "top";
  c.fillStyle = "#fff";
  c.fillText(t, 0, 0);
  const px = c.getImageData(0, 0, w, ROWS).data;
  matrixW = w;
  matrix = new Uint8Array(w * ROWS);
  for (let y = 0; y < ROWS; y++)
    for (let x = 0; x < w; x++)
      matrix[y * w + x] = px[(y * w + x) * 4 + 3] > 110 ? 1 : 0;
}

export function setScroll(text: string) {
  if (text === scrollText) return;
  scrollText = text;
  rasterise(text);
  scrollX = 0;
}

function drawScroller(dt: number) {
  const { w, h } = scrDims;
  rctx.clearRect(0, 0, w, h);
  if (!matrix) return;

  const dot = 3, gap = 1, pitch = dot + gap;
  const cols = Math.floor(w / pitch);
  const yTop = Math.round((h - (ROWS * pitch - gap)) / 2);

  for (let cx = 0; cx < cols; cx++) {
    const mx = (Math.floor(scrollX) + cx) % matrixW;
    for (let y = 0; y < ROWS; y++) {
      const on = matrix[y * matrixW + ((mx + matrixW) % matrixW)];
      rctx.fillStyle = on ? "#ff9b21" : "#241705";
      rctx.fillRect(cx * pitch, yTop + y * pitch, dot, dot);
    }
  }
  if (playing || tuning) scrollX += dt * 0.022;
}

// ---- draw loop -------------------------------------------------------------

let raf = 0;
let last = 0;
const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

function frame(t: number) {
  const dt = last ? Math.min(64, t - last) : 16;
  last = t;
  drawSpectrum();
  drawScroller(reduce ? 0 : dt);
  raf = requestAnimationFrame(frame);
}

export function initVisuals() {
  resize();
  window.addEventListener("resize", resize);
  if (!raf) raf = requestAnimationFrame(frame);
}
