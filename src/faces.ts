// ---------------------------------------------------------------------------
// faces.ts — face (skin) registry and switching.
//
// Two faces share one window: the default rack unit and the vintage 80s
// receiver. The inactive face's root is hidden with [hidden]; a body class
// lets CSS restyle shared, reparented sections (the graphic equaliser).
// The choice persists across launches.
// ---------------------------------------------------------------------------

export type FaceId = "default" | "vintage";

const KEY = "ltbrfm.face";
const faceCbs: ((f: FaceId) => void)[] = [];
let current: FaceId = "default";

export function currentFace(): FaceId {
  return current;
}

export function onFaceChange(cb: (f: FaceId) => void) {
  faceCbs.push(cb);
}

export function setFace(f: FaceId) {
  current = f;
  document.body.classList.toggle("face-vintage", f === "vintage");
  document.getElementById("faceDefault")!.toggleAttribute("hidden", f !== "default");
  document.getElementById("faceVintage")!.toggleAttribute("hidden", f !== "vintage");
  try {
    localStorage.setItem(KEY, f);
  } catch {
    /* private mode — the choice just won't persist */
  }
  for (const cb of faceCbs) cb(f);
}

export function savedFace(): FaceId {
  try {
    const saved = localStorage.getItem(KEY);
    if (saved === "default" || saved === "vintage") return saved;
    // No stored preference yet — new users land on the Vintage 80s face.
    return "vintage";
  } catch {
    return "vintage";
  }
}
