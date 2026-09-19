// ---------------------------------------------------------------------------
// eq.ts — the graphic equaliser as a model.
//
// The faders themselves live in the default face's DOM (main.ts builds them
// and every face docks that same section), so this module is the one place
// other faces read the curve from, cycle presets through, or switch the EQ
// in and out — without touching the widgets.
// ---------------------------------------------------------------------------

import { cmd } from "./player.ts";

export const FREQS = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
export const MAX_DB = 12;

export const PRESETS: Record<string, number[]> = {
  flat:   [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  pirate: [4, 5, 2, -1, -2, 0, 2, 4, 5, 3], // scooped mids, hyped top — cassette-dub feel
  bass:   [8, 7, 5, 2, 0, 0, 0, 0, 1, 2],
  voice:  [-4, -3, 0, 3, 5, 5, 3, 1, -1, -2],
};
export const PRESET_ORDER = ["flat", "pirate", "bass", "voice"];

type Cb = () => void;
const cbs: Cb[] = [];
const gains = new Array(FREQS.length).fill(0) as number[];
let preamp = 0;
let enabled = true;
let applier: ((gains: number[]) => void) | null = null;

function notify() {
  for (const cb of cbs) cb();
}

export function onEqChange(cb: Cb) { cbs.push(cb); }
export function getGains(): readonly number[] { return gains; }
export function getPreamp(): number { return preamp; }
export function isEnabled(): boolean { return enabled; }

/** Called by the fader that owns band `i` whenever it moves. */
export function noteBand(i: number, db: number) {
  gains[i] = db;
  notify();
}
export function notePreamp(db: number) {
  preamp = db;
  notify();
}

/** The default face registers how to move all ten faders at once. */
export function registerApplier(fn: (gains: number[]) => void) {
  applier = fn;
}

export function applyPreset(name: string) {
  const p = PRESETS[name];
  if (p && applier) applier(p);
}

/** Which preset the faders currently sit on exactly, if any. */
export function matchingPreset(): string | null {
  return PRESET_ORDER.find((n) => PRESETS[n].every((v, i) => Math.abs(v - gains[i]) < 0.05)) ?? null;
}

/** Step to the next / previous preset in the list (wrapping). Returns its name. */
export function stepPreset(dir: 1 | -1): string {
  const cur = matchingPreset();
  const i = cur ? PRESET_ORDER.indexOf(cur) : -1;
  const next = PRESET_ORDER[(i + dir + PRESET_ORDER.length) % PRESET_ORDER.length];
  applyPreset(next);
  return next;
}

/** EQ in / out. The engine glides every band to 0 dB while the faders keep
 *  their positions, so switching back restores the curve without a click. */
export function setEnabled(on: boolean) {
  if (on === enabled) return;
  enabled = on;
  cmd("set_eq_enabled", { enabled: on });
  notify();
}
