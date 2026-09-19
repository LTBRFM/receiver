// ---------------------------------------------------------------------------
// fader.ts — the pointer/keyboard slider behind every fader on every face.
//
// The element carries a `.cap` child that is positioned along the track;
// faces style the track and cap however they like. Vertical faders put the
// maximum at the top.
// ---------------------------------------------------------------------------

export interface FaderOpts {
  min: number;
  max: number;
  value: number;
  vertical: boolean;
  onChange: (v: number) => void;
  format?: (v: number) => string;
}

export function makeFader(el: HTMLElement, opts: FaderOpts) {
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

