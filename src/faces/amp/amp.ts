// ---------------------------------------------------------------------------
// amp.ts — the "Amp" face: a late-90s desktop-player skin.
//
// The blue LCD is the point of this face and is painted on a canvas so it
// has real pixels: a chunky time readout, KBPS / KHZ readouts, a STEREO
// badge, a little analyser, and the title scrolling underneath — all set in
// the same 5x7 dot font the rack face uses, drawn as solid cells. The words
// come from display.ts; this file only decides how they look.
//
// Functionality maps onto the skin's furniture: ⏮ ⏭ step through EQ presets
// (it is radio — there are no tracks to skip), the eject-shaped key mutes,
// the position bar shows where the current track is on the station timeline,
// the side keys are NO DJ and always-on-top, the EQ panel's ON key bypasses
// the equaliser, and the title-bar "shade" key folds the player into a bar.
// ---------------------------------------------------------------------------

import * as player from "../../player.ts";
import { cmd } from "../../player.ts";
import { onFaceChange } from "../../faces.ts";
import { makeFader } from "../../fader.ts";
import { rasterise, ROWS } from "../../visuals.ts";
import * as eq from "../../eq.ts";
import * as display from "../../display.ts";

// ---- palette ----------------------------------------------------------------

const INK = "#cfe0ff";
const INK_DIM = "#4d74cf";
const BADGE = "#b9d0ff";
const BADGE_INK = "#0b1f66";
const BADGE_DIM = "#1f3f9a";
const BADGE_DIM_INK = "#6f8fdc";
const PEAK = "#ffffff";

const BARS = 20;

// ---- dot-font text ------------------------------------------------------------

/** Draw `text` in the dot font at `scale` px per cell. Returns the width. */
function text(c: CanvasRenderingContext2D, s: string, x: number, y: number, scale: number, color: string): number {
  const { bits, w } = rasterise(s, 0);
  c.fillStyle = color;
  for (let cx = 0; cx < w; cx++) {
    for (let r = 0; r < ROWS; r++) {
      if (bits[r * w + cx]) c.fillRect(x + cx * scale, y + r * scale, scale, scale);
    }
  }
  return w * scale;
}

function textWidth(s: string, scale: number): number {
  return rasterise(s, 0).w * scale;
}

/** A pill-shaped readout badge: filled when lit, recessed when not. */
function badge(c: CanvasRenderingContext2D, s: string, x: number, y: number, lit: boolean, minW = 0): number {
  const tw = textWidth(s, 1);
  const w = Math.max(minW, tw + 7);
  const h = 11;
  c.fillStyle = lit ? BADGE : BADGE_DIM;
  c.beginPath();
  if (typeof c.roundRect === "function") c.roundRect(x, y, w, h, 3);
  else c.rect(x, y, w, h);
  c.fill();
  text(c, s, x + Math.round((w - tw) / 2), y + 2, 1, lit ? BADGE_INK : BADGE_DIM_INK);
  return w;
}

// ---- canvases -----------------------------------------------------------------

interface Lcd {
  cv: HTMLCanvasElement;
  c: CanvasRenderingContext2D;
  w: number;
  h: number;
}

function lcd(id: string): Lcd {
  const cv = document.getElementById(id) as HTMLCanvasElement;
  return { cv, c: cv.getContext("2d")!, w: 0, h: 0 };
}

/** Size the backing store to the CSS box (DPR-aware). Design height comes
 *  from the markup's height attribute, latched once. */
function fit(l: Lcd): boolean {
  const dpr = window.devicePixelRatio || 1;
  const w = l.cv.clientWidth;
  if (!l.cv.dataset.designH) l.cv.dataset.designH = l.cv.getAttribute("height")!;
  const h = parseInt(l.cv.dataset.designH, 10);
  if (w <= 0) return false;
  if (w === l.w && h === l.h && l.cv.width === Math.round(w * dpr)) return true;
  l.w = w;
  l.h = h;
  l.cv.width = Math.round(w * dpr);
  l.cv.height = Math.round(h * dpr);
  l.cv.style.height = h + "px";
  l.c.setTransform(dpr, 0, 0, dpr, 0, 0);
  l.c.imageSmoothingEnabled = false;
  return true;
}

// ---- state --------------------------------------------------------------------

let active = false;
let raf = 0;
let last = 0;
let main: Lcd;
let shade: Lcd;
let curve: HTMLCanvasElement;

const targets = new Float32Array(BARS);
const levels = new Float32Array(BARS);
const peaks = new Float32Array(BARS);

let playStartedAt = 0; // session clock, when no track timeline is known
let scrollX = 0;
let scrollText = "";
let shadeScrollX = 0;

// ---- what the display says -----------------------------------------------------

function elapsedMs(): number | null {
  const meta = player.getMetadata();
  const now = player.timelineMs();
  if (meta?.now && now !== null) {
    const e = now - meta.now.startMs;
    if (e >= 0) return e;
  }
  return playStartedAt ? Date.now() - playStartedAt : null;
}

function fmtTime(ms: number): string {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  return `${m}:${String(s % 60).padStart(2, "0")}`;
}

/** The title line: what is heard, then what is next, in the skin's idiom. */
function titleLine(): string {
  const state = player.getState();
  const fault = player.getFault();
  if (state === "live") {
    const meta = player.getMetadata();
    if (meta?.kind === "off") return `${display.stationName()} - OFF AIR`;
    const cur = display.currentLine();
    const next = display.nextLine();
    return next ? `${cur}  ***  ${next}` : cur;
  }
  if (state === "tuning") {
    return fault ? `${display.faultLabel(fault)} - RECONNECTING` : `${display.stationName()} - TUNING`;
  }
  if (state === "error") return `${display.faultLabel(fault) || "FAULT"} - ${fault?.message ?? ""}`.trim();
  return `${display.STATION_FALLBACK} - PRESS PLAY`;
}

// ---- main display ----------------------------------------------------------------
//
// Laid out as the classic skin: readouts across the top (time, kbps / kHz,
// STEREO and NO DJ) with the analyser on the right, and the title on its own
// darker band along the foot. The glass is painted here too — a domed sheen
// across the top and a soft vignette — rather than left to CSS, so the
// highlights curve with the corners.

const RADIUS = 5;
const PAD = 10;
const BAND_H = 34; // the title band along the foot

function roundedRect(c: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, r: number) {
  c.beginPath();
  if (typeof c.roundRect === "function") c.roundRect(x, y, w, h, r);
  else c.rect(x, y, w, h);
}

function drawGlass(c: CanvasRenderingContext2D, w: number, h: number) {
  // body: royal blue fading to navy
  const g = c.createLinearGradient(0, 0, 0, h);
  g.addColorStop(0, "#2d5ad1");
  g.addColorStop(0.35, "#153a9c");
  g.addColorStop(1, "#0a1d5e");
  c.fillStyle = g;
  c.fillRect(0, 0, w, h);

  // title band along the foot: darker, with a lit lower lip
  c.fillStyle = "rgba(3,12,48,.42)";
  c.fillRect(0, h - BAND_H, w, BAND_H);
  c.fillStyle = "rgba(0,0,0,.35)";
  c.fillRect(0, h - BAND_H, w, 1);
  c.fillStyle = "rgba(160,190,255,.16)";
  c.fillRect(0, h - BAND_H + 1, w, 1);

  // domed sheen: a wide ellipse of light whose curved lower edge shows
  c.save();
  c.beginPath();
  c.ellipse(w * 0.42, -h * 0.55, w * 0.72, h * 0.95, 0, 0, Math.PI * 2);
  c.clip();
  const sheen = c.createLinearGradient(0, 0, 0, h * 0.42);
  sheen.addColorStop(0, "rgba(255,255,255,.26)");
  sheen.addColorStop(1, "rgba(255,255,255,.06)");
  c.fillStyle = sheen;
  c.fillRect(0, 0, w, h * 0.42);
  c.restore();

  // specular kiss at the top-left corner
  const spec = c.createRadialGradient(w * 0.12, 2, 2, w * 0.12, 2, w * 0.22);
  spec.addColorStop(0, "rgba(255,255,255,.20)");
  spec.addColorStop(1, "rgba(255,255,255,0)");
  c.fillStyle = spec;
  c.fillRect(0, 0, w * 0.4, h * 0.5);

  // vignette into the bezel
  const vig = c.createRadialGradient(w / 2, h * 0.6, w * 0.25, w / 2, h * 0.6, w * 0.75);
  vig.addColorStop(0, "rgba(0,0,0,0)");
  vig.addColorStop(1, "rgba(0,0,0,.28)");
  c.fillStyle = vig;
  c.fillRect(0, 0, w, h);

  // inner shadow along the top edge, where the glass sits under the bezel
  const lip = c.createLinearGradient(0, 0, 0, 6);
  lip.addColorStop(0, "rgba(0,0,0,.45)");
  lip.addColorStop(1, "rgba(0,0,0,0)");
  c.fillStyle = lip;
  c.fillRect(0, 0, w, 6);
}

function drawAnalyser(c: CanvasRenderingContext2D, x: number, y: number, w: number, h: number) {
  // recessed window with the classic dotted backdrop
  c.fillStyle = "rgba(3,12,48,.45)";
  roundedRect(c, x - 3, y - 3, w + 6, h + 6, 3);
  c.fill();
  c.fillStyle = "rgba(130,165,255,.18)";
  for (let gy = y + h - 1; gy > y; gy -= 4) {
    for (let gx = x + 1; gx < x + w; gx += 4) c.fillRect(gx, gy, 1, 1);
  }

  const gap = 2;
  const barW = Math.floor((w - gap * (BARS - 1)) / BARS);
  const span = barW * BARS + gap * (BARS - 1);
  const x0 = x + Math.floor((w - span) / 2);
  const grad = c.createLinearGradient(0, y + h, 0, y);
  grad.addColorStop(0, "#3f74e6");
  grad.addColorStop(0.55, "#8db6ff");
  grad.addColorStop(1, "#eaf2ff");
  for (let i = 0; i < BARS; i++) {
    const v = Math.min(1, targets[i] * (1 + (i / BARS) * 0.35) * 0.85);
    levels[i] += (v - levels[i]) * (v > levels[i] ? 0.6 : 0.14);
    peaks[i] = Math.max(peaks[i] - 0.012, levels[i]);
    const bx = x0 + i * (barW + gap);
    // quantised to 2px steps, like a segmented meter
    const hgt = Math.floor((levels[i] * h) / 2) * 2;
    if (hgt > 0) {
      c.fillStyle = grad;
      c.fillRect(bx, y + h - hgt, barW, hgt);
    }
    const ph = Math.floor((peaks[i] * h) / 2) * 2;
    if (ph > 1) {
      c.fillStyle = PEAK;
      c.fillRect(bx, y + h - ph - 1, barW, 1);
    }
  }
}

function drawMain(dt: number) {
  if (!fit(main)) return;
  const { c, w, h } = main;
  c.clearRect(0, 0, w, h);
  const state = player.getState();
  const live = state === "live";
  const blink = Math.floor(Date.now() / 500) % 2 === 0;

  c.save();
  roundedRect(c, 0, 0, w, h, RADIUS);
  c.clip();
  drawGlass(c, w, h);

  // -- play-state glyph, then the time, big
  const top = 14;
  c.fillStyle = INK;
  if (live) {
    c.beginPath(); c.moveTo(PAD, top + 6); c.lineTo(PAD, top + 16); c.lineTo(PAD + 7, top + 11); c.closePath(); c.fill();
  } else if (state === "tuning") {
    if (blink) { c.fillRect(PAD - 1, top + 6, 3, 10); c.fillRect(PAD + 4, top + 6, 3, 10); }
  } else {
    c.fillRect(PAD, top + 7, 8, 8);
  }
  const t = live
    ? (() => { const e = elapsedMs(); return e === null ? "0:00" : fmtTime(e); })()
    : state === "tuning" ? (blink ? "-:--" : "    ") : "0:00";
  const timeX = PAD + 16;
  const timeW = text(c, t.padStart(5, " "), timeX, top, 3, INK);

  // -- readouts, two evenly spaced rows after the time
  const kbps = player.getBitrateKbps() || player.getStation()?.bitrateKbps || 0;
  const khz = Math.round(player.getSampleRate() / 1000);
  const rx = timeX + timeW + 20;
  let x = rx;
  x += text(c, "KBPS", x, top + 2, 1, live ? INK : INK_DIM) + 5;
  x += badge(c, live && kbps ? String(kbps) : "---", x, top, live, 28) + 12;
  x += text(c, "KHZ", x, top + 2, 1, live ? INK : INK_DIM) + 5;
  x += badge(c, live && khz ? String(khz) : "--", x, top, live, 24);
  const readoutsEnd = x;

  x = rx;
  x += badge(c, "STEREO", x, top + 22, live) + 8;
  // NO DJ blinks while the other mount is being brought in.
  badge(c, "NO DJ", x, top + 22, player.isSwitching() ? blink : player.getNoDj());

  // -- analyser: everything to the right of the readouts
  const specX = readoutsEnd + 22;
  drawAnalyser(c, specX, top - 2, w - PAD - specX, 40);

  // -- title band: scrolling when it does not fit
  const line = titleLine().toUpperCase();
  if (line !== scrollText) {
    scrollText = line;
    scrollX = 0;
  }
  const scale = 2;
  const avail = w - PAD * 2;
  const tw = textWidth(line, scale);
  const ty = h - BAND_H + Math.round((BAND_H - ROWS * scale) / 2);
  c.save();
  c.beginPath();
  c.rect(PAD, h - BAND_H, avail, BAND_H);
  c.clip();
  if (tw <= avail) {
    text(c, line, PAD, ty, scale, INK);
  } else {
    const loop = line + "  ***  ";
    const lw = textWidth(loop, scale);
    if (live || state === "tuning") scrollX = (scrollX + dt * 0.03) % lw;
    const off = Math.floor(scrollX / scale) * scale;
    text(c, loop, PAD - off, ty, scale, INK);
    text(c, loop, PAD - off + lw, ty, scale, INK);
  }
  c.restore();

  // -- a last curved glint over everything, faint enough to read through
  const glint = c.createLinearGradient(0, 0, 0, h * 0.3);
  glint.addColorStop(0, "rgba(255,255,255,.07)");
  glint.addColorStop(1, "rgba(255,255,255,0)");
  c.fillStyle = glint;
  c.fillRect(0, 0, w, h * 0.3);
  c.restore();
}

// ---- shade display -----------------------------------------------------------------

function drawShade(dt: number) {
  if (!fit(shade)) return;
  const { c, w, h } = shade;
  c.clearRect(0, 0, w, h);
  const live = player.getState() === "live";
  const e = live ? elapsedMs() : null;
  const tstr = e === null ? "0:00" : fmtTime(e);
  const tx = w - 4 - textWidth(tstr, 1);
  text(c, tstr, tx, 3, 1, INK);

  const line = titleLine().toUpperCase();
  const avail = tx - 16;
  const tw = textWidth(line, 1);
  c.save();
  c.beginPath();
  c.rect(4, 0, avail, h);
  c.clip();
  if (tw <= avail) {
    text(c, line, 4, 3, 1, INK);
  } else {
    const loop = line + "  ***  ";
    const lw = textWidth(loop, 1);
    if (live) shadeScrollX = (shadeScrollX + dt * 0.02) % lw;
    const off = Math.floor(shadeScrollX);
    text(c, loop, 4 - off, 3, 1, INK);
    text(c, loop, 4 - off + lw, 3, 1, INK);
  }
  c.restore();
}

// ---- EQ curve ---------------------------------------------------------------------

/** A smooth response curve through the ten band gains (and the preamp as
 *  its baseline), the way the classic skin previewed its equaliser. */
function drawCurve() {
  const c = curve.getContext("2d")!;
  const dpr = window.devicePixelRatio || 1;
  const w = 72, h = 30;
  if (curve.width !== Math.round(w * dpr)) {
    curve.width = Math.round(w * dpr);
    curve.height = Math.round(h * dpr);
  }
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.clearRect(0, 0, w, h);
  // zero line
  c.fillStyle = INK_DIM;
  c.fillRect(4, Math.round(h / 2), w - 8, 1);

  const gains = eq.isEnabled() ? eq.getGains() : new Array(eq.FREQS.length).fill(0);
  const n = gains.length;
  const px = (i: number) => 4 + (i / (n - 1)) * (w - 8);
  const py = (db: number) => h / 2 - (db / eq.MAX_DB) * (h / 2 - 3);
  c.strokeStyle = eq.isEnabled() ? INK : INK_DIM;
  c.lineWidth = 1.5;
  c.beginPath();
  c.moveTo(px(0), py(gains[0]));
  for (let i = 0; i < n - 1; i++) {
    // Catmull-Rom → Bézier, so the curve passes through every band.
    const p0 = gains[Math.max(0, i - 1)], p1 = gains[i], p2 = gains[i + 1], p3 = gains[Math.min(n - 1, i + 2)];
    const x1 = px(i), x2 = px(i + 1);
    c.bezierCurveTo(
      x1 + (x2 - x1) / 3, py(p1 + (p2 - p0) / 6),
      x2 - (x2 - x1) / 3, py(p2 - (p3 - p1) / 6),
      x2, py(p2),
    );
  }
  c.stroke();
}

// ---- position bar -------------------------------------------------------------------

function updateSeek() {
  const knob = document.getElementById("aSeekKnob")!;
  const track = document.getElementById("aSeek")!;
  const meta = player.getMetadata();
  const now = player.timelineMs();
  let t = 0;
  if (player.getState() === "live" && meta?.now && now !== null && meta.now.durationMs > 0) {
    t = Math.max(0, Math.min(1, (now - meta.now.startMs) / meta.now.durationMs));
  }
  knob.style.left = `calc(14px + ${(t * 100).toFixed(2)}% - ${(t * 28).toFixed(1)}px)`;
  track.title = meta?.now && t > 0 ? `Track position ${Math.round(t * 100)}%` : "Track position";
}

// ---- frame loop ------------------------------------------------------------------------

function frame(t: number) {
  const dt = last ? Math.min(64, t - last) : 16;
  last = t;
  if (document.body.classList.contains("mini")) drawShade(dt);
  else drawMain(dt);
  raf = requestAnimationFrame(frame);
}

// ---- docking the shared equaliser ------------------------------------------------------

const eqAnchor = { parent: null as HTMLElement | null, next: null as Element | null };

function moveEqIn() {
  const sec = document.querySelector<HTMLElement>(".eq");
  if (!sec) return;
  eqAnchor.parent = sec.parentElement;
  eqAnchor.next = sec.nextElementSibling;
  document.getElementById("aeqSlot")!.appendChild(sec);
}

function moveEqOut() {
  const sec = document.querySelector<HTMLElement>(".eq");
  if (!sec || !eqAnchor.parent) return;
  eqAnchor.parent.insertBefore(sec, eqAnchor.next);
}

// ---- activation ---------------------------------------------------------------------------

let volFader: { set(v: number): void; get(): number };

function activate() {
  active = true;
  moveEqIn();
  volFader.set(player.getUserVolume() * 100);
  syncTransport();
  syncEqPanel();
  updateSeek();
  drawCurve();
  last = 0;
  if (!raf) raf = requestAnimationFrame(frame);
}

function deactivate() {
  active = false;
  if (raf) { cancelAnimationFrame(raf); raf = 0; }
  moveEqOut();
}

// ---- controls -------------------------------------------------------------------------------

function syncTransport() {
  const s = player.getState();
  document.getElementById("aPlay")!.setAttribute("aria-pressed", String(s === "live" || s === "tuning"));
  document.getElementById("aPause")!.setAttribute("aria-pressed", "false");
  document.getElementById("aStop")!.setAttribute("aria-pressed", String(s === "standby"));
  document.getElementById("aMute")!.setAttribute("aria-pressed", String(player.getMuted()));
}

function syncEqPanel() {
  const hidden = document.querySelector(".eq")?.classList.contains("hidden") ?? false;
  document.getElementById("aEqPanel")!.classList.toggle("hidden", hidden);
  document.getElementById("aEqTab")!.setAttribute("aria-pressed", String(!hidden));
}

/** Open the shared window-options menu just under an element. */
function popMenu(el: HTMLElement) {
  const r = el.getBoundingClientRect();
  el.dispatchEvent(new MouseEvent("contextmenu", {
    bubbles: true, cancelable: true, clientX: r.left, clientY: r.bottom + 2,
  }));
}

const $ = (id: string) => document.getElementById(id)!;

export function initAmp() {
  main = lcd("aDisplay");
  shade = lcd("aShadeDisplay");
  curve = $("aCurve") as HTMLCanvasElement;

  // -- title bar
  // The shade key toggles: fold into the bar, or unfold when already shaded.
  $("aShade").addEventListener("click", () => {
    $(document.body.classList.contains("mini") ? "ctxFaceAmp" : "ctxFaceMini").click();
  });
  $("aShadeExpand").addEventListener("click", () => $("ctxFaceAmp").click());
  $("aClose").addEventListener("click", () => $("btnPower").click());

  // -- menu bar: window options where a menu would drop, transport where it is a verb
  for (const id of ["aMenuFile", "aMenuOptions", "aMenuView", "aSideMenu"]) {
    $(id).addEventListener("click", (e) => popMenu(e.currentTarget as HTMLElement));
  }
  $("aMenuPlay").addEventListener("click", () => $("aPlay").click());
  $("aMenuHelp").addEventListener("click", () => cmd("open_home_page"));
  $("aBolt").addEventListener("click", () => cmd("open_home_page"));

  // -- transport
  const play = () => { if (player.getState() !== "live" && player.getState() !== "tuning") player.play(); };
  $("aPlay").addEventListener("click", play);
  $("aShadePlay").addEventListener("click", play);
  $("aPause").addEventListener("click", () => player.pause());
  $("aShadePause").addEventListener("click", () => player.pause());
  $("aStop").addEventListener("click", () => player.stop());
  $("aShadeStop").addEventListener("click", () => player.stop());
  $("aMute").addEventListener("click", () => player.setMuted(!player.getMuted()));
  const step = (dir: 1 | -1) => {
    const name = eq.stepPreset(dir);
    display.flash("EQ · " + name.toUpperCase());
  };
  $("aPrev").addEventListener("click", () => step(-1));
  $("aNext").addEventListener("click", () => step(1));

  volFader = makeFader($("aVol"), {
    min: 0, max: 100, value: player.getUserVolume() * 100, vertical: false,
    format: (n) => Math.round(n) + "%",
    onChange: (n) => { if (active) player.setUserVolume(n / 100); },
  });

  // -- side keys
  $("aNoDj").addEventListener("click", () => player.setNoDj(!player.getNoDj()));
  player.onNoDjChange((on, switching) => {
    $("aNoDj").setAttribute("aria-pressed", String(on));
    $("aNoDjLed").classList.toggle("lit", on && !switching);
    $("aNoDjLed").classList.toggle("pending", switching);
  });
  $("aNoDj").setAttribute("aria-pressed", String(player.getNoDj()));
  $("aNoDjLed").classList.toggle("lit", player.getNoDj());

  const ctxOnTop = $("ctxOnTop");
  $("aOnTop").addEventListener("click", () => ctxOnTop.click());
  new MutationObserver(() => {
    const on = ctxOnTop.getAttribute("aria-checked") === "true";
    $("aOnTop").setAttribute("aria-pressed", String(on));
    $("aOnTopLed").classList.toggle("lit", on);
  }).observe(ctxOnTop, { attributes: true, attributeFilter: ["aria-checked"] });

  // -- equaliser panel
  $("aEqTab").addEventListener("click", () => $("btnEq").click());
  new MutationObserver(syncEqPanel).observe(document.querySelector(".eq")!, {
    attributes: true, attributeFilter: ["class"],
  });
  $("aEqOn").addEventListener("click", () => eq.setEnabled(!eq.isEnabled()));
  $("aEqFlat").addEventListener("click", () => {
    eq.applyPreset("flat");
    display.flash("EQ · FLAT");
  });
  eq.onEqChange(() => {
    $("aEqOn").setAttribute("aria-pressed", String(eq.isEnabled()));
    $("aEqOnLed").classList.toggle("lit", eq.isEnabled());
    if (active) drawCurve();
  });

  // -- engine
  player.onSpectrum((bars) => {
    const n = Math.min(BARS, bars.length);
    for (let i = 0; i < n; i++) targets[i] = bars[i];
  });
  player.onState((s) => {
    if (s === "tuning" && !playStartedAt) playStartedAt = Date.now();
    if (s === "standby" || s === "error") playStartedAt = 0;
    if (s !== "live") targets.fill(0);
    syncTransport();
    updateSeek();
  });
  player.onMuteChange(syncTransport);
  player.onMetadata(updateSeek);
  setInterval(() => { if (active) updateSeek(); }, 1000);

  // -- fascia doubles as the drag handle (see DRAG_REGIONS in main.ts)
  for (const sel of [
    "#faceAmp", ".atitle", ".agrip", ".aname", ".amain", ".arow", ".alcd", "#aDisplay",
    ".aside", ".aseek", ".aseek-track", ".aseek-knob", ".atransport", ".aroundkeys",
    ".aeqpanel", ".aeqleft", ".aeqscale", ".aeqscale span", "#aCurve", ".ashade",
    ".ashade-lcd", "#aShadeDisplay", ".amenu",
  ]) {
    document.querySelectorAll<HTMLElement>(sel).forEach((el) => {
      el.setAttribute("data-tauri-drag-region", "");
    });
  }

  onFaceChange((f) => {
    if (f === "amp") activate();
    else deactivate();
  });
}
