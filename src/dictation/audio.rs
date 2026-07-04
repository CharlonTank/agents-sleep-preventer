use block::ConcreteBlock;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hound::{SampleFormat, WavSpec, WavWriter};
use objc::{class, msg_send, sel, sel_impl};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

// Link AVFoundation framework
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

/// Check current microphone permission status
pub fn check_microphone_permission() -> MicrophonePermission {
    unsafe {
        let media_type: *mut objc::runtime::Object =
            msg_send![class!(NSString), stringWithUTF8String: b"soun\0".as_ptr()];

        let status: i64 =
            msg_send![class!(AVCaptureDevice), authorizationStatusForMediaType: media_type];

        match status {
            0 => MicrophonePermission::NotDetermined,
            1 => MicrophonePermission::Denied, // Restricted
            2 => MicrophonePermission::Denied, // Denied
            3 => MicrophonePermission::Granted,
            _ => MicrophonePermission::Denied,
        }
    }
}

/// Request microphone permission and wait for result (blocking)
/// Returns true if granted, false if denied
pub fn request_microphone_permission_sync() -> bool {
    let result = Arc::new((Mutex::new(None::<bool>), Condvar::new()));
    let result_clone = result.clone();

    unsafe {
        let media_type: *mut objc::runtime::Object =
            msg_send![class!(NSString), stringWithUTF8String: b"soun\0".as_ptr()];

        // Create completion handler block
        let block = ConcreteBlock::new(move |granted: bool| {
            let (lock, cvar) = &*result_clone;
            let mut result = lock.lock().unwrap();
            *result = Some(granted);
            cvar.notify_one();
        });
        let block = block.copy();

        // Request access - this triggers the system dialog
        let _: () = msg_send![class!(AVCaptureDevice), requestAccessForMediaType: media_type completionHandler: &*block];
    }

    // Wait for result with timeout (30 seconds - user may take time to respond)
    let (lock, cvar) = &*result;
    let mut guard = lock.lock().unwrap();
    let timeout = std::time::Duration::from_secs(30);

    while guard.is_none() {
        let (new_guard, timeout_result) = cvar.wait_timeout(guard, timeout).unwrap();
        guard = new_guard;
        if timeout_result.timed_out() {
            return false;
        }
    }

    guard.unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MicrophonePermission {
    Granted,
    Denied,
    NotDetermined,
}

pub struct AudioRecorder {
    samples: Arc<Mutex<Vec<f32>>>,
    stream: Option<cpal::Stream>,
    sample_rate: u32,
    channels: u16,
}

impl AudioRecorder {
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("No audio input device available")?;

        let config = device
            .default_input_config()
            .map_err(|e| format!("Failed to get input config: {}", e))?;

        Ok(Self {
            samples: Arc::new(Mutex::new(Vec::new())),
            stream: None,
            sample_rate: config.sample_rate().0,
            channels: config.channels(),
        })
    }

    pub fn start_recording(&mut self) -> Result<(), String> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("No audio input device")?;

        let config = device
            .default_input_config()
            .map_err(|e| format!("Failed to get config: {}", e))?;

        self.sample_rate = config.sample_rate().0;
        self.channels = config.channels();

        // Clear previous samples
        self.samples.lock().unwrap().clear();
        let samples_clone = self.samples.clone();

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| {
                    samples_clone.lock().unwrap().extend_from_slice(data);
                },
                |err| eprintln!("Audio stream error: {}", err),
                None,
            ),
            cpal::SampleFormat::I16 => {
                let samples_clone = self.samples.clone();
                device.build_input_stream(
                    &config.into(),
                    move |data: &[i16], _| {
                        let floats: Vec<f32> =
                            data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                        samples_clone.lock().unwrap().extend_from_slice(&floats);
                    },
                    |err| eprintln!("Audio stream error: {}", err),
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let samples_clone = self.samples.clone();
                device.build_input_stream(
                    &config.into(),
                    move |data: &[u16], _| {
                        let floats: Vec<f32> = data
                            .iter()
                            .map(|&s| (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                            .collect();
                        samples_clone.lock().unwrap().extend_from_slice(&floats);
                    },
                    |err| eprintln!("Audio stream error: {}", err),
                    None,
                )
            }
            format => return Err(format!("Unsupported sample format: {:?}", format)),
        }
        .map_err(|e| format!("Failed to build input stream: {}", e))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start stream: {}", e))?;

        self.stream = Some(stream);
        Ok(())
    }

    pub fn stop_recording(&mut self) -> Vec<f32> {
        // Drop the stream to stop recording
        self.stream = None;
        std::mem::take(&mut *self.samples.lock().unwrap())
    }

    /// Stop capturing but leave the recorded samples in the shared buffer
    /// (used by the streaming path, where the transcription thread owns
    /// a clone of the buffer Arc).
    pub fn stop_stream(&mut self) {
        self.stream = None;
    }

    /// Shared live sample buffer, for streaming transcription while recording.
    pub fn live_buffer(&self) -> Arc<Mutex<Vec<f32>>> {
        self.samples.clone()
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Convert raw interleaved device samples to 16kHz mono.
    pub fn to_16k_mono(samples: &[f32], channels: u16, sample_rate: u32) -> Vec<f32> {
        const TARGET_SAMPLE_RATE: u32 = 16000;

        let mono_samples: Vec<f32> = if channels > 1 {
            samples
                .chunks(channels as usize)
                .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
                .collect()
        } else {
            samples.to_vec()
        };

        if sample_rate != TARGET_SAMPLE_RATE {
            Self::resample(&mono_samples, sample_rate, TARGET_SAMPLE_RATE)
        } else {
            mono_samples
        }
    }

    /// Save audio as 16kHz mono WAV (whisper-cpp format)
    pub fn save_to_wav(&self, samples: &[f32], path: &PathBuf) -> Result<(), String> {
        const TARGET_SAMPLE_RATE: u32 = 16000;

        let resampled = Self::to_16k_mono(samples, self.channels, self.sample_rate);

        let spec = WavSpec {
            channels: 1,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };

        let mut writer =
            WavWriter::create(path, spec).map_err(|e| format!("Failed to create WAV: {}", e))?;

        for &sample in &resampled {
            let amplitude = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer
                .write_sample(amplitude)
                .map_err(|e| format!("Failed to write sample: {}", e))?;
        }

        writer
            .finalize()
            .map_err(|e| format!("Failed to finalize WAV: {}", e))?;

        Ok(())
    }

    /// Linear interpolation resampling
    fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }

        let ratio = from_rate as f64 / to_rate as f64;
        let output_len = (samples.len() as f64 / ratio).ceil() as usize;
        let mut output = Vec::with_capacity(output_len);

        for i in 0..output_len {
            let src_pos = i as f64 * ratio;
            let src_idx = src_pos.floor() as usize;
            let frac = (src_pos - src_idx as f64) as f32;

            let sample = if src_idx + 1 < samples.len() {
                // Linear interpolation
                samples[src_idx] * (1.0 - frac) + samples[src_idx + 1] * frac
            } else if src_idx < samples.len() {
                samples[src_idx]
            } else {
                0.0
            };
            output.push(sample);
        }

        output
    }
}
