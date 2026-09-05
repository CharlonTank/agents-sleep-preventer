use crate::audio_samples::{self, MAX_RECORDING_SECS};
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};

pub struct Recorder {
    stream: cpal::Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
    rate: u32,
    channels: u16,
}

impl Recorder {
    pub fn start() -> Result<Self> {
        let device = cpal::default_host().default_input_device().context("No microphone found. Connect a microphone and allow desktop apps in Windows microphone privacy settings.")?;
        let supported = device
            .default_input_config()
            .context("Cannot open the microphone. Check Windows microphone permissions.")?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let error = Arc::new(Mutex::new(None));
        let stream = match format {
            cpal::SampleFormat::F32 => {
                build::<f32>(&device, &config, samples.clone(), error.clone())
            }
            cpal::SampleFormat::F64 => {
                build::<f64>(&device, &config, samples.clone(), error.clone())
            }
            cpal::SampleFormat::I8 => build::<i8>(&device, &config, samples.clone(), error.clone()),
            cpal::SampleFormat::U8 => build::<u8>(&device, &config, samples.clone(), error.clone()),
            cpal::SampleFormat::I16 => {
                build::<i16>(&device, &config, samples.clone(), error.clone())
            }
            cpal::SampleFormat::U16 => {
                build::<u16>(&device, &config, samples.clone(), error.clone())
            }
            cpal::SampleFormat::I32 => {
                build::<i32>(&device, &config, samples.clone(), error.clone())
            }
            cpal::SampleFormat::U32 => {
                build::<u32>(&device, &config, samples.clone(), error.clone())
            }
            _ => bail!("Unsupported microphone format: {format:?}"),
        }?;
        stream.play()?;
        Ok(Self {
            stream,
            samples,
            error,
            rate: config.sample_rate.0,
            channels: config.channels,
        })
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }
    pub fn stop(self) -> Result<Vec<f32>> {
        drop(self.stream);
        if let Some(error) = self.error.lock().ok().and_then(|e| e.clone()) {
            bail!("Microphone disconnected: {error}");
        }
        let samples = self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("Microphone buffer unavailable"))?;
        Ok(audio_samples::to_16k_mono(
            &samples,
            self.channels,
            self.rate,
        ))
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let limit =
        config.sample_rate.0 as usize * config.channels as usize * MAX_RECORDING_SECS as usize;
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            if let Ok(mut buffer) = samples.lock() {
                let remaining = limit.saturating_sub(buffer.len());
                buffer.extend(
                    data.iter()
                        .take(remaining)
                        .map(|sample| sample.to_sample::<f32>()),
                );
            }
        },
        move |err| {
            if let Ok(mut value) = error.lock() {
                *value = Some(err.to_string());
            }
        },
        None,
    )?)
}
