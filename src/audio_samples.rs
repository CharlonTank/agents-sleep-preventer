use anyhow::{ensure, Result};
use std::path::Path;

pub const SAMPLE_RATE: u32 = 16_000;
pub const MAX_RECORDING_SECS: u32 = 120;

pub fn to_16k_mono(samples: &[f32], channels: u16, rate: u32) -> Vec<f32> {
    if channels == 0 || rate == 0 {
        return Vec::new();
    }
    let mono: Vec<f32> = samples
        .chunks_exact(channels as usize)
        .map(|frame| {
            frame
                .iter()
                .map(|x| if x.is_finite() { *x } else { 0.0 })
                .sum::<f32>()
                / channels as f32
        })
        .collect();
    if rate == SAMPLE_RATE {
        return mono;
    }
    let ratio = rate as f64 / SAMPLE_RATE as f64;
    let len = (mono.len() as f64 / ratio).ceil() as usize;
    (0..len)
        .map(|i| {
            let position = i as f64 * ratio;
            let index = position.floor() as usize;
            let fraction = (position - index as f64) as f32;
            let first = mono[index.min(mono.len() - 1)];
            let second = mono[(index + 1).min(mono.len() - 1)];
            first * (1.0 - fraction) + second * fraction
        })
        .collect()
}

pub fn write_wav(samples: &[f32], path: &Path) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for sample in samples {
        writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

pub fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    ensure!(
        spec.channels > 0 && spec.sample_rate > 0,
        "Invalid WAV format"
    );
    ensure!(
        reader.duration() <= spec.sample_rate.saturating_mul(MAX_RECORDING_SECS),
        "Audio is longer than two minutes"
    );
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            ensure!(
                spec.bits_per_sample > 0 && spec.bits_per_sample <= 32,
                "Unsupported WAV bit depth"
            );
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|x| x.map(|v| v as f32 / scale))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(to_16k_mono(&samples, spec.channels, spec.sample_rate))
}

pub fn has_audio(samples: &[f32]) -> bool {
    samples.len() >= (SAMPLE_RATE / 5) as usize
        && samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32 > 0.000_000_01
}

pub fn normalized_text(text: &str) -> String {
    // Never inject line breaks or control keys into a terminal: dictation
    // inserts editable text and must not submit a command or a chat message.
    text.trim_start_matches('\u{feff}')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn microphone_formats_convert_and_wav_roundtrips() {
        let stereo = (0..48_000)
            .flat_map(|i| {
                let s = (i as f32 * 0.05).sin() * 0.4;
                [s, s]
            })
            .collect::<Vec<_>>();
        let mono = to_16k_mono(&stereo, 2, 48_000);
        assert_eq!(mono.len(), 16_000);
        assert!(has_audio(&mono));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.wav");
        write_wav(&mono, &path).unwrap();
        let decoded = read_wav(&path).unwrap();
        assert_eq!(decoded.len(), mono.len());
        assert!((decoded[100] - mono[100]).abs() < 0.0001);
        assert!(!has_audio(&vec![0.0; 16_000]));
        assert!(to_16k_mono(&[], 1, 48_000).is_empty());
    }
    #[test]
    fn transcript_does_not_submit_terminal_commands_and_keeps_unicode() {
        assert_eq!(
            normalized_text("\u{feff} Bonjour\r\n世界\t🙂\u{7} "),
            "Bonjour 世界 🙂"
        );
    }
}
