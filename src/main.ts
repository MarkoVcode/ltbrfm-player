// ---------------------------------------------------------------------------
// main.ts — UI wiring for the LTBR·FM Receiver.
//
// All audio (decode, EQ, volume, spectrum) lives in the Rust engine. This
// module renders the controls and translates user intent into IPC commands,
// and reflects engine events back into the display.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  initVisuals,
  setScroll,
  setSpectrum,
  setPlaying,
  setTuning,
  clearSpectrum,
} from "./visuals.ts";

const FREQS = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
const MAX_DB = 12;

const PRESETS: Record<string, number[]> = {
  flat:   [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  pirate: [4, 5, 2, -1, -2, 0, 2, 4, 5, 3], // scooped mids, hyped top — cassette-dub feel
  bass:   [8, 7, 5, 2, 0, 0, 0, 0, 1, 2],
  voice:  [-4, -3, 0, 3, 5, 5, 3, 1, -1, -2],
};

const DEFAULT_URL = "https://stream.ltbr.fm/live";

// Fire-and-forget command helper — the engine is authoritative, so a failed
// command must never take down the UI.
function cmd(name: string, args?: Record<string, unknown>): void {
  invoke(name, args).catch((e) => console.error(`cmd ${name} failed:`, e));
}

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

let muted = false;

makeFader(document.getElementById("volFader")!, {
  min: 0, max: 100, value: 80, vertical: false,
  format: (n) => Math.round(n) + "%",
  onChange: (n) => {
    cmd("set_volume", { level: n / 100 });
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
    setScroll("EQ · " + btn.textContent!.toUpperCase());
  });
});

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

function hostOf(u: string): string {
  try {
    return new URL(u).hostname;
  } catch {
    return "stream";
  }
}

function applyState(s: typeof engineState, message?: string) {
  engineState = s;
  const playing = s === "live" || s === "tuning";
  setPlaying(s === "live");
  setTuning(s === "tuning");

  txState.classList.toggle("live", s === "live");
  txState.classList.toggle("tuning", s === "tuning");
  // The label always reads "Standby"; only the LED signals state
  // (solid red = idle, pulsing red = on air).
  txLabel.textContent = "Standby";

  btnPlay.setAttribute("aria-pressed", String(s === "live" || s === "tuning"));
  icoPlay.querySelector("path")!.setAttribute("d", playing ? PAUSE_PATH : PLAY_PATH);
  btnPlay.setAttribute("aria-label", playing ? "Pause" : "Play");

  if (s === "live") {
    metaVal.textContent = nowPlaying || hostOf(streamUrl.value);
    setScroll(
      nowPlaying
        ? "LTBR FM · ON AIR · " + nowPlaying.toUpperCase() + " ·"
        : "LTBR FM · ON AIR · " + hostOf(streamUrl.value).toUpperCase() + " ·",
    );
  } else if (s === "tuning") {
    metaVal.textContent = message || "acquiring…";
    setScroll("LTBR FM · TUNING · STAND BY ·");
  } else if (s === "error") {
    metaVal.textContent = "— no carrier —";
  } else {
    metaVal.textContent = message || "— no carrier —";
    setScroll("LTBR FM · LONDON TOWER BLOCK RADIO · PRESS PLAY ·");
    clearSpectrum();
  }
}

function fault(msg: string) {
  faultEl.textContent = msg || "";
}

function play() {
  fault("");
  const url = streamUrl.value.trim() || DEFAULT_URL;
  applyState("tuning", "acquiring…");
  cmd("play", { url });
}

function pause() {
  cmd("pause");
  applyState("standby", "paused");
}

function stop() {
  cmd("stop");
  nowPlaying = "";
  applyState("standby");
}

btnPlay.addEventListener("click", () => {
  if (engineState === "live" || engineState === "tuning") pause();
  else play();
});
document.getElementById("btnStop")!.addEventListener("click", stop);

const btnMute = document.getElementById("btnMute")!;
btnMute.addEventListener("click", () => {
  muted = !muted;
  btnMute.setAttribute("aria-pressed", String(muted));
  cmd("set_mute", { muted });
});

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
document.addEventListener("keydown", (e) => {
  const t = e.target as HTMLElement;
  if (t.matches("input, [role=slider]")) return;
  if (e.code === "Space") {
    e.preventDefault();
    if (engineState === "live" || engineState === "tuning") pause();
    else play();
  }
  if (e.key.toLowerCase() === "m") (btnMute as HTMLButtonElement).click();
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

// ---- engine events ---------------------------------------------------------

interface StateEvent {
  state: "standby" | "tuning" | "live" | "error";
  message?: string;
}

listen<StateEvent>("state", (e) => {
  applyState(e.payload.state, e.payload.message);
  if (e.payload.state === "error" && e.payload.message) fault(e.payload.message);
  else if (e.payload.state !== "error") fault("");
});

listen<{ title: string }>("nowplaying", (e) => {
  nowPlaying = e.payload.title || "";
  if (engineState === "live") applyState("live");
});

listen<{ message: string }>("fault", (e) => fault(e.payload.message));

listen<number[]>("spectrum", (e) => setSpectrum(e.payload));

// ---- boot ------------------------------------------------------------------

initVisuals();
applyState("standby");
