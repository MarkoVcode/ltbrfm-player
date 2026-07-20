//! Audio output via cpal. The decode thread produces device-rate, device-layout
//! interleaved f32 frames into a lock-free ring buffer; the real-time callback
//! only ever pops from it (never allocates or locks).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

pub struct Output {
    _stream: cpal::Stream,
    prod: HeapProd<f32>,
    pub channels: usize,
    pub sample_rate: u32,
}

impl Output {
    /// Open the default output device and start its stream.
    pub fn new() -> Result<Output, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no output audio device found".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("no default output config: {e}"))?;

        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();
        let channels = config.channels as usize;
        let sample_rate = config.sample_rate.0;

        // ~2 seconds of slack to ride out network jitter.
        let capacity = (sample_rate as usize) * channels * 2;
        let (prod, cons) = HeapRb::<f32>::new(capacity).split();

        let stream = match sample_format {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, cons),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, cons),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, cons),
            other => Err(format!("unsupported sample format: {other:?}")),
        }?;

        stream.play().map_err(|e| format!("failed to start audio stream: {e}"))?;

        Ok(Output {
            _stream: stream,
            prod,
            channels,
            sample_rate,
        })
    }

    /// Push interleaved device-layout frames; returns how many samples were
    /// accepted (may be fewer than offered when the buffer is full).
    #[inline]
    pub fn push(&mut self, data: &[f32]) -> usize {
        self.prod.push_slice(data)
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut cons: HeapCons<f32>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            config,
            move |out: &mut [T], _| {
                if scratch.len() < out.len() {
                    scratch.resize(out.len(), 0.0);
                }
                let got = cons.pop_slice(&mut scratch[..out.len()]);
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = if i < got { scratch[i] } else { 0.0 };
                    *slot = T::from_sample(v);
                }
            },
            move |err| eprintln!("audio output error: {err}"),
            None,
        )
        .map_err(|e| format!("failed to build output stream: {e}"))
}
