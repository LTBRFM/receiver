// ---------------------------------------------------------------------------
// vu.ts — analogue VU meter for the vintage face.
//
// A cream meter face with a black needle behind glass, in the idiom of the
// moving-coil meters on late-70s/80s British receivers. The face (scale arc,
// tick marks, red zone, legend) is pre-rendered; per frame we draw only the
// needle and the glass shading. Needle ballistics use the classic VU
// standard: ~300 ms integration with a touch of overshoot, implemented as a
// damped spring.
// ---------------------------------------------------------------------------

const ANGLE_SPAN = 84; // degrees, total sweep
const A_MIN = (-ANGLE_SPAN / 2) * (Math.PI / 180);
const A_MAX = (ANGLE_SPAN / 2) * (Math.PI / 180);

// VU scale stops, as fractions of the sweep, with printed labels.
const STOPS: [number, string][] = [
  [0.0, "-20"],
  [0.16, "-10"],
  [0.32, "-7"],
  [0.5, "-5"],
  [0.68, "-3"],
  [0.8, "0"],
  [1.0, "+3"],
];
const RED_FROM = 0.8; // red zone starts at 0 VU

export class VUMeter {
  private cv: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private face: HTMLCanvasElement | null = null;
  private w = 0;
  private h = 0;
  private label: string;

  // spring state
  private pos = 0; // 0..1 along the sweep
  private vel = 0;
  private target = 0;

  constructor(canvas: HTMLCanvasElement, label: string) {
    this.cv = canvas;
    this.ctx = canvas.getContext("2d")!;
    this.label = label;
    this.fit();
  }

  fit(force = false) {
    const dpr = window.devicePixelRatio || 1;
    const w = this.cv.clientWidth || 128;
    const h = this.cv.clientHeight || 78;
    if (!force && w === this.w && h === this.h && this.face) return;
    this.w = w;
    this.h = h;
    this.cv.width = Math.round(w * dpr);
    this.cv.height = Math.round(h * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.face = this.renderFace(w, h, dpr);
  }

  /** Meter drive 0..1 (post-ballistics target). */
  setLevel(v: number) {
    this.target = Math.max(0, Math.min(1, v));
  }

  private pivot() {
    // pivot sits below the visible face so the needle swings in a shallow arc
    return { px: this.w / 2, py: this.h * 1.62, r: this.h * 1.38 };
  }

  private renderFace(w: number, h: number, dpr: number): HTMLCanvasElement {
    const cv = document.createElement("canvas");
    cv.width = Math.round(w * dpr);
    cv.height = Math.round(h * dpr);
    const c = cv.getContext("2d")!;
    c.setTransform(dpr, 0, 0, dpr, 0, 0);
    const { px, py, r } = this.pivot();
    // Scale factor against the meter's default size (128x78), clamped so
    // legends stay readable at both the full and the shrunk (mini-view)
    // dimensions.
    const s = Math.max(0.42, Math.min(w / 128, h / 78));
    // The channel name ("LEFT"/"RIGHT") only fits above a certain size —
    // below it, drop straight to just "VU" to save space.
    const showChannel = h >= 55;

    // aged-cream face with a soft vignette
    const bg = c.createRadialGradient(w / 2, h * 0.3, 6, w / 2, h * 0.6, w * 0.75);
    bg.addColorStop(0, "#efe4c8");
    bg.addColorStop(0.75, "#e3d5b4");
    bg.addColorStop(1, "#cdbc97");
    c.fillStyle = bg;
    c.fillRect(0, 0, w, h);

    const angOf = (t: number) => A_MIN + t * (A_MAX - A_MIN);

    // scale arc
    c.lineWidth = 1.6;
    c.strokeStyle = "#2b241a";
    c.beginPath();
    c.arc(px, py, r * 0.72, angOf(0) - Math.PI / 2, angOf(RED_FROM) - Math.PI / 2);
    c.stroke();
    c.strokeStyle = "#b3271e";
    c.lineWidth = 2.4;
    c.beginPath();
    c.arc(px, py, r * 0.72, angOf(RED_FROM) - Math.PI / 2, angOf(1) - Math.PI / 2);
    c.stroke();

    // ticks + labels
    c.font = `700 ${Math.max(6, Math.round(7 * s))}px 'Inter', sans-serif`;
    c.textAlign = "center";
    for (const [t, lbl] of STOPS) {
      const a = angOf(t) - Math.PI / 2;
      const red = t >= RED_FROM;
      c.strokeStyle = red ? "#b3271e" : "#2b241a";
      c.fillStyle = red ? "#b3271e" : "#2b241a";
      c.lineWidth = 1.2;
      const x1 = px + Math.cos(a) * r * 0.72;
      const y1 = py + Math.sin(a) * r * 0.72;
      const x2 = px + Math.cos(a) * r * 0.79;
      const y2 = py + Math.sin(a) * r * 0.79;
      c.beginPath();
      c.moveTo(x1, y1);
      c.lineTo(x2, y2);
      c.stroke();
      c.fillText(lbl, px + Math.cos(a) * r * 0.85, py + Math.sin(a) * r * 0.85 + 2);
    }
    // minor ticks
    c.strokeStyle = "rgba(43,36,26,0.5)";
    c.lineWidth = 0.8;
    for (let i = 0; i <= 20; i++) {
      const a = angOf(i / 20) - Math.PI / 2;
      c.beginPath();
      c.moveTo(px + Math.cos(a) * r * 0.72, py + Math.sin(a) * r * 0.72);
      c.lineTo(px + Math.cos(a) * r * 0.755, py + Math.sin(a) * r * 0.755);
      c.stroke();
    }

    // legend — channel name stacked over "VU", centred in the free area
    // between the scale arc and the bottom edge. Below a certain size the
    // channel name is dropped (kept to just "VU") so it doesn't crowd or
    // overrun the shrunk meter face used in the minimised view.
    c.fillStyle = "#2b241a";
    if (showChannel) {
      c.font = `700 ${Math.max(6, Math.round(7 * s))}px 'Inter', sans-serif`;
      c.fillText(this.label, w / 2, h - 17 * s);
      c.font = `800 ${Math.max(7, Math.round(9 * s))}px 'Inter', sans-serif`;
      c.fillText("VU", w / 2, h - 7 * s);
    } else {
      c.font = `800 ${Math.max(7, Math.round(9 * s))}px 'Inter', sans-serif`;
      c.fillText("VU", w / 2, h - 6 * s);
    }

    return cv;
  }

  /** Advance ballistics and repaint. dt in ms; lamp = backlight 0..1. */
  draw(dt: number, lamp = 1) {
    const { ctx: c, w, h } = this;
    if (!this.face) return;

    // damped spring ≈ 300ms VU integration with slight overshoot
    const dts = Math.min(0.05, dt / 1000);
    const K = 190; // stiffness
    const D = 21; // damping (slightly under-critical → overshoot)
    this.vel += (K * (this.target - this.pos) - D * this.vel) * dts;
    this.pos += this.vel * dts;
    if (this.pos < 0) { this.pos = 0; this.vel = 0; }
    if (this.pos > 1.04) { this.pos = 1.04; this.vel = -this.vel * 0.3; }

    c.clearRect(0, 0, w, h);
    c.drawImage(this.face, 0, 0, w, h);

    // warm backlight pooling behind the lower half of the face, following the
    // power lamp so the meter goes dark with the set switched off
    if (lamp > 0.01) {
      const lampGlow = c.createRadialGradient(w / 2, h * 0.72, 3, w / 2, h * 0.62, w * 0.55);
      lampGlow.addColorStop(0, `rgba(255,184,80,${0.32 * lamp})`);
      lampGlow.addColorStop(0.6, `rgba(255,184,80,${0.10 * lamp})`);
      lampGlow.addColorStop(1, "rgba(255,184,80,0)");
      c.fillStyle = lampGlow;
      c.fillRect(0, 0, w, h);
    }

    // needle
    const { px, py, r } = this.pivot();
    const a = A_MIN + Math.min(1, this.pos) * (A_MAX - A_MIN) - Math.PI / 2;
    c.save();
    // needle shadow (offset, soft)
    c.strokeStyle = "rgba(30,24,16,0.25)";
    c.lineWidth = 1.6;
    c.beginPath();
    c.moveTo(px + Math.cos(a) * r * 0.18 + 1.5, py + Math.sin(a) * r * 0.18 + 2);
    c.lineTo(px + Math.cos(a) * r * 0.8 + 1.5, py + Math.sin(a) * r * 0.8 + 2);
    c.stroke();
    // needle
    c.strokeStyle = "#1c1712";
    c.lineWidth = 1.4;
    c.beginPath();
    c.moveTo(px + Math.cos(a) * r * 0.18, py + Math.sin(a) * r * 0.18);
    c.lineTo(px + Math.cos(a) * r * 0.8, py + Math.sin(a) * r * 0.8);
    c.stroke();
    c.restore();

    // unlit shade: with the lamp off the face reads by room light only,
    // sinking into the same murk as the frequency dial
    if (lamp < 0.99) {
      c.fillStyle = `rgba(16,12,7,${0.62 * (1 - lamp)})`;
      c.fillRect(0, 0, w, h);
    }

    // ---- domed glass, plugin-style ----
    // 1) bottom vignette: the face curves away from the light
    const vg = c.createRadialGradient(w / 2, h * 0.35, h * 0.3, w / 2, h * 0.55, w * 0.75);
    vg.addColorStop(0, "rgba(0,0,0,0)");
    vg.addColorStop(0.75, "rgba(25,17,9,0.12)");
    vg.addColorStop(1, "rgba(25,17,9,0.34)");
    c.fillStyle = vg;
    c.fillRect(0, 0, w, h);

    // 2) curved glare sheet: a big ellipse dipping into the upper half — its
    //    lower boundary is the visible "edge" of the reflection on the dome
    c.save();
    c.beginPath();
    c.ellipse(w * 0.42, -h * 0.70, w * 0.80, h * 1.24, -0.10, 0, Math.PI * 2);
    c.clip();
    const sheen = c.createLinearGradient(0, 0, 0, h * 0.60);
    sheen.addColorStop(0, "rgba(255,255,255,0.30)");
    sheen.addColorStop(0.55, "rgba(255,255,255,0.13)");
    sheen.addColorStop(1, "rgba(255,255,255,0.09)");
    c.fillStyle = sheen;
    c.fillRect(0, 0, w, h);
    c.restore();

    // 3) hotspot blob where the dome faces the room light
    const hl = c.createRadialGradient(w * 0.28, h * 0.10, 2, w * 0.33, h * 0.20, w * 0.40);
    hl.addColorStop(0, "rgba(255,255,255,0.30)");
    hl.addColorStop(0.35, "rgba(255,255,255,0.10)");
    hl.addColorStop(1, "rgba(255,255,255,0)");
    c.fillStyle = hl;
    c.fillRect(0, 0, w, h);

    // 4) faint secondary hotspot, lower-right, as if bounced light
    const hl2 = c.createRadialGradient(w * 0.80, h * 0.82, 1, w * 0.80, h * 0.82, w * 0.22);
    hl2.addColorStop(0, "rgba(255,255,255,0.09)");
    hl2.addColorStop(1, "rgba(255,255,255,0)");
    c.fillStyle = hl2;
    c.fillRect(0, 0, w, h);

    // 5) corner shading pinches the glass into its bezel
    const edge = c.createRadialGradient(w / 2, h / 2, Math.min(w, h) * 0.42, w / 2, h / 2, w * 0.72);
    edge.addColorStop(0, "rgba(0,0,0,0)");
    edge.addColorStop(1, "rgba(15,10,6,0.30)");
    c.fillStyle = edge;
    c.fillRect(0, 0, w, h);
  }
}
