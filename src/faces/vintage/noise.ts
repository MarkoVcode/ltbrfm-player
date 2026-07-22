// ---------------------------------------------------------------------------
// noise.ts — Web Audio inter-station static for the vintage face.
//
// Real FM inter-station noise is not plain white noise: the receiver's
// de-emphasis rolls the top off, the AGC pumps slowly, and there are
// occasional impulsive crackles. We bake a few seconds of shaped noise into
// a looping buffer, then run it through a gentle low-pass and a gain node.
// The face drives the level every frame (with a slow wobble) so the static
// crossfades against the carrier as the needle moves.
//
// The AudioContext is created lazily on the power gesture, which satisfies
// every webview's autoplay policy.
// ---------------------------------------------------------------------------

const LOOP_SECONDS = 4;

export class StaticNoise {
  private ctx: AudioContext | null = null;
  private gain: GainNode | null = null;
  private level = 0;

  /** Create/resume the audio graph. Must be called from a user gesture. */
  start() {
    if (this.ctx) {
      this.ctx.resume().catch(() => {});
      return;
    }
    const ctx = new AudioContext();
    const rate = ctx.sampleRate;
    const buf = ctx.createBuffer(2, LOOP_SECONDS * rate, rate);

    for (let ch = 0; ch < 2; ch++) {
      const data = buf.getChannelData(ch);
      let lp = 0; // one-pole low-pass state — softens the hiss
      let agc = 1; // slow random amplitude drift, like a pumping AGC
      let crackle = 0;
      for (let i = 0; i < data.length; i++) {
        const w = Math.random() * 2 - 1;
        lp += 0.18 * (w - lp);
        if (i % 2048 === 0) agc = 0.75 + Math.random() * 0.5;
        // sparse impulsive crackles with a fast decay
        if (Math.random() < 0.00004) crackle = (Math.random() * 2 - 1) * 2.5;
        crackle *= 0.995;
        data[i] = (w * 0.35 + lp * 0.9 + crackle) * 0.5 * agc;
      }
    }

    const src = ctx.createBufferSource();
    src.buffer = buf;
    src.loop = true;

    const filter = ctx.createBiquadFilter();
    filter.type = "lowpass";
    filter.frequency.value = 5200;
    filter.Q.value = 0.4;

    const gain = ctx.createGain();
    gain.gain.value = 0;

    src.connect(filter).connect(gain).connect(ctx.destination);
    src.start();

    this.ctx = ctx;
    this.gain = gain;
  }

  /** Target loudness 0..1; smoothed so needle sweeps never click. */
  setLevel(v: number) {
    const nv = Math.max(0, Math.min(1, v));
    if (!this.ctx || !this.gain) {
      this.level = nv;
      return;
    }
    if (Math.abs(nv - this.level) < 0.002) return;
    this.level = nv;
    // Perceptual-ish curve — static should sit under the programme, not on it.
    this.gain.gain.setTargetAtTime(nv * nv * 0.5, this.ctx.currentTime, 0.06);
  }

  suspend() {
    this.setLevel(0);
    this.ctx?.suspend().catch(() => {});
  }

  resume() {
    this.ctx?.resume().catch(() => {});
  }
}
