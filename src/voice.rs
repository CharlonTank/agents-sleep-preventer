//! Parakeet transcription of recorded audio, shared by live dictation
//! (macOS) and Telegram voice notes (macOS and Linux).

use std::path::Path;
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;

/// The model's files, downloaded from Hugging Face
/// (istupakov/parakeet-tdt-0.6b-v3-onnx) into `parakeet-tdt-0.6b-v3-int8/`.
pub const PARAKEET_FILES: [&str; 4] = [
    "encoder-model.int8.onnx",
    "decoder_joint-model.int8.onnx",
    "nemo128.onnx",
    "vocab.txt",
];

/// Commit the transcript in chunks while recording so the hotkey release
/// only has to transcribe the last few seconds, never the whole recording.
pub(crate) const COMMIT_TARGET_SECS: usize = 15;
/// Search for the quietest cut point this far back from the commit target,
/// so chunks split on a natural pause instead of mid-word.
pub(crate) const COMMIT_SEARCH_SECS: usize = 5;
/// Silence padding around each transcribed snippet to soften edge effects.
const SNIPPET_PAD_SECS: f32 = 0.2;

/// Quietest 30ms-frame boundary in `raw[from..to]` (channel-aligned).
pub(crate) fn quietest_cut(raw: &[f32], channels: u16, sample_rate: u32, from: usize, to: usize) -> usize {
    let channels = channels.max(1) as usize;
    let frame = ((sample_rate as usize * 30 / 1000) * channels).max(channels);
    let from = from - (from % channels);
    let to = to.min(raw.len());

    let mut best = to - (to % channels);
    let mut best_rms = f32::MAX;
    let mut offset = from;
    while offset + frame <= to {
        let rms = raw[offset..offset + frame]
            .iter()
            .map(|s| s * s)
            .sum::<f32>()
            / frame as f32;
        if rms < best_rms {
            best_rms = rms;
            best = offset + frame;
        }
        offset += frame;
    }
    best
}

/// Transcribe a 16k-mono snippet padded with a little silence; None when
/// empty or failed.
pub(crate) fn transcribe_snippet(model: &mut ParakeetModel, samples: &[f32]) -> Option<String> {
    if samples.is_empty() {
        return None;
    }
    let pad = (SNIPPET_PAD_SECS * 16_000.0) as usize;
    let mut padded = vec![0.0f32; pad];
    padded.extend_from_slice(samples);
    padded.resize(padded.len() + pad, 0.0);
    model
        .transcribe_with(&padded, &ParakeetParams::default())
        .ok()
        .map(|result| result.text.trim().to_string())
        .filter(|text| !text.is_empty())
}


/// Transcribes a whole 16 kHz mono recording (a voice message), cut at
/// natural pauses about every 15 s like live dictation, so long notes stay
/// accurate. The model loads once for all the cuts.
pub fn transcribe_recording(model_dir: &Path, samples: &[f32]) -> Result<String, String> {
    let mut model = ParakeetModel::load(model_dir, &Quantization::Int8)
        .map_err(|e| format!("Failed to load Parakeet model: {}", e))?;
    let target = COMMIT_TARGET_SECS * 16_000;
    let search = COMMIT_SEARCH_SECS * 16_000;
    let mut parts = Vec::new();
    let mut start = 0;
    while samples.len() - start > target + search {
        let cut = quietest_cut(samples, 1, 16_000, start + target - search, start + target);
        parts.extend(transcribe_snippet(&mut model, &samples[start..cut]));
        start = cut;
    }
    parts.extend(transcribe_snippet(&mut model, &samples[start..]));
    Ok(parts.join(" "))
}

/// Where the model goes on Linux: ~/.local/share/AgentsSleepPreventer/models.
#[cfg(target_os = "linux")]
fn linux_model_dir() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("AgentsSleepPreventer")
        .join("models")
        .join("parakeet-tdt-0.6b-v3-int8")
}

/// The downloaded model folder, if complete.
#[cfg(target_os = "linux")]
pub fn parakeet_model_dir() -> Option<std::path::PathBuf> {
    let dir = linux_model_dir();
    PARAKEET_FILES.iter().all(|file| dir.join(file).is_file()).then_some(dir)
}

/// Downloads the model (about 640 MB), file by file, each to a temporary
/// name first so an interrupted download never looks complete.
#[cfg(target_os = "linux")]
pub fn download_parakeet() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    let dir = linux_model_dir();
    std::fs::create_dir_all(&dir)?;
    for file in PARAKEET_FILES {
        let target = dir.join(file);
        if target.is_file() {
            continue;
        }
        println!("  {file}…");
        let url = format!("https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/{file}");
        let response = ureq::get(&url).call().with_context(|| url.clone())?;
        let partial = target.with_extension("part");
        let mut out = std::fs::File::create(&partial)?;
        std::io::copy(&mut response.into_body().into_reader(), &mut out)?;
        std::fs::rename(&partial, &target)?;
    }
    Ok(dir)
}
