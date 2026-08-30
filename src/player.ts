// ---------------------------------------------------------------------------
// player.ts — shared player core.
//
// One source of truth for engine state, volume arbitration and engine events,
// so that every face (default receiver, vintage 80s, …) wires against the
// same model instead of duplicating IPC plumbing.
//
// Volume arbitration: the engine level is userVolume × signalFactor. The
// default face keeps signalFactor at 1; the vintage face scales it with the
// tuning-needle signal strength so the stream fades in under the static.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type EngineState = "standby" | "tuning" | "live" | "error";

// ---- metadata ---------------------------------------------------------------
//
// Mirrors the payloads the Rust engine emits, which in turn mirror the
// station's in-band ICY contract (music-db docs/player-stream-metadata-api.md).
// The engine releases these in step with the audio, so `playheadMs` is the
// station timeline position of what is being heard *right now* — anchor on it
// and interpolate to run a countdown without any clock sync.

export interface Segment {
  kind: string;
  id: string;
  artist?: string;
  title?: string;
  startMs: number;
  durationMs: number;
}

export interface Programme {
  id: string;
  name: string;
  showTitle?: string;
  dj?: string;
}

export interface Station {
  name: string;
  description: string;
  genre: string;
  url: string;
  bitrateKbps: number;
}

export interface Metadata {
  title: string;
  /** track | jingle | talk | off | hb */
  kind: string;
  seq: number;
  playheadMs: number;
  itemId: string;
  station?: Station;
  programme?: Programme;
  now?: Segment;
  next: Segment[];
  talk: { id: string; startMs: number; durationMs: number; dj?: string; fromChat?: boolean }[];
  jingles: { id: string; startMs: number; durationMs: number; role: string; name?: string }[];
  /** The schedule was dropped to fit the block's byte cap — the lists are empty
   *  because we were not told, not because nothing is coming up. */
  scheduleTruncated: boolean;
}

export interface SyncInfo {
  lagMs: number | null;
  excessMs: number | null;
  bufferMs: number;
  bufferBytes: number;
  bitrateKbps: number;
  targetMs: number;
  ceilingMs: number;
  state: string;
  action: string;
  droppedMs: number;
  catchups: number;
  reconnects: number;
}

export const DEFAULT_URL = "https://stream.ltbr.fm/live";

// Fire-and-forget command helper — the engine is authoritative, so a failed
// command must never take down the UI.
export function cmd(name: string, args?: Record<string, unknown>): void {
  invoke(name, args).catch((e) => console.error(`cmd ${name} failed:`, e));
}

// ---- observable state -------------------------------------------------------

type StateCb = (s: EngineState, message?: string) => void;
type TitleCb = (title: string) => void;
type SpectrumCb = (bars: number[]) => void;
type FaultCb = (message: string) => void;
type MuteCb = (muted: boolean) => void;
type MetaCb = (m: Metadata) => void;
type SyncCb = (s: SyncInfo) => void;

const stateCbs: StateCb[] = [];
const titleCbs: TitleCb[] = [];
const spectrumCbs: SpectrumCb[] = [];
const faultCbs: FaultCb[] = [];
const muteCbs: MuteCb[] = [];
const metaCbs: MetaCb[] = [];
const syncCbs: SyncCb[] = [];

let engineState: EngineState = "standby";
let nowPlaying = "";
let metadata: Metadata | null = null;
let station: Station | null = null;
/** Anchor for interpolating the station timeline between blocks. */
let anchor: { playheadMs: number; at: number } | null = null;
let userVolume = 0.8;
let signalFactor = 1;
let muted = false;

export function onState(cb: StateCb) { stateCbs.push(cb); }
export function onNowPlaying(cb: TitleCb) { titleCbs.push(cb); }
export function onSpectrum(cb: SpectrumCb) { spectrumCbs.push(cb); }
export function onFault(cb: FaultCb) { faultCbs.push(cb); }
export function onMuteChange(cb: MuteCb) { muteCbs.push(cb); }
export function onMetadata(cb: MetaCb) { metaCbs.push(cb); }
export function onSync(cb: SyncCb) { syncCbs.push(cb); }

export function getState(): EngineState { return engineState; }
export function getNowPlaying(): string { return nowPlaying; }
export function getUserVolume(): number { return userVolume; }
export function getMuted(): boolean { return muted; }
export function getMetadata(): Metadata | null { return metadata; }
export function getStation(): Station | null { return station; }

/** Where the listener is on the station's timeline, interpolated forward from
 *  the last block. Null until one has arrived. */
export function timelineMs(): number | null {
  if (!anchor) return null;
  return anchor.playheadMs + (Date.now() - anchor.at);
}

function emitState(s: EngineState, message?: string) {
  engineState = s;
  for (const cb of stateCbs) cb(s, message);
}

// ---- transport ---------------------------------------------------------------

export function play(url: string = DEFAULT_URL) {
  emitState("tuning", "acquiring…");
  cmd("play", { url });
}

export function pause() {
  cmd("pause");
  emitState("standby", "paused");
}

export function stop() {
  cmd("stop");
  nowPlaying = "";
  metadata = null;
  anchor = null;
  emitState("standby");
}

// ---- volume / mute -----------------------------------------------------------

function pushVolume() {
  cmd("set_volume", { level: userVolume * signalFactor });
}

/** The listener-facing volume (0..1) — knob or fader position. */
export function setUserVolume(v: number) {
  userVolume = Math.max(0, Math.min(1, v));
  pushVolume();
}

/** Tuning attenuation (0..1). 1 = full carrier lock; faces off-station fade
 *  the stream under the static by lowering this. */
export function setSignalFactor(f: number) {
  let nf = Math.max(0, Math.min(1, f));
  if (nf < 0.005) nf = 0;
  if (nf > 0.995) nf = 1;
  if (Math.abs(nf - signalFactor) < 0.005) return; // don't flood the IPC
  signalFactor = nf;
  pushVolume();
}

export function setMuted(m: boolean) {
  muted = m;
  cmd("set_mute", { muted: m });
  for (const cb of muteCbs) cb(m);
}

// ---- engine events -----------------------------------------------------------

interface StateEvent {
  state: EngineState;
  message?: string;
}

export function initPlayer() {
  listen<StateEvent>("state", (e) => {
    emitState(e.payload.state, e.payload.message);
    if (e.payload.state === "error" && e.payload.message) {
      for (const cb of faultCbs) cb(e.payload.message);
    } else if (e.payload.state !== "error") {
      for (const cb of faultCbs) cb("");
    }
  });

  listen<{ title: string }>("nowplaying", (e) => {
    nowPlaying = e.payload.title || "";
    for (const cb of titleCbs) cb(nowPlaying);
  });

  listen<Metadata>("metadata", (e) => {
    metadata = e.payload;
    if (e.payload.station) station = e.payload.station;
    // Released in step with the audio, so "now" really is now.
    anchor = { playheadMs: e.payload.playheadMs, at: Date.now() };
    for (const cb of metaCbs) cb(metadata);
  });

  listen<SyncInfo>("sync", (e) => {
    for (const cb of syncCbs) cb(e.payload);
  });

  listen<{ message: string }>("fault", (e) => {
    for (const cb of faultCbs) cb(e.payload.message);
  });

  listen<number[]>("spectrum", (e) => {
    for (const cb of spectrumCbs) cb(e.payload);
  });
}
