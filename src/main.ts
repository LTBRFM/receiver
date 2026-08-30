// ---------------------------------------------------------------------------
// main.ts — UI wiring for the LTBR·FM Receiver.
//
// All audio (decode, EQ, volume, spectrum) lives in the Rust engine. This
// module renders the controls and translates user intent into IPC commands,
// and reflects engine events back into the display.
// ---------------------------------------------------------------------------

import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import {
  initVisuals,
  setLine,
  setScroll,
  setSpectrum,
  setPlaying,
  setTuning,
  clearSpectrum,
} from "./visuals.ts";
import * as player from "./player.ts";
import { cmd, DEFAULT_URL } from "./player.ts";
import { setFace, savedFace, currentFace, onFaceChange, type FaceId } from "./faces.ts";
import { initVintage } from "./faces/vintage/vintage.ts";
import "./faces/vintage/vintage.css";

const FREQS = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
const MAX_DB = 12;

const PRESETS: Record<string, number[]> = {
  flat:   [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  pirate: [4, 5, 2, -1, -2, 0, 2, 4, 5, 3], // scooped mids, hyped top — cassette-dub feel
  bass:   [8, 7, 5, 2, 0, 0, 0, 0, 1, 2],
  voice:  [-4, -3, 0, 3, 5, 5, 3, 1, -1, -2],
};

// ---- generic fader ---------------------------------------------------------

interface FaderOpts {
  min: number;
  max: number;
  value: number;
  vertical: boolean;
  onChange: (v: number) => void;
  format?: (v: number) => string;
}

function makeFader(el: HTMLElement, opts: FaderOpts) {
  const { min, max, value, vertical, onChange, format } = opts;
  const cap = el.querySelector(".cap") as HTMLElement;
  let v = value;

  const paint = () => {
    const t = (v - min) / (max - min);
    if (vertical) cap.style.top = (1 - t) * 100 + "%";
    else cap.style.left = t * 100 + "%";
    el.setAttribute("aria-valuenow", String(Math.round(v)));
    if (format) el.setAttribute("aria-valuetext", format(v));
    onChange(v);
  };

  const setFromPointer = (e: PointerEvent) => {
    const r = el.getBoundingClientRect();
    const t = vertical
      ? 1 - (e.clientY - r.top) / r.height
      : (e.clientX - r.left) / r.width;
    v = min + Math.max(0, Math.min(1, t)) * (max - min);
    paint();
  };

  el.addEventListener("pointerdown", (e) => {
    el.setPointerCapture(e.pointerId);
    setFromPointer(e);
    el.focus();
  });
  el.addEventListener("pointermove", (e) => {
    if (el.hasPointerCapture(e.pointerId)) setFromPointer(e);
  });
  el.addEventListener("dblclick", () => {
    v = min < 0 && max > 0 ? 0 : value;
    paint();
  });
  el.addEventListener("keydown", (e) => {
    const step = e.shiftKey ? (max - min) / 100 : (max - min) / 24;
    let hit = true;
    switch (e.key) {
      case "ArrowUp": case "ArrowRight": v = Math.min(max, v + step); break;
      case "ArrowDown": case "ArrowLeft": v = Math.max(min, v - step); break;
      case "Home": v = max; break;
      case "End": v = min; break;
      case "PageUp": v = Math.min(max, v + (max - min) / 4); break;
      case "PageDown": v = Math.max(min, v - (max - min) / 4); break;
      default: hit = false;
    }
    if (hit) {
      e.preventDefault();
      paint();
    }
  });

  paint();
  return {
    set(nv: number) {
      v = nv;
      paint();
    },
    get() {
      return v;
    },
  };
}

// ---- volume + mute ---------------------------------------------------------

const volFader = makeFader(document.getElementById("volFader")!, {
  min: 0, max: 100, value: 80, vertical: false,
  format: (n) => Math.round(n) + "%",
  onChange: (n) => {
    player.setUserVolume(n / 100);
  },
});

// ---- EQ: preamp + 10 bands -------------------------------------------------

const bandsEl = document.getElementById("bands")!;
const bandFaders: { set(v: number): void; get(): number }[] = [];

const label = (hz: number) => (hz >= 1000 ? hz / 1000 + "k" : String(hz));

// preamp fader first, then a rule, then the ten bands
const preWrap = document.createElement("div");
preWrap.className = "band pre";
preWrap.innerHTML = `<span class="db" id="dbPre">0.0</span>
  <div class="fader-v" tabindex="0" role="slider" aria-label="Preamp"
       aria-valuemin="-12" aria-valuemax="12" aria-valuenow="0">
    <div class="slot"></div><div class="cap"></div></div>
  <span class="hz">PRE</span>`;
bandsEl.appendChild(preWrap);
bandsEl.appendChild(Object.assign(document.createElement("div"), { className: "rule" }));

makeFader(preWrap.querySelector(".fader-v")!, {
  min: -MAX_DB, max: MAX_DB, value: 0, vertical: true,
  format: (n) => n.toFixed(1) + " dB",
  onChange: (n) => {
    (preWrap.querySelector("#dbPre") as HTMLElement).textContent =
      (n >= 0 ? "+" : "") + n.toFixed(1);
    preWrap.classList.toggle("active", Math.abs(n) > 0.05);
    cmd("set_preamp", { db: n });
  },
});

FREQS.forEach((hz, i) => {
  const b = document.createElement("div");
  b.className = "band";
  b.innerHTML = `<span class="db">0.0</span>
    <div class="fader-v" tabindex="0" role="slider" aria-label="${label(hz)} hertz"
         aria-valuemin="-12" aria-valuemax="12" aria-valuenow="0">
      <div class="slot"></div><div class="cap"></div></div>
    <span class="hz">${label(hz)}</span>`;
  bandsEl.appendChild(b);

  bandFaders.push(
    makeFader(b.querySelector(".fader-v")!, {
      min: -MAX_DB, max: MAX_DB, value: 0, vertical: true,
      format: (n) => n.toFixed(1) + " dB",
      onChange: (n) => {
        (b.querySelector(".db") as HTMLElement).textContent =
          (n >= 0 ? "+" : "") + n.toFixed(1);
        b.classList.toggle("active", Math.abs(n) > 0.05);
        cmd("set_eq_band", { index: i, db: n });
      },
    }),
  );
});

document.querySelectorAll<HTMLButtonElement>("button.chip").forEach((btn) => {
  btn.addEventListener("click", () => {
    const p = PRESETS[btn.dataset.preset!];
    p.forEach((val, i) => bandFaders[i].set(val));
    flashLine2("EQ · " + btn.textContent!.toUpperCase());
  });
});

// ---- wordmark: click through to the station's site -------------------------
// Both faces' logos link out to ltbr.fm. They sit inside a drag region (the
// faceplate doubles as the window's drag handle, see DRAG_REGIONS below) —
// data-tauri-drag-region only intercepts an actual drag gesture, so a plain
// click still reaches this handler, same as the update-available "tx" block.
function openHomePage() {
  cmd("open_home_page");
}

for (const sel of [".brand", ".vbrand .vname"]) {
  document.querySelectorAll<HTMLElement>(sel).forEach((el) => {
    el.classList.add("clickable");
    el.title = "Open ltbr.fm";
    el.setAttribute("role", "link");
    el.setAttribute("tabindex", "0");
    el.addEventListener("click", openHomePage);
    el.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        openHomePage();
      }
    });
  });
}

// ---- transport + state -----------------------------------------------------

const txState = document.getElementById("txState")!;
const txLabel = document.getElementById("txLabel")!;
const metaVal = document.getElementById("metaVal")!;
const faultEl = document.getElementById("fault")!;
const btnPlay = document.getElementById("btnPlay")!;
const icoPlay = document.getElementById("icoPlay")!;
const streamUrl = document.getElementById("streamUrl") as HTMLInputElement;

let engineState: "standby" | "tuning" | "live" | "error" = "standby";
let nowPlaying = "";

const PLAY_PATH = "M7 4l13 8-13 8z";
const PAUSE_PATH = "M6 4h4v16H6zM14 4h4v16h-4z";

// ---- update-available indicator --------------------------------------------

let updateAvailable = false;

function refreshTx() {
  txState.classList.toggle("update", updateAvailable);
  txLabel.textContent = updateAvailable ? "New version" : "Standby";
  if (updateAvailable) {
    txState.setAttribute("role", "button");
    txState.setAttribute("tabindex", "0");
    txState.title = "A new version is available — click to download";
  } else {
    txState.removeAttribute("role");
    txState.removeAttribute("tabindex");
    txState.title = "";
  }
}

txState.addEventListener("click", () => {
  if (updateAvailable) cmd("open_download_page");
});
txState.addEventListener("keydown", (e) => {
  if (updateAvailable && (e.key === "Enter" || e.key === " ")) {
    e.preventDefault();
    cmd("open_download_page");
  }
});

listen<{ version: string }>("update_available", () => {
  updateAvailable = true;
  refreshTx();
});

// ---- auto-update overlay ----------------------------------------------------
//
// Where the install can replace itself, Rust downloads and installs a new
// version automatically on startup and restarts the app. This block only
// *renders* that: the overlay appears on the first `downloading` event,
// switches to an installing message, and hides again on `dismiss` (a failed
// download — the current version simply keeps playing).

const updOverlay = document.getElementById("updOverlay")!;
const updTitle = document.getElementById("updTitle")!;
const updBar = document.getElementById("updBar")!;
const updFill = document.getElementById("updFill") as HTMLElement;
const updPct = document.getElementById("updPct")!;
const updBytes = document.getElementById("updBytes")!;
const updNote = document.getElementById("updNote")!;

function fmtMB(bytes: number): string {
  return (bytes / (1024 * 1024)).toFixed(1) + " MB";
}

listen<{ phase: string; downloaded: number; total: number | null }>(
  "update://state",
  ({ payload }) => {
    if (payload.phase === "dismiss") {
      updOverlay.hidden = true;
      return;
    }
    updOverlay.hidden = false;

    if (payload.phase === "installing") {
      updTitle.textContent = "Installing…";
      updNote.textContent = "Rewiring the receiver — it will restart in a moment.";
      updBar.classList.add("indeterminate");
      updPct.textContent = "";
      updBytes.textContent = "";
      return;
    }

    // downloading
    updTitle.textContent = "Receiving update…";
    if (payload.total && payload.total > 0) {
      const pct = Math.min(100, (payload.downloaded / payload.total) * 100);
      updBar.classList.remove("indeterminate");
      updFill.style.width = pct.toFixed(1) + "%";
      updPct.textContent = Math.floor(pct) + "%";
      updBytes.textContent = fmtMB(payload.downloaded) + " / " + fmtMB(payload.total);
    } else {
      updBar.classList.add("indeterminate");
      updPct.textContent = "";
      updBytes.textContent = fmtMB(payload.downloaded);
    }
  },
);

function hostOf(u: string): string {
  try {
    return new URL(u).hostname;
  } catch {
    return "stream";
  }
}

// ---- dot-matrix display -----------------------------------------------------
//
// Two bands. The top one is always what you are hearing — station, state and
// the current track — and is never used for anything else. The bottom one is a
// context belt: it cycles through whichever of the next track, the programme
// and the DJ the station has actually told us about, and yields to transient
// notices (DJ on air, a station ident, an EQ change, a resync).
//
// Everything below reads from the decoded ICY payload, which the engine
// releases in step with the audio, so the countdown to the next track is
// honest rather than an estimate.

const STATION_FALLBACK = "LTBR·FM";
const ROTATE_MS = 8000;
const FLASH_MS = 3000;

let rotation = 0;
let rotateAt = 0;
let flashText = "";
let flashUntil = 0;

/** Take the secondary line for a few seconds, then let it resume rotating. */
function flashLine2(text: string) {
  flashText = text;
  flashUntil = Date.now() + FLASH_MS;
  renderDisplay();
}

function stationName(): string {
  return player.getStation()?.name || STATION_FALLBACK;
}

/** "ARTIST - TITLE", falling back to whichever half exists. */
function segmentLabel(seg: player.Segment): string {
  const artist = seg.artist?.trim();
  const title = seg.title?.trim();
  if (artist && title) return `${artist} - ${title}`;
  return title || artist || "";
}

/** m:ss, or "<1:00" style seconds for very short gaps. Track durations vary
 *  wildly — the station schedules idents only a few seconds long. */
function countdown(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  const m = Math.floor(total / 60);
  const sec = total % 60;
  return m > 0 ? `${m}:${String(sec).padStart(2, "0")}` : `${sec}S`;
}

/** Markers are on the station timeline, so "live" means the interpolated
 *  listener position falls inside them. */
function activeMarker<T extends { startMs: number; durationMs: number }>(
  markers: T[],
  now: number | null,
): T | undefined {
  if (now === null) return undefined;
  return markers.find((m) => now >= m.startMs && now < m.startMs + m.durationMs);
}

/** The rotating context belt. Returns the candidates that actually have data,
 *  so a station that sends no schedule simply shows fewer things rather than
 *  blank slots. */
function line2Candidates(): string[] {
  const meta = player.getMetadata();
  if (!meta) return [];
  const out: string[] = [];
  const now = player.timelineMs();

  const next = meta.next[0];
  if (next) {
    const label = segmentLabel(next);
    if (label) {
      const eta = now !== null ? next.startMs - now : null;
      out.push(
        eta !== null && eta > 0
          ? `NEXT · ${label} · IN ${countdown(eta)}`
          : `NEXT · ${label}`,
      );
    }
  } else if (meta.scheduleTruncated) {
    // Told nothing, rather than told there is nothing.
    out.push("NEXT · —");
  }

  const prog = meta.programme;
  if (prog) {
    const show = prog.showTitle?.trim() || prog.name?.trim();
    const dj = prog.dj?.trim();
    if (show && dj) out.push(`${show} · WITH ${dj}`);
    else if (show) out.push(show);
    else if (dj) out.push(`WITH ${dj}`);
  }
  return out;
}

/** Transient states that take the line immediately, ahead of the rotation. */
function line2Override(): string | null {
  if (Date.now() < flashUntil) return flashText;

  const meta = player.getMetadata();
  if (!meta) return null;
  const now = player.timelineMs();

  const talk = activeMarker(meta.talk, now);
  if (talk || meta.kind === "talk") {
    const dj = talk?.dj || meta.programme?.dj;
    return dj ? `DJ ON AIR · ${dj}` : "DJ ON AIR";
  }
  // Jingle names are raw asset slugs (LTBR_FM_All_Day_..._01), so they are
  // never shown verbatim.
  if (activeMarker(meta.jingles, now) || meta.kind === "jingle") {
    return "STATION IDENT";
  }
  return null;
}

function renderDisplay() {
  if (engineState === "live") {
    const meta = player.getMetadata();
    const station = stationName().toUpperCase();

    if (meta?.kind === "off") {
      setLine(0, `${station} · OFF AIR ·`);
      setLine(1, "");
      return;
    }

    const label = meta?.now ? segmentLabel(meta.now) : "";
    const heading = label || nowPlaying || hostOf(streamUrl.value);
    setLine(0, `${station} · ON AIR · ${heading.toUpperCase()} ·`);

    const override = line2Override();
    if (override) {
      setLine(1, override.toUpperCase() + " ·");
      return;
    }

    const belt = line2Candidates();
    if (belt.length === 0) {
      setLine(1, "");
      return;
    }
    const now = Date.now();
    if (now >= rotateAt) {
      rotateAt = now + ROTATE_MS;
      rotation++;
    }
    setLine(1, belt[rotation % belt.length].toUpperCase() + " ·");
    return;
  }

  if (engineState === "tuning") {
    setLine(0, `${stationName().toUpperCase()} · TUNING ·`);
    setLine(1, "STAND BY ·");
  } else if (engineState === "error") {
    setLine(0, `${stationName().toUpperCase()} · NO CARRIER ·`);
    setLine(1, "RETRYING ·");
  } else {
    setScroll("LTBR FM · LONDON TOWER BLOCK RADIO · PRESS PLAY ·");
    setLine(1, "");
  }
}

// The countdown and the rotation both move on their own, so the display is
// refreshed on a slow timer rather than only on engine events. One second is
// as fine-grained as a m:ss readout can show.
setInterval(() => {
  if (engineState === "live") renderDisplay();
}, 1000);

function applyState(s: typeof engineState, message?: string) {
  engineState = s;
  const playing = s === "live" || s === "tuning";
  setPlaying(s === "live");
  setTuning(s === "tuning");

  txState.classList.toggle("live", s === "live");
  txState.classList.toggle("tuning", s === "tuning");
  // The label always reads "Standby"; only the LED signals state
  // (solid red = idle, pulsing red = on air). An available update
  // overrides the block with the amber "New version" alert.
  refreshTx();

  btnPlay.setAttribute("aria-pressed", String(s === "live" || s === "tuning"));
  icoPlay.querySelector("path")!.setAttribute("d", playing ? PAUSE_PATH : PLAY_PATH);
  btnPlay.setAttribute("aria-label", playing ? "Pause" : "Play");

  if (s === "live") {
    metaVal.textContent = nowPlaying || hostOf(streamUrl.value);
  } else if (s === "tuning") {
    metaVal.textContent = message || "acquiring…";
  } else if (s === "error") {
    metaVal.textContent = "— no carrier —";
  } else {
    metaVal.textContent = message || "— no carrier —";
    clearSpectrum();
  }
  renderDisplay();
}

function fault(msg: string) {
  faultEl.textContent = msg || "";
}

function play() {
  fault("");
  const url = streamUrl.value.trim() || DEFAULT_URL;
  player.play(url); // emits "tuning" back through onState
}

function pause() {
  player.pause();
}

function stop() {
  player.stop();
}

btnPlay.addEventListener("click", () => {
  if (engineState === "live" || engineState === "tuning") pause();
  else play();
});
document.getElementById("btnStop")!.addEventListener("click", stop);

const btnMute = document.getElementById("btnMute")!;
btnMute.addEventListener("click", () => {
  player.setMuted(!player.getMuted());
});
player.onMuteChange((m) => {
  btnMute.setAttribute("aria-pressed", String(m));
});

// EQ show/hide — audio is untouched (the DSP keeps its settings); only the
// panel collapses, and the window re-fits to the new content height.
// Each face remembers its own choice across launches.
const btnEq = document.getElementById("btnEq")!;
const eqSection = document.querySelector<HTMLElement>(".eq")!;
let eqVisible = true;

const EQ_KEY = "ltbrfm.eq."; // + face id

function savedEqVisible(f: FaceId): boolean {
  try {
    return localStorage.getItem(EQ_KEY + f) !== "hidden";
  } catch {
    return true;
  }
}

function setEqVisible(v: boolean) {
  eqVisible = v;
  eqSection.classList.toggle("hidden", !v);
  btnEq.setAttribute("aria-pressed", String(v));
  try {
    localStorage.setItem(EQ_KEY + currentFace(), v ? "shown" : "hidden");
  } catch {
    /* private mode — the choice just won't persist */
  }
  requestAnimationFrame(() => {
    fitWindow().catch((e) => console.error("fitWindow failed:", e));
  });
}

btnEq.addEventListener("click", () => setEqVisible(!eqVisible));

// ---- minimised view ---------------------------------------------------------
// A compact "mini bar" for both faces: the scroller/spectrum window, the
// equaliser and the source row (default face) or the VU meters, tuning dial
// and tuning knob (vintage face) drop out of view, leaving a strip about a
// quarter of the full faceplate's footprint. Nothing about the engine
// changes — playback, mute, volume and any EQ settings already dialled in
// keep running; only the chrome is tucked away.
//
// Mini is chosen from the context menu's face list rather than from a key on
// the faceplate, so it reads as the third face it effectively is — and the
// fascias stay free of a control that is really about the window, not the
// radio. It keeps the *base* face's styling (the mini strip looks quite
// different on the rack unit and the vintage receiver), so the choice is a
// single global preference rather than per-face state: picking Receiver or
// Vintage from the same menu expands straight back to that face.
let minimized = false;
// The width to restore when expanding again — captured the moment we shrink,
// since the window is otherwise always the width the current face settled on.
let normalWidth: number | null = null;

const MINI_KEY = "ltbrfm.mini";
const MINI_WIDTH = 320;

function savedMinimized(f: FaceId): boolean {
  try {
    const v = localStorage.getItem(MINI_KEY);
    if (v !== null) return v === "1";
    // Migrate the old per-face key, so anyone who left the previous build
    // minimised comes back minimised rather than unexpectedly expanded.
    return localStorage.getItem(MINI_KEY + "." + f) === "1";
  } catch {
    return false;
  }
}

function setMinimized(v: boolean) {
  if (v && !minimized) normalWidth = window.innerWidth;
  minimized = v;
  document.body.classList.toggle("mini", v);
  refreshFaceChecks(currentFace());
  try {
    localStorage.setItem(MINI_KEY, v ? "1" : "0");
  } catch {
    /* private mode — the choice just won't persist */
  }
  requestAnimationFrame(() => {
    fitWindow().catch((e) => console.error("fitWindow failed:", e));
  });
}

document.getElementById("btnTune")!.addEventListener("click", () => {
  const url = streamUrl.value.trim();
  if (!url) {
    fault("Enter a stream URL first.");
    return;
  }
  play();
});

// Enter in the URL box tunes.
streamUrl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    play();
  }
});

// Keyboard: space toggles, M mutes — but not while typing or on a slider.
// Space transport is a default-face affordance; on the vintage face the
// power key and tuning knob own the audio lifecycle.
document.addEventListener("keydown", (e) => {
  const t = e.target as HTMLElement;
  if (t.matches("input, [role=slider]")) return;
  if (e.code === "Space" && currentFace() === "default") {
    e.preventDefault();
    if (engineState === "live" || engineState === "tuning") pause();
    else play();
  }
  if (e.key.toLowerCase() === "m") (btnMute as HTMLButtonElement).click();
  if (e.key.toLowerCase() === "e") (btnEq as HTMLButtonElement).click();
});

// ---- frameless window: power off + drag regions ----------------------------

// Power key: ramp the audio down (the engine's smoothed stop avoids a pop),
// then close the window, which exits the app.
document.getElementById("btnPower")!.addEventListener("click", () => {
  cmd("stop");
  setTimeout(() => {
    getCurrentWindow()
      .close()
      .catch(() => window.close());
  }, 150);
});

// The window has no titlebar, so the faceplate itself is the drag handle.
// data-tauri-drag-region only fires when the clicked element ITSELF carries
// the attribute, so interactive children (buttons, faders, input) stay live.
const DRAG_REGIONS = [
  ".unit", ".face", ".brand-row", ".brand", ".strap",
  ".windows", ".window", "canvas",
  ".transport", ".keys", ".meta", ".meta .lbl", ".meta .val", ".vol", ".vol .lbl",
  ".eq", ".eq-head", ".eq-head .title", ".presets", ".bands", ".band",
  ".band .hz", ".band .db", ".rule",
  ".source", ".source label", ".fault", ".tx", ".screw",
];
for (const sel of DRAG_REGIONS) {
  document.querySelectorAll<HTMLElement>(sel).forEach((el) => {
    el.setAttribute("data-tauri-drag-region", "");
  });
}

// ---- context menu: window options ------------------------------------------

// Right-click anywhere on the faceplate opens a small window-options menu.
// Text inputs keep the webview's native menu (cut/copy/paste).
const ctxMenu = document.getElementById("ctxMenu")!;
const ctxOnTop = document.getElementById("ctxOnTop")!;
let alwaysOnTop = false;

function closeCtxMenu() {
  ctxMenu.classList.remove("open");
}

function openCtxMenu(cx: number, cy: number) {
  ctxMenu.classList.add("open");
  // Clamp so the menu never opens past the window edge.
  const x = Math.min(cx, window.innerWidth - ctxMenu.offsetWidth - 6);
  const y = Math.min(cy, window.innerHeight - ctxMenu.offsetHeight - 6);
  ctxMenu.style.left = Math.max(6, x) + "px";
  ctxMenu.style.top = Math.max(6, y) + "px";
  (ctxMenu.querySelector("button") as HTMLButtonElement).focus();
}

document.addEventListener("contextmenu", (e) => {
  if ((e.target as HTMLElement).closest("input")) return;
  e.preventDefault();
  // Always use our own HTML menu: it carries app-level items (face
  // selection) that a window manager's menu can never show.
  openCtxMenu(e.clientX, e.clientY);
});

document.addEventListener("pointerdown", (e) => {
  if (!ctxMenu.contains(e.target as Node)) closeCtxMenu();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeCtxMenu();
});
window.addEventListener("blur", closeCtxMenu);

// Note: on Linux/Wayland compositors ignore the keep-above request (no
// protocol for it); the toggle still reflects what was asked of the OS.
ctxOnTop.addEventListener("click", () => {
  alwaysOnTop = !alwaysOnTop;
  ctxOnTop.setAttribute("aria-checked", String(alwaysOnTop));
  getCurrentWindow()
    .setAlwaysOnTop(alwaysOnTop)
    .catch((e) => console.error("setAlwaysOnTop failed:", e));
  closeCtxMenu();
});

// ---- context menu: faces ----------------------------------------------------

const ctxFaceDefault = document.getElementById("ctxFaceDefault")!;
const ctxFaceVintage = document.getElementById("ctxFaceVintage")!;
const ctxFaceMini = document.getElementById("ctxFaceMini")!;

// One radio group of three. Mini is not a base face of its own — it wears
// whichever fascia is underneath — so it simply wins the tick while it is on.
function refreshFaceChecks(f: FaceId) {
  ctxFaceDefault.setAttribute("aria-checked", String(f === "default" && !minimized));
  ctxFaceVintage.setAttribute("aria-checked", String(f === "vintage" && !minimized));
  ctxFaceMini.setAttribute("aria-checked", String(minimized));
}

/** Picking a full face expands, so the three items behave as one choice. */
function chooseFace(f: FaceId) {
  setMinimized(false);
  setFace(f);
  closeCtxMenu();
}

ctxFaceDefault.addEventListener("click", () => chooseFace("default"));
ctxFaceVintage.addEventListener("click", () => chooseFace("vintage"));
ctxFaceMini.addEventListener("click", () => {
  setMinimized(true);
  closeCtxMenu();
});

// ---- context menu: exit + version -------------------------------------------

document.getElementById("ctxExit")!.addEventListener("click", () => {
  closeCtxMenu();
  invoke("quit").catch((e) => console.error("quit failed:", e));
});

const ctxVersion = document.getElementById("ctxVersion")!;
getVersion()
  .then((v) => {
    ctxVersion.textContent = "v" + v;
  })
  .catch(() => {
    ctxVersion.textContent = "";
  });

onFaceChange((f) => {
  refreshFaceChecks(f);
  // each face carries its own remembered EQ visibility
  setEqVisible(savedEqVisible(f));
  // returning to the default face: reflect the shared player state in its
  // controls (the vintage knob may have moved the volume meanwhile)
  if (f === "default") {
    volFader.set(player.getUserVolume() * 100);
  }
  requestAnimationFrame(() => {
    fitWindow().catch((e) => console.error("fitWindow failed:", e));
  });
});

// ---- engine events ---------------------------------------------------------

player.onState((s, message) => {
  nowPlaying = player.getNowPlaying();
  applyState(s, message);
});

player.onNowPlaying((title) => {
  nowPlaying = title;
  if (engineState === "live") applyState("live");
});

player.onMetadata(() => {
  // A fresh block can change either line — a new track, a new next-up, a DJ
  // coming on air — so recompose rather than waiting for the tick.
  if (engineState === "live") renderDisplay();
});

player.onSync((s) => {
  if (s.action === "catchup") flashLine2("RESYNC · CATCHING UP");
  else if (s.action === "reconnect") flashLine2("RESYNC · RETUNING");
});

player.onFault((m) => fault(m));

player.onSpectrum((bars) => setSpectrum(bars));

// ---- boot ------------------------------------------------------------------

// Fit the window to the faceplate's natural height. Font metrics differ per
// platform (WebKitGTK renders text more compactly than macOS/Windows), so a
// hard-coded height leaves a bottom margin on some systems and clips on
// others. Measure the real content and size the window to it exactly.
//
// Measuring is only trustworthy once the bundled fonts are loaded AND the
// webview viewport has caught up with the *target* width — at boot (notably
// macOS/WKWebView) the first frames render before either is true, and a
// measurement taken then sees a wrapped layout and locks in a squeezed
// window. The same trap bites when toggling the mini bar: reading content
// height right after changing width class(es) still reflects the OLD
// viewport width (the native window hasn't resized yet), so it sees a
// wrapped/inflated layout unless we wait for the viewport to actually
// reach the target width first.
async function fitWindow(attempt = 0) {
  const face = document.querySelector(".face") as HTMLElement;
  if (!face) return;
  await document.fonts.ready;
  const win = getCurrentWindow();

  // The width we're aiming for: a fixed compact bar while minimised, the
  // previously-captured full width when restoring it, or (first boot, never
  // having been minimised) whatever the OS already gave the window.
  const desiredWidth = minimized
    ? MINI_WIDTH
    : normalWidth ?? (await win.innerSize()).toLogical(await win.scaleFactor()).width;

  if (Math.abs(window.innerWidth - desiredWidth) > 1) {
    if (attempt === 0) {
      // Command the width now; height is corrected in a follow-up pass
      // once the viewport has actually caught up — measuring at the old
      // width would see a wrapped, artificially tall layout.
      await win.setSize(new LogicalSize(desiredWidth, window.innerHeight));
    }
    // Viewport not settled yet — retry for up to ~1s, else leave the
    // configured size alone rather than resize from a bad measurement.
    if (attempt < 60) {
      requestAnimationFrame(() => {
        fitWindow(attempt + 1).catch((e) => console.error("fitWindow failed:", e));
      });
    }
    return;
  }

  // Measure whichever face is active: the default fascia plus the unit
  // border, or the vintage receiver's full case.
  const target = currentFace() === "vintage"
    ? Math.ceil(document.getElementById("faceVintage")!.offsetHeight)
    : Math.ceil(face.offsetHeight) + 2; // + unit border
  if (Math.abs(window.innerHeight - target) > 1) {
    await win.setSize(new LogicalSize(desiredWidth, target));
  }
}

player.initPlayer();
initVisuals();
initVintage();
setFace(savedFace());
setMinimized(savedMinimized(currentFace()));
applyState("standby");
requestAnimationFrame(() => {
  fitWindow().catch((e) => console.error("fitWindow failed:", e));
});
