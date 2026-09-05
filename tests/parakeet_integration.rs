#![cfg(target_os = "macos")]

//! Integration check for the Parakeet engine. Runs only when the model is
//! already downloaded (dev machines); skips silently otherwise.

use std::path::PathBuf;
use std::process::Command;

use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;

fn model_dir() -> Option<PathBuf> {
    let dir = dirs::data_local_dir()?
        .join("AgentsSleepPreventer")
        .join("models")
        .join("parakeet-tdt-0.6b-v3-int8");
    dir.join("encoder-model.int8.onnx").exists().then_some(dir)
}

#[test]
fn parakeet_transcribes_generated_speech() {
    let Some(dir) = model_dir() else {
        eprintln!("Parakeet model not downloaded; skipping");
        return;
    };

    let tmp = std::env::temp_dir();
    let aiff = tmp.join("asp_parakeet_test.aiff");
    let wav = tmp.join("asp_parakeet_test.wav");
    let ok = Command::new("say")
        .args(["-o", aiff.to_str().unwrap(), "Hello world, testing dictation."])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        && Command::new("afconvert")
            .args([
                "-f",
                "WAVE",
                "-d",
                "LEI16@16000",
                "-c",
                "1",
                aiff.to_str().unwrap(),
                wav.to_str().unwrap(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    assert!(ok, "failed to generate test audio");

    let mut model = ParakeetModel::load(&dir, &Quantization::Int8).expect("model load");
    let samples = transcribe_rs::audio::read_wav_samples(&wav).expect("read wav");
    let result = model
        .transcribe_with(&samples, &ParakeetParams::default())
        .expect("transcription");

    let text = result.text.to_lowercase();
    eprintln!("Parakeet transcription: {:?}", result.text);
    assert!(
        text.contains("hello") && text.contains("world"),
        "unexpected transcription: {:?}",
        result.text
    );

    let _ = std::fs::remove_file(aiff);
    let _ = std::fs::remove_file(wav);
}
