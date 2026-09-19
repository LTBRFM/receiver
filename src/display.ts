// ---------------------------------------------------------------------------
// display.ts — what the displays say.
//
// One place composes the words every face shows: the station name, the line
// for what is being heard right now (track, DJ on air, ident, or a transient
// notice), the next-up line, and the short form of a fault. Faces only decide
// how to draw them — the dot-matrix, the blue LCD, a pilot lamp.
//
// Everything below reads from the decoded ICY payload, which the engine
// releases in step with the audio.
// ---------------------------------------------------------------------------

import * as player from "./player.ts";

function hostOf(u: string): string {
  try {
    return new URL(u).hostname;
  } catch {
    return "stream";
  }
}

export const STATION_FALLBACK = "LONDON TOWER BLOCK RADIO";
const FLASH_MS = 3000;

/** A fault code as a couple of dot-matrix words. */
export function faultLabel(f: player.Fault | null): string {
  switch (f?.code) {
    case "device": return "NO AUDIO DEVICE";
    case "connect":
    case "timeout":
    case "network": return "NO CONNECTION";
    case "http": return "STREAM OFFLINE";
    case "decode": return "BAD SIGNAL";
    case "dropped": return "SIGNAL LOST";
    default: return f ? "FAULT" : "";
  }
}

let flashText = "";
let flashUntil = 0;

/** Take the current-track line for a few seconds, then let it resume. */
export function flash(text: string) {
  flashText = text;
  flashUntil = Date.now() + FLASH_MS;
}

/** The notice currently taking over the current-track line, if any. */
export function activeFlash(): string | null {
  return Date.now() < flashUntil ? flashText : null;
}

export function stationName(): string {
  return player.getStation()?.name || STATION_FALLBACK;
}

/** "ARTIST - TITLE", falling back to whichever half exists. */
function segmentLabel(seg: player.Segment): string {
  const artist = seg.artist?.trim();
  const title = seg.title?.trim();
  if (artist && title) return `${artist} - ${title}`;
  return title || artist || "";
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

/** What you are hearing right now: a transient notice if one applies, else
 *  the current track, else whatever fallback title we last saw. */
export function currentLine(): string {
  if (Date.now() < flashUntil) return flashText;

  const meta = player.getMetadata();
  if (meta) {
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
    const label = meta.now ? segmentLabel(meta.now) : "";
    if (label) return label;
  }
  return player.getNowPlaying() || hostOf(player.currentUrl());
}

/** "NEXT: artist - track", or a placeholder when told nothing, or empty when
 *  the station simply has no schedule data. No countdown here on purpose —
 *  a timer changes every second, which would keep resetting the scroll. */
export function nextLine(): string {
  const meta = player.getMetadata();
  if (!meta) return "";

  const next = meta.next[0];
  if (next) {
    const label = segmentLabel(next);
    if (label) return `NEXT: ${label}`;
  } else if (meta.scheduleTruncated) {
    // Told nothing, rather than told there is nothing.
    return "NEXT: —";
  }
  return "";
}

