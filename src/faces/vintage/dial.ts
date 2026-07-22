// ---------------------------------------------------------------------------
// dial.ts — backlit tuning dial glass for the vintage face.
//
// The printed scale (rules, MHz numerals, station name) is rasterised once
// into an offscreen layer; per frame we compose backlight glow + print +
// needle, all devicePixelRatio-aware. The lamp brightness follows the power
// state so switching on feels like a filament warming up.
// ---------------------------------------------------------------------------

export const F_MIN = 87.5;
export const F_MAX = 108.5;
export const STATION_FREQ = 101.5;
export const STATION_NAME = "LTBR.FM";

const PAD = 26; // horizontal glass margin before the scale starts

export class Dial {
  private cv: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private print: HTMLCanvasElement | null = null;
  private beaconX = 0;
  private w = 0;
  private h = 0;

  constructor(canvas: HTMLCanvasElement) {
    this.cv = canvas;
    this.ctx = canvas.getContext("2d")!;
    this.fit();
  }

  fit(force = false) {
    const dpr = window.devicePixelRatio || 1;
    const w = this.cv.clientWidth || 640;
    const h = this.cv.clientHeight || 96;
    if (!force && w === this.w && h === this.h && this.print) return;
    this.w = w;
    this.h = h;
    this.cv.width = Math.round(w * dpr);
    this.cv.height = Math.round(h * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.print = this.renderPrint(w, h, dpr);
  }

  freqToX(f: number): number {
    const t = (f - F_MIN) / (F_MAX - F_MIN);
    return PAD + t * (this.w - PAD * 2);
  }

  // The dial print: white/amber ink on the inside of the glass.
  private renderPrint(w: number, h: number, dpr: number): HTMLCanvasElement {
    const cv = document.createElement("canvas");
    cv.width = Math.round(w * dpr);
    cv.height = Math.round(h * dpr);
    const c = cv.getContext("2d")!;
    c.setTransform(dpr, 0, 0, dpr, 0, 0);

    const yRule = h * 0.62; // main horizontal rule
    const ink = "rgba(240,230,210,0.92)";
    const inkDim = "rgba(240,230,210,0.55)";

    c.strokeStyle = inkDim;
    c.lineWidth = 1;
    c.beginPath();
    c.moveTo(PAD - 8, yRule);
    c.lineTo(w - PAD + 8, yRule);
    c.stroke();

    // ticks: minor each 0.5 MHz, major each 1, numbered each 2
    for (let f = 88; f <= 108; f += 0.5) {
      const x = PAD + ((f - F_MIN) / (F_MAX - F_MIN)) * (w - PAD * 2);
      const major = Number.isInteger(f);
      const numbered = major && f % 2 === 0;
      c.strokeStyle = numbered ? ink : inkDim;
      c.lineWidth = numbered ? 1.4 : 1;
      c.beginPath();
      c.moveTo(x, yRule);
      c.lineTo(x, yRule - (numbered ? 13 : major ? 9 : 5));
      c.stroke();
      if (numbered) {
        c.fillStyle = ink;
        c.font = "600 11px 'Inter', sans-serif";
        c.textAlign = "center";
        c.fillText(String(f), x, yRule - 18);
      }
    }

    // band legend
    c.fillStyle = inkDim;
    c.font = "600 8px 'Inter', sans-serif";
    c.textAlign = "left";
    c.fillText("FM  MHz", PAD - 8, h - 9);
    c.textAlign = "right";
    c.fillText("AFC  ·  STEREO", w - PAD + 8, h - 9);
    // the stereo beacon sits clear to the LEFT of the legend, with a gap
    this.beaconX = w - PAD + 8 - c.measureText("AFC  ·  STEREO").width - 12;

    // station marker: amber lozenge + name above the scale
    const sx = PAD + ((STATION_FREQ - F_MIN) / (F_MAX - F_MIN)) * (w - PAD * 2);
    c.fillStyle = "rgba(255,170,60,0.95)";
    c.beginPath();
    c.moveTo(sx, yRule + 4);
    c.lineTo(sx - 4, yRule + 10);
    c.lineTo(sx + 4, yRule + 10);
    c.closePath();
    c.fill();
    c.font = "800 10px 'Inter', sans-serif";
    c.textAlign = "center";
    c.fillText(STATION_NAME, sx, yRule + 22);

    return cv;
  }

  /**
   * @param freq   needle frequency (MHz)
   * @param lamp   backlight brightness 0..1 (power ramp)
   * @param signal carrier lock 0..1 (lights the STEREO beacon)
   */
  draw(freq: number, lamp: number, signal: number) {
    const { ctx: c, w, h } = this;
    c.clearRect(0, 0, w, h);

    // glass body — near-black when off, warm amber wash when lit
    const g = c.createLinearGradient(0, 0, 0, h);
    g.addColorStop(0, `rgba(${18 + 30 * lamp},${14 + 20 * lamp},${8 + 8 * lamp},1)`);
    g.addColorStop(0.5, `rgba(${26 + 46 * lamp},${20 + 30 * lamp},${10 + 10 * lamp},1)`);
    g.addColorStop(1, `rgba(${14 + 22 * lamp},${11 + 15 * lamp},${7 + 6 * lamp},1)`);
    c.fillStyle = g;
    c.fillRect(0, 0, w, h);

    if (lamp > 0.01) {
      // twin filament pools of light behind the glass
      for (const fx of [0.22, 0.78]) {
        const r = c.createRadialGradient(w * fx, h * 0.5, 4, w * fx, h * 0.5, w * 0.34);
        r.addColorStop(0, `rgba(255,190,90,${0.20 * lamp})`);
        r.addColorStop(1, "rgba(255,190,90,0)");
        c.fillStyle = r;
        c.fillRect(0, 0, w, h);
      }
    }

    // dial print, dimmed with the lamp
    if (this.print) {
      c.globalAlpha = 0.22 + 0.78 * lamp;
      c.drawImage(this.print, 0, 0, w, h);
      c.globalAlpha = 1;
    }

    // stereo beacon — lights only on full lock
    const bx = this.beaconX;
    const by = h - 12;
    const lit = Math.max(0, signal - 0.6) / 0.4;
    c.fillStyle = lit > 0
      ? `rgba(255,${Math.round(90 + 60 * lit)},40,${0.25 + 0.75 * lit})`
      : "rgba(60,30,18,0.9)";
    c.beginPath();
    c.arc(bx, by, 2.6, 0, Math.PI * 2);
    c.fill();
    if (lit > 0.3) {
      c.shadowColor = "rgba(255,120,50,0.9)";
      c.shadowBlur = 6 * lit;
      c.beginPath();
      c.arc(bx, by, 2.6, 0, Math.PI * 2);
      c.fill();
      c.shadowBlur = 0;
    }

    // needle — edge-lit red vertical, brighter when the lamp is on
    const x = this.freqToX(freq);
    c.strokeStyle = `rgba(255,${40 + 30 * lamp},${26 + 20 * lamp},${0.55 + 0.45 * lamp})`;
    c.lineWidth = 2;
    if (lamp > 0.05) {
      c.shadowColor = "rgba(255,60,30,0.8)";
      c.shadowBlur = 5 * lamp;
    }
    c.beginPath();
    c.moveTo(x, 6);
    c.lineTo(x, h - 6);
    c.stroke();
    c.shadowBlur = 0;
    // needle carriage highlight
    c.fillStyle = `rgba(255,255,255,${0.14 + 0.1 * lamp})`;
    c.fillRect(x - 0.5, 6, 1, h - 12);
  }
}
