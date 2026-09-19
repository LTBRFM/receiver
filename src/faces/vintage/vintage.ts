// ---------------------------------------------------------------------------
// vintage.ts — the "Wavemaster 3000" face: an early-80s British FM receiver.
//
// Interaction model: the big knurled knob sweeps a needle across a backlit
// dial. Off-carrier you hear shaped inter-station static (Web Audio, local);
// as the needle nears LTBR·FM's printed frequency the engine stream is
// started and crossfaded in — engine volume scales with signal strength,
// static with its complement — exactly like pulling a real station out of
// the noise. Powering on always starts off-station.
// ---------------------------------------------------------------------------

import * as player from "../../player.ts";
import { onFaceChange } from "../../faces.ts";
import { Dial, F_MIN, F_MAX, STATION_FREQ } from "./dial.ts";
import { VUMeter } from "./vu.ts";
import { StaticNoise } from "./noise.ts";

const OFF_STATION_FREQ = 94.0; // where the needle rests at power-on
const SIG_WIDTH = 0.28; // MHz — gaussian width of the carrier lobe
const CAPTURE = 0.5; // MHz — start the stream inside this window
const RELEASE = 0.65; // MHz — stop it again outside this (hysteresis)

function signalAt(f: number): number {
  const d = (f - STATION_FREQ) / SIG_WIDTH;
  return Math.exp(-d * d);
}

// ---- rotary knob widget ------------------------------------------------------

interface KnobOpts {
  min: number;
  max: number;
  value: number;
  /** pixels of vertical drag for a full sweep */
  travel?: number;
  /**
   * Multi-turn mode: number of full cap revolutions to traverse the range
   * (like a geared tuning capacitor). Switches the pointer gesture to
   * circular dragging around the knob centre.
   */
  turns?: number;
  wheelStep: number;
  keyStep: number;
  format: (v: number) => string;
  onInput: (v: number) => void;
}

const KNOB_SWEEP = 270; // degrees, -135..+135

function makeKnob(el: HTMLElement, opts: KnobOpts) {
  const cap = el.querySelector<HTMLElement>(".vknob-cap")!;
  let v = opts.value;
  const travel = opts.travel ?? 220;

  const paint = () => {
    const t = (v - opts.min) / (opts.max - opts.min);
    const deg = opts.turns
      ? t * opts.turns * 360 // geared: several full revolutions end-to-end
      : -KNOB_SWEEP / 2 + t * KNOB_SWEEP;
    cap.style.transform = `rotate(${deg}deg)`;
    el.setAttribute("aria-valuenow", String(Math.round(v * 100) / 100));
    el.setAttribute("aria-valuetext", opts.format(v));
    opts.onInput(v);
  };

  const nudge = (dv: number) => {
    v = Math.max(opts.min, Math.min(opts.max, v + dv));
    paint();
  };

  let dragY = 0;
  let dragV = 0;
  let dragA = 0; // last pointer angle around the knob centre (degrees)
  const angleOf = (e: PointerEvent) => {
    const r = el.getBoundingClientRect();
    return Math.atan2(e.clientY - (r.top + r.height / 2), e.clientX - (r.left + r.width / 2)) * 180 / Math.PI;
  };
  el.addEventListener("pointerdown", (e) => {
    el.setPointerCapture(e.pointerId);
    dragY = e.clientY;
    dragV = v;
    if (opts.turns) dragA = angleOf(e);
    el.focus();
    e.preventDefault();
  });
  el.addEventListener("pointermove", (e) => {
    if (!el.hasPointerCapture(e.pointerId)) return;
    if (opts.turns) {
      // circular drag: follow the finger around the cap; each full lap
      // advances the value by range/turns, like a real geared tuner
      const a = angleOf(e);
      let da = a - dragA;
      if (da > 180) da -= 360;
      if (da < -180) da += 360;
      dragA = a;
      v = Math.max(opts.min, Math.min(opts.max, v + (da / 360) * (opts.max - opts.min) / opts.turns));
    } else {
      const dy = dragY - e.clientY; // up = clockwise = increase
      v = Math.max(opts.min, Math.min(opts.max, dragV + (dy / travel) * (opts.max - opts.min)));
    }
    paint();
  });
  el.addEventListener("wheel", (e) => {
    e.preventDefault();
    nudge((e.deltaY < 0 ? 1 : -1) * opts.wheelStep);
  }, { passive: false });
  el.addEventListener("keydown", (e) => {
    const s = e.shiftKey ? opts.keyStep * 4 : opts.keyStep;
    let hit = true;
    switch (e.key) {
      case "ArrowUp": case "ArrowRight": nudge(s); break;
      case "ArrowDown": case "ArrowLeft": nudge(-s); break;
      case "Home": v = opts.min; paint(); break;
      case "End": v = opts.max; paint(); break;
      default: hit = false;
    }
    if (hit) e.preventDefault();
  });

  paint();
  return {
    set(nv: number) { v = Math.max(opts.min, Math.min(opts.max, nv)); paint(); },
    get() { return v; },
  };
}

// ---- face state --------------------------------------------------------------

let active = false;
let powered = false;
let tunedFreq = OFF_STATION_FREQ; // knob position
let needleFreq = OFF_STATION_FREQ; // spring-follows tunedFreq
let needleVel = 0;
let lamp = 0; // backlight ramp 0..1
let liveRamp = 0; // 0 = no programme audio yet, 1 = stream is up
let stationHeld = false; // stream session requested (capture hysteresis)
let retryAt = 0; // watchdog for engine errors while on-station

let lastBars: number[] = [];
let raf = 0;
let last = 0;

let dial: Dial;
let vuL: VUMeter;
let vuR: VUMeter;
const noise = new StaticNoise();

let volKnob: { set(v: number): void; get(): number };

// ---- tuning / crossfade logic -------------------------------------------------

function applyTuning() {
  if (!active || !powered) return;
  const df = Math.abs(needleFreq - STATION_FREQ);

  if (!stationHeld && df < CAPTURE) {
    stationHeld = true;
    player.play();
  } else if (stationHeld && df > RELEASE) {
    stationHeld = false;
    player.stop();
  }
  player.setSignalFactor(signalAt(needleFreq));
}

function setPowered(on: boolean) {
  powered = on;
  const btn = document.getElementById("vPower")!;
  btn.setAttribute("aria-pressed", String(on));
  document.getElementById("faceVintage")!.classList.toggle("vpowered", on);
  syncLamps();
  if (on) {
    noise.start();
    // NB: stationHeld is left as-is — activate() pre-holds it when a stream
    // is already live, so we never restart a running session here.
    applyTuning();
  } else {
    stationHeld = false;
    player.stop();
    player.setSignalFactor(1);
    noise.setLevel(0);
  }
}

// ---- per-frame ----------------------------------------------------------------

function frame(t: number) {
  const dt = last ? Math.min(64, t - last) : 16;
  last = t;
  const dts = dt / 1000;

  // needle follows the knob with mechanical lag (critically damped spring)
  const K = 260, D = 32;
  needleVel += (K * (tunedFreq - needleFreq) - D * needleVel) * dts;
  needleFreq += needleVel * dts;
  applyTuning();

  // lamp warms up / cools down like a filament
  lamp += ((powered ? 1 : 0) - lamp) * Math.min(1, dts * (powered ? 3.2 : 5));

  // programme ramp: only once the engine reports live does the carrier count
  const isLive = player.getState() === "live";
  liveRamp += ((isLive ? 1 : 0) - liveRamp) * Math.min(1, dts * 2.2);

  const sig = signalAt(needleFreq);
  const vol = player.getUserVolume();
  const muted = player.getMuted();

  // static = everything the carrier hasn't claimed, with a slow AGC wobble
  const wobble = 0.86 + 0.14 * Math.sin(t * 0.0011) * Math.sin(t * 0.00037 + 1.7);
  const staticLevel = powered && !muted ? vol * (1 - sig * liveRamp) * wobble : 0;
  noise.setLevel(staticLevel);

  // engine error watchdog: if the needle is on-station but the engine
  // faulted, retry every few seconds — a real set would keep hissing too
  if (powered && stationHeld && player.getState() === "error") {
    if (retryAt === 0) retryAt = t + 3000;
    else if (t > retryAt) { retryAt = 0; player.play(); }
  } else {
    retryAt = 0;
  }

  // VU drive: programme level when live, noise flutter when off-carrier
  let base = 0;
  if (isLive && lastBars.length) {
    let lo = 0, hi = 0;
    const n = lastBars.length;
    for (let i = 0; i < n; i++) {
      if (i < n / 2) lo += lastBars[i]; else hi += lastBars[i];
    }
    lo /= n / 2; hi /= n / 2;
    const mix = sig * liveRamp * (muted ? 0 : 1);
    // Gentle drive: typical programme should ride around -7..-3 VU and only
    // kiss the red on peaks, not sit pinned at +3.
    vuL.setLevel(Math.min(1, Math.pow(lo, 1.35) * 1.0) * mix + staticLevel * 0.22);
    vuR.setLevel(Math.min(1, Math.pow(hi, 1.35) * 1.1) * mix + staticLevel * 0.22);
  } else {
    base = staticLevel * (0.2 + Math.random() * 0.13);
    vuL.setLevel(base);
    vuR.setLevel(base * (0.85 + Math.random() * 0.3));
  }

  dial.draw(needleFreq, lamp, powered ? sig * liveRamp : 0);
  vuL.draw(dt, lamp);
  vuR.draw(dt, lamp);

  raf = requestAnimationFrame(frame);
}

// ---- activation ----------------------------------------------------------------

const eqAnchor = { parent: null as HTMLElement | null, next: null as Element | null };

function moveEqIn() {
  const eq = document.querySelector<HTMLElement>(".eq");
  if (!eq) return;
  eqAnchor.parent = eq.parentElement;
  eqAnchor.next = eq.nextElementSibling;
  document.getElementById("veqSlot")!.appendChild(eq);
}

function moveEqOut() {
  const eq = document.querySelector<HTMLElement>(".eq");
  if (!eq || !eqAnchor.parent) return;
  eqAnchor.parent.insertBefore(eq, eqAnchor.next);
}

function activate() {
  active = true;
  moveEqIn();
  dial.fit();
  vuL.fit();
  vuR.fit();
  volKnob.set(player.getUserVolume() * 100);
  syncMuteButton();

  // Face switches always land on a cold set: anything playing on the default
  // face is stopped and the receiver comes in powered off.
  if (powered) setPowered(false);
  else player.stop();
  last = 0;
  if (!raf) raf = requestAnimationFrame(frame);
}

function deactivate() {
  active = false;
  if (raf) { cancelAnimationFrame(raf); raf = 0; }
  // power down the set (stops the stream) before handing over
  if (powered) setPowered(false);
  else player.stop();
  moveEqOut();
  noise.suspend();
  player.setSignalFactor(1); // default face gets full-volume behaviour back
}

function syncMuteButton() {
  document.getElementById("vMute")!
    .setAttribute("aria-pressed", String(player.getMuted()));
}

// NO DJ key: latching, with a pilot lamp that glows while the no-DJ mount is
// on air and flickers while the engine crossfades over to the other mount.
function syncNoDjKey(on: boolean, switching: boolean) {
  const key = document.getElementById("vNoDj")!;
  key.setAttribute("aria-pressed", String(on));
  key.classList.toggle("vpending", switching);
}

// Pilot lamps under the wordmark. TUNED follows the carrier; NO SIGNAL lights
// on a fault and pulses while the set is hunting for the station again. The
// full reason is a tooltip, never text on the fascia.
function syncLamps() {
  const state = player.getState();
  const fault = player.getFault();
  const tuned = document.getElementById("vLampTuned")!;
  const faultLamp = document.getElementById("vLampFault")!;
  tuned.classList.toggle("lit", powered && state === "live");
  faultLamp.classList.toggle("lit", powered && fault !== null);
  faultLamp.classList.toggle("hunting", powered && fault !== null && state === "tuning");
  faultLamp.title = fault?.message ?? "";
}

// ---- init -----------------------------------------------------------------------

export function initVintage() {
  dial = new Dial(document.getElementById("vDial") as HTMLCanvasElement);
  vuL = new VUMeter(document.getElementById("vVuL") as HTMLCanvasElement, "LEFT");
  vuR = new VUMeter(document.getElementById("vVuR") as HTMLCanvasElement, "RIGHT");

  makeKnob(document.getElementById("vknobTune")!, {
    min: F_MIN, max: F_MAX, value: OFF_STATION_FREQ,
    turns: 3, // three full revolutions end-to-end, ~7 MHz per turn
    wheelStep: 0.05,
    keyStep: 0.05,
    format: (f) => f.toFixed(2) + " MHz",
    onInput: (f) => { tunedFreq = f; },
  });

  volKnob = makeKnob(document.getElementById("vknobVol")!, {
    min: 0, max: 100, value: player.getUserVolume() * 100,
    wheelStep: 2,
    keyStep: 2,
    format: (n) => Math.round(n) + "%",
    onInput: (n) => player.setUserVolume(n / 100),
  });

  document.getElementById("vPower")!.addEventListener("click", () => {
    setPowered(!powered);
  });

  document.getElementById("vMute")!.addEventListener("click", () => {
    player.setMuted(!player.getMuted());
    syncMuteButton();
  });

  document.getElementById("vNoDj")!.addEventListener("click", () => {
    player.setNoDj(!player.getNoDj());
  });
  player.onNoDjChange(syncNoDjKey);
  syncNoDjKey(player.getNoDj(), player.isSwitching());

  player.onState(syncLamps);
  player.onFault(syncLamps);

  // The vintage EQ key drives the shared toggle on the default face, so the
  // panel state and window fit stay owned by one place.
  document.getElementById("vEq")!.addEventListener("click", () => {
    (document.getElementById("btnEq") as HTMLButtonElement).click();
  });

  player.onSpectrum((bars) => { lastBars = bars; });
  player.onMuteChange(syncMuteButton);

  // The dial print and meter faces carry text — re-rasterise once the
  // bundled fonts are in, or the first paint keeps the fallback font.
  document.fonts.ready.then(() => {
    dial.fit(true);
    vuL.fit(true);
    vuR.fit(true);
  });

  // The window has no titlebar — the vintage fascia doubles as drag handle.
  const DRAG = [
    "#faceVintage", ".vface", ".vtop", ".vbrand", ".vbrand small",
    ".vmeters", ".vmeter", ".vdial-bezel", "#vDial", "#vVuL", "#vVuR",
    ".vcontrols", ".vgroup", ".vgroup .vlbl", "#veqSlot",
    ".vlamps", ".vlamp",
  ];
  for (const sel of DRAG) {
    document.querySelectorAll<HTMLElement>(sel).forEach((el) => {
      el.setAttribute("data-tauri-drag-region", "");
    });
  }

  onFaceChange((f) => {
    if (f === "vintage") activate();
    else deactivate();
  });

  window.addEventListener("resize", () => {
    if (!active) return;
    dial.fit();
    vuL.fit();
    vuR.fit();
  });
}
