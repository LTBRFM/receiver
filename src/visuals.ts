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
let scrDims = { w: 300, h: 78 };

function fitCanvas(cv: HTMLCanvasElement, cssW: number) {
  const dpr = window.devicePixelRatio || 1;
  const w = cssW || cv.clientWidth;
  // Latch the design height from the markup ONCE. Assigning cv.height below
  // rewrites the height attribute, so re-reading it on later resizes would
  // multiply by dpr each time (46 -> 92 -> 184 ... on retina displays),
  // stretching the canvases — and the window — exponentially.
  if (!cv.dataset.designH) cv.dataset.designH = cv.getAttribute("height")!;
  const h = parseInt(cv.dataset.designH, 10);
  cv.width = Math.round(w * dpr);
  cv.height = Math.round(h * dpr);
  cv.style.height = h + "px";
  const c = cv.getContext("2d")!;
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  return { w, h };
}

export function resize() {
  // A hidden face reports zero/negative widths — sizing canvases from those
  // would allocate absurd backing stores. Skip; we re-run on face switch.
  if (spec.offsetParent === null) return;
  const specParentW = spec.parentElement!.clientWidth - 22;
  if (specParentW <= 0) return;
  specDims = fitCanvas(spec, specParentW > 400 ? 196 : Math.min(196, specParentW));
  spec.style.width = specDims.w + "px";
  scrDims = fitCanvas(scr, scr.parentElement!.clientWidth - 22);
  buildGrid();
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
    // Attenuate the drive so the columns live mid-scale and only peaks reach
    // the red — full-scale bars most of the time read as clipping.
    const v = Math.min(1, targets[i] * tilt * 0.7);

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
//
// The glyphs are a built-in 5x7 bitmap font (the classic LED-matrix charset),
// NOT rasterised system text. Rendering OS fonts at 7px and sampling the
// pixels back produced a different (often broken) dot pattern on every
// platform — font choice, hinting and antialiasing all differ between
// WebKitGTK, CoreText and DirectWrite. A bitmap font is deterministic:
// identical dots on every OS, like a real dot-matrix display.

const ROWS = 7;
const GLYPH_W = 5;
const GLYPH_STEP = 6; // 5 columns + 1 blank spacing column

// Each glyph: 7 rows, 5 bits per row, bit 4 = leftmost column.
const FONT: Record<string, number[]> = {
  " ": [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
  "A": [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
  "B": [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e],
  "C": [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e],
  "D": [0x1e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1e],
  "E": [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f],
  "F": [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10],
  "G": [0x0e, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0e],
  "H": [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
  "I": [0x0e, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e],
  "J": [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0c],
  "K": [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
  "L": [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f],
  "M": [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11],
  "N": [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
  "O": [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
  "P": [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10],
  "Q": [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d],
  "R": [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11],
  "S": [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e],
  "T": [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
  "U": [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
  "V": [0x11, 0x11, 0x11, 0x11, 0x11, 0x0a, 0x04],
  "W": [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0a],
  "X": [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11],
  "Y": [0x11, 0x11, 0x11, 0x0a, 0x04, 0x04, 0x04],
  "Z": [0x1f, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1f],
  "0": [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
  "1": [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
  "2": [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
  "3": [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e],
  "4": [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
  "5": [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e],
  "6": [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e],
  "7": [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
  "8": [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
  "9": [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c],
  ".": [0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x06],
  ",": [0x00, 0x00, 0x00, 0x00, 0x0c, 0x04, 0x08],
  "·": [0x00, 0x00, 0x00, 0x0c, 0x0c, 0x00, 0x00],
  "-": [0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00],
  "_": [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1f],
  ":": [0x00, 0x06, 0x06, 0x00, 0x06, 0x06, 0x00],
  ";": [0x00, 0x06, 0x06, 0x00, 0x06, 0x04, 0x08],
  "/": [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10],
  "'": [0x04, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00],
  "\"": [0x0a, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00],
  "!": [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
  "?": [0x0e, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04],
  "&": [0x08, 0x14, 0x14, 0x08, 0x15, 0x12, 0x0d],
  "(": [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
  ")": [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
  "[": [0x0e, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0e],
  "]": [0x0e, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0e],
  "+": [0x00, 0x04, 0x04, 0x1f, 0x04, 0x04, 0x00],
  "=": [0x00, 0x00, 0x1f, 0x00, 0x1f, 0x00, 0x00],
  "*": [0x00, 0x0a, 0x04, 0x1f, 0x04, 0x0a, 0x00],
  "#": [0x0a, 0x0a, 0x1f, 0x0a, 0x1f, 0x0a, 0x0a],
  "%": [0x19, 0x1a, 0x02, 0x04, 0x08, 0x0b, 0x13],
  "@": [0x0e, 0x11, 0x17, 0x15, 0x17, 0x10, 0x0e],
};

// Typographic characters that ICY titles often carry, mapped to plain glyphs.
const ALIASES: Record<string, string> = {
  "’": "'", "‘": "'", "“": "\"", "”": "\"",
  "–": "-", "—": "-", "•": "·", "…": ".",
};

function rasterise(text: string): { bits: Uint8Array; w: number } {
  const t = "   " + text + "   ";
  // Uppercase and strip diacritics so e.g. "Édith" renders as "EDITH";
  // anything still unknown becomes a blank cell.
  const chars = Array.from(
    t.toUpperCase().normalize("NFD").replace(/[\u0300-\u036f]/g, ""),
  ).map((ch) => ALIASES[ch] ?? ch);

  const w = chars.length * GLYPH_STEP;
  const bits = new Uint8Array(w * ROWS);
  chars.forEach((ch, i) => {
    const glyph = FONT[ch] ?? FONT[" "];
    for (let y = 0; y < ROWS; y++) {
      const row = glyph[y];
      for (let x = 0; x < GLYPH_W; x++) {
        if (row & (1 << (GLYPH_W - 1 - x))) bits[y * w + i * GLYPH_STEP + x] = 1;
      }
    }
  });
  return { bits, w };
}

// The panel is two independent 7-row bands. Each keeps its own bitmap and its
// own scroll offset, but both advance at the same rate — differing speeds make
// the two lines visibly beat against one another instead of reading as one
// display.
interface Line {
  bits: Uint8Array | null;
  w: number;
  x: number;
  text: string;
}

const LINES = 2;
const LINE_GAP = 6; // unlit rows between the two bands
const DOT = 3, DOT_GAP = 1, PITCH = DOT + DOT_GAP;
const BAND_H = ROWS * PITCH - DOT_GAP;
const LIT = "#ff9b21";
const UNLIT = "#241705";

const lines: Line[] = Array.from({ length: LINES }, () => ({
  bits: null,
  w: 0,
  x: 0,
  text: "",
}));

/** Vertical origin of band `i`, both bands centred as a group. */
function bandTop(i: number, h: number): number {
  const total = BAND_H * LINES + LINE_GAP * (LINES - 1);
  return Math.round((h - total) / 2) + i * (BAND_H + LINE_GAP);
}

export function setLine(i: number, text: string) {
  const line = lines[i];
  if (!line || text === line.text) return; // same text never restarts the scroll
  line.text = text;
  const { bits, w } = rasterise(text);
  line.bits = bits;
  line.w = w;
  line.x = 0;
}

/** Back-compat alias: the top line is the primary one. */
export function setScroll(text: string) {
  setLine(0, text);
}

// The unlit grid never changes between resizes, so it is pre-rendered once to
// an offscreen canvas and blitted per frame — the same trick dial.ts and vu.ts
// use for their printed layers. Painting every cell inline was already ~770
// fillRects a frame with one line; two lines would have doubled it.
let grid: HTMLCanvasElement | null = null;

function buildGrid() {
  const { w, h } = scrDims;
  if (w <= 0 || h <= 0) return;
  const dpr = window.devicePixelRatio || 1;
  const cv = document.createElement("canvas");
  cv.width = Math.round(w * dpr);
  cv.height = Math.round(h * dpr);
  const c = cv.getContext("2d")!;
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.fillStyle = UNLIT;
  const cols = Math.floor(w / PITCH);
  for (let i = 0; i < LINES; i++) {
    const yTop = bandTop(i, h);
    for (let cx = 0; cx < cols; cx++) {
      for (let y = 0; y < ROWS; y++) {
        c.fillRect(cx * PITCH, yTop + y * PITCH, DOT, DOT);
      }
    }
  }
  grid = cv;
}

function drawScroller(dt: number) {
  const { w, h } = scrDims;
  rctx.clearRect(0, 0, w, h);
  if (grid) rctx.drawImage(grid, 0, 0, w, h);

  const cols = Math.floor(w / PITCH);
  rctx.fillStyle = LIT;

  for (let i = 0; i < LINES; i++) {
    const line = lines[i];
    if (!line.bits || line.w <= 0) continue;
    const yTop = bandTop(i, h);
    const off = Math.floor(line.x);
    for (let cx = 0; cx < cols; cx++) {
      const mx = (((off + cx) % line.w) + line.w) % line.w;
      for (let y = 0; y < ROWS; y++) {
        if (line.bits[y * line.w + mx]) {
          rctx.fillRect(cx * PITCH, yTop + y * PITCH, DOT, DOT);
        }
      }
    }
    if (playing || tuning) line.x += dt * 0.022;
  }
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
