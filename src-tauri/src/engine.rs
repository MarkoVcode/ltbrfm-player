//! Engine orchestration: owns a long-lived worker thread that plays a stream
//! session (network -> decode -> DSP -> resample -> output) and reconnects
//! forever until told to stop. Tauri commands mutate [`Controls`] atomics and
//! send [`Cmd`]s here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tauri::{AppHandle, Emitter};

use crate::dsp::{Controls, Dsp, NUM_BANDS};
use crate::output::Output;
use crate::spectrum::Spectrum;
use crate::stream::{self, BytePipe, PipeReader};

const RESAMPLE_CHUNK: usize = 1024;

pub const PRESETS: &[(&str, [f32; NUM_BANDS])] = &[
    ("flat", [0.0; NUM_BANDS]),
    ("pirate", [4.0, 5.0, 2.0, -1.0, -2.0, 0.0, 2.0, 4.0, 5.0, 3.0]),
    ("bass", [8.0, 7.0, 5.0, 2.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0]),
    ("voice", [-4.0, -3.0, 0.0, 3.0, 5.0, 5.0, 3.0, 1.0, -1.0, -2.0]),
];

enum Cmd {
    Play(String),
    Stop,
}

#[derive(serde::Serialize, Clone)]
struct NowPlaying {
    title: String,
}

#[derive(serde::Serialize, Clone)]
struct StatePayload {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn emit_state(app: &AppHandle, state: &'static str, message: Option<&str>) {
    let _ = app.emit(
        "state",
        StatePayload {
            state,
            message: message.map(|s| s.to_string()),
        },
    );
}

pub struct Engine {
    controls: Arc<Controls>,
    cmd_tx: Sender<Cmd>,
    session: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

impl Engine {
    pub fn new(app: AppHandle) -> Engine {
        let controls = Arc::new(Controls::default());
        let session: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::new(Mutex::new(None));
        let (cmd_tx, cmd_rx) = channel::<Cmd>();

        {
            let controls = controls.clone();
            let session = session.clone();
            thread::Builder::new()
                .name("ltbr-audio".into())
                .spawn(move || worker(cmd_rx, controls, session, app))
                .expect("failed to spawn audio worker");
        }

        Engine {
            controls,
            cmd_tx,
            session,
        }
    }

    pub fn controls(&self) -> &Arc<Controls> {
        &self.controls
    }

    fn cancel_current(&self) {
        if let Some(flag) = self.session.lock().unwrap().take() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn play(&self, url: String) {
        self.cancel_current();
        let _ = self.cmd_tx.send(Cmd::Play(url));
    }

    pub fn stop(&self) {
        self.cancel_current();
        let _ = self.cmd_tx.send(Cmd::Stop);
    }
}

fn worker(
    rx: Receiver<Cmd>,
    controls: Arc<Controls>,
    session: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    app: AppHandle,
) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Stop => {
                emit_state(&app, "standby", None);
            }
            Cmd::Play(url) => {
                let stop = Arc::new(AtomicBool::new(false));
                *session.lock().unwrap() = Some(stop.clone());
                run_session(&url, stop, &controls, &app);
            }
        }
    }
}

/// Play one URL, reconnecting until `stop`.
fn run_session(url: &str, stop: Arc<AtomicBool>, controls: &Arc<Controls>, app: &AppHandle) {
    let mut out = match Output::new() {
        Ok(o) => o,
        Err(e) => {
            emit_state(app, "error", Some(&format!("Audio device error: {e}")));
            return;
        }
    };

    let mut backoff_ms = 500u64;
    while !stop.load(Ordering::Relaxed) {
        emit_state(app, "tuning", Some("acquiring…"));

        let pipe = BytePipe::new();
        let attempt_stop = Arc::new(AtomicBool::new(false));

        // combined stop for the network thread
        let net_stop = Arc::new(AtomicBool::new(false));
        let net_handle = {
            let url = url.to_string();
            let pipe = pipe.clone();
            let app = app.clone();
            let net_stop = net_stop.clone();
            thread::Builder::new()
                .name("ltbr-net".into())
                .spawn(move || {
                    let _ = stream::run(&url, net_stop, pipe, |title| {
                        let _ = app.emit("nowplaying", NowPlaying { title });
                    });
                })
                .ok()
        };

        let outcome = decode_loop(&pipe, &stop, &attempt_stop, controls, &mut out, app);

        // Tear the attempt down.
        attempt_stop.store(true, Ordering::SeqCst);
        net_stop.store(true, Ordering::SeqCst);
        pipe.close();
        if let Some(h) = net_handle {
            let _ = h.join();
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }

        match outcome {
            DecodeOutcome::Ended => backoff_ms = 500,
            DecodeOutcome::Failed(msg) => {
                emit_state(app, "tuning", Some(&format!("reconnecting… ({msg})")));
                let _ = app.emit(
                    "fault",
                    serde_json::json!({ "message": format!("Stream dropped: {msg}. Reconnecting…") }),
                );
            }
        }

        // Backoff, staying responsive to stop.
        let mut waited = 0;
        while waited < backoff_ms && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
            waited += 50;
        }
        backoff_ms = (backoff_ms * 2).min(8000);
    }
}

enum DecodeOutcome {
    Ended,
    Failed(String),
}

fn decode_loop(
    pipe: &Arc<BytePipe>,
    stop: &AtomicBool,
    attempt_stop: &AtomicBool,
    controls: &Arc<Controls>,
    out: &mut Output,
    app: &AppHandle,
) -> DecodeOutcome {
    let mss = MediaSourceStream::new(Box::new(PipeReader::new(pipe.clone())), Default::default());

    let mut hint = Hint::new();
    hint.mime_type("audio/mpeg");
    hint.with_extension("mp3");

    let probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(e) => return DecodeOutcome::Failed(format!("probe: {e}")),
    };
    let mut format = probed.format;

    let track = match format.default_track() {
        Some(t) => t.clone(),
        None => return DecodeOutcome::Failed("no audio track".into()),
    };
    let mut decoder = match symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
    {
        Ok(d) => d,
        Err(e) => return DecodeOutcome::Failed(format!("codec: {e}")),
    };
    let track_id = track.id;

    let mut state: Option<SessionState> = None;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut live = false;

    loop {
        if stop.load(Ordering::Relaxed) || attempt_stop.load(Ordering::Relaxed) {
            return DecodeOutcome::Ended;
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(_)) => return DecodeOutcome::Ended,
            Err(e) => return DecodeOutcome::Failed(format!("read: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue, // skip a bad frame
            Err(SymError::IoError(_)) => return DecodeOutcome::Ended,
            Err(e) => return DecodeOutcome::Failed(format!("decode: {e}")),
        };

        let spec = *decoded.spec();
        let frames = decoded.capacity() as u64;
        let sbuf = sample_buf.get_or_insert_with(|| SampleBuffer::<f32>::new(frames, spec));
        sbuf.copy_interleaved_ref(decoded);
        let interleaved = sbuf.samples();
        let in_ch = spec.channels.count().max(1);

        let st = state.get_or_insert_with(|| {
            let app = app.clone();
            SessionState::new(
                spec.rate,
                out.sample_rate,
                out.channels,
                controls.clone(),
                Box::new(move |bars| {
                    let _ = app.emit("spectrum", bars.to_vec());
                }),
            )
        });

        st.process(interleaved, in_ch, out, stop, attempt_stop);

        if !live {
            live = true;
            emit_state(app, "live", None);
        }
    }
}

/// Per-session mutable audio state: DSP, spectrum, optional resampler.
/// AppHandle-free so it can be driven both by the Tauri engine and by the
/// standalone `audio_probe` example / tests.
pub struct SessionState {
    dsp: Dsp,
    spectrum: Spectrum,
    app_channels: usize,
    resampler: Option<SincFixedIn<f32>>,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    scratch: Vec<f32>,
    on_spectrum: Box<dyn FnMut([f32; crate::spectrum::BARS]) + Send>,
}

impl SessionState {
    pub fn new(
        in_rate: u32,
        out_rate: u32,
        out_channels: usize,
        controls: Arc<Controls>,
        on_spectrum: Box<dyn FnMut([f32; crate::spectrum::BARS]) + Send>,
    ) -> Self {
        let resampler = if in_rate != out_rate {
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            SincFixedIn::<f32>::new(
                out_rate as f64 / in_rate as f64,
                2.0,
                params,
                RESAMPLE_CHUNK,
                2,
            )
            .ok()
        } else {
            None
        };

        SessionState {
            dsp: Dsp::new(in_rate as f32, controls),
            spectrum: Spectrum::new(in_rate as f32),
            app_channels: out_channels,
            resampler,
            in_l: Vec::with_capacity(RESAMPLE_CHUNK * 2),
            in_r: Vec::with_capacity(RESAMPLE_CHUNK * 2),
            scratch: Vec::new(),
            on_spectrum,
        }
    }

    pub fn process(
        &mut self,
        interleaved: &[f32],
        in_ch: usize,
        out: &mut Output,
        stop: &AtomicBool,
        attempt_stop: &AtomicBool,
    ) {
        self.render(interleaved, in_ch);
        push_all(out, &self.scratch, stop, attempt_stop);
    }

    /// DSP + resample one decoded packet into `self.scratch` (device-layout
    /// interleaved). Separated from the output push so it can be unit-tested.
    fn render(&mut self, interleaved: &[f32], in_ch: usize) {
        let frames = interleaved.len() / in_ch;
        self.scratch.clear();

        for f in 0..frames {
            let base = f * in_ch;
            let (l, r) = if in_ch >= 2 {
                (interleaved[base], interleaved[base + 1])
            } else {
                let m = interleaved[base];
                (m, m)
            };

            let (ol, or, tap) = self.dsp.process_frame(l, r);

            if let Some(bars) = self.spectrum.push(tap) {
                (self.on_spectrum)(bars);
            }

            match &mut self.resampler {
                Some(_) => {
                    self.in_l.push(ol);
                    self.in_r.push(or);
                }
                None => interleave_into(&mut self.scratch, ol, or, self.app_channels),
            }
        }

        // Drain any full resampler chunks.
        if self.resampler.is_some() {
            self.drain_resampler();
        }
    }

    fn drain_resampler(&mut self) {
        let ch = self.app_channels;
        let resampler = self.resampler.as_mut().unwrap();
        while self.in_l.len() >= RESAMPLE_CHUNK {
            let l: Vec<f32> = self.in_l.drain(..RESAMPLE_CHUNK).collect();
            let r: Vec<f32> = self.in_r.drain(..RESAMPLE_CHUNK).collect();
            if let Ok(outbuf) = resampler.process(&[l, r], None) {
                let n = outbuf[0].len();
                for i in 0..n {
                    interleave_into(&mut self.scratch, outbuf[0][i], outbuf[1][i], ch);
                }
            }
        }
    }
}

#[inline]
fn interleave_into(buf: &mut Vec<f32>, l: f32, r: f32, channels: usize) {
    match channels {
        0 => {}
        1 => buf.push(0.5 * (l + r)),
        _ => {
            buf.push(l);
            buf.push(r);
            for _ in 2..channels {
                buf.push(0.0);
            }
        }
    }
}

/// Push every sample, spinning briefly when the ring is full. Bails on stop.
fn push_all(out: &mut Output, data: &[f32], stop: &AtomicBool, attempt_stop: &AtomicBool) {
    let mut off = 0;
    while off < data.len() {
        off += out.push(&data[off..]);
        if off < data.len() {
            if stop.load(Ordering::Relaxed) || attempt_stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise the resampler path (48 kHz stream -> 44.1 kHz device) that this
    /// dev machine's matched-rate device does not hit at runtime.
    #[test]
    fn resamples_48k_to_44k_cleanly() {
        let controls = Arc::new(Controls::default());
        controls.set_volume(1.0);
        let mut st = SessionState::new(48_000, 44_100, 2, controls, Box::new(|_| {}));
        assert!(st.resampler.is_some(), "resampler should be active");

        // Feed 1 second of a 440 Hz stereo sine at 48 kHz.
        let n = 48_000;
        let mut interleaved = Vec::with_capacity(n * 2);
        for i in 0..n {
            let x = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin() * 0.5;
            interleaved.push(x);
            interleaved.push(x);
        }
        st.render(&interleaved, 2);

        // Output is stereo-interleaved; expect ~44.1k frames (down from 48k),
        // all finite and within range.
        let frames = st.scratch.len() / 2;
        assert!(
            (40_000..=44_100).contains(&frames),
            "unexpected resampled frame count: {frames}"
        );
        assert!(
            st.scratch.iter().all(|s| s.is_finite() && s.abs() <= 1.5),
            "resampled output has bad samples"
        );
    }

    #[test]
    fn matched_rate_has_no_resampler() {
        let controls = Arc::new(Controls::default());
        let st = SessionState::new(44_100, 44_100, 2, controls, Box::new(|_| {}));
        assert!(st.resampler.is_none());
    }
}
