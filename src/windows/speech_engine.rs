use super::speech_config::Settings;
use crate::audio_samples;
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub fn runtime_dir() -> Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .context("Missing application directory")?
        .join("speech"))
}

pub fn runtime_ready() -> bool {
    runtime_dir().is_ok_and(|dir| {
        dir.join("whisper-cli.exe").is_file() && dir.join("parakeet-cli.exe").is_file()
    })
}

fn accelerated_cpu() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("fma")
            && std::is_x86_feature_detected!("f16c")
    }
    #[cfg(not(target_arch = "x86_64"))]
    false
}

fn engine_path(dir: &Path, parakeet: bool, accelerated: bool) -> PathBuf {
    let name = if parakeet {
        "parakeet-cli"
    } else {
        "whisper-cli"
    };
    let optimized = dir.join(format!("{name}-avx2.exe"));
    if accelerated && optimized.is_file() {
        optimized
    } else {
        dir.join(format!("{name}.exe"))
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn run_child(
    command: &mut Command,
    cancel: &AtomicBool,
    timeout: Duration,
    mut progress: impl FnMut(),
) -> Result<()> {
    let errors = tempfile::NamedTempFile::new()?;
    let mut child = command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errors.reopen()?)
        .spawn()
        .context("Could not start the local speech engine; reinstall the complete Windows ZIP")?;
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Operation cancelled or timed out");
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                ensure!(
                    status.success(),
                    "Local speech operation failed: {}",
                    fs::read_to_string(errors.path())
                        .unwrap_or_default()
                        .chars()
                        .take(1500)
                        .collect::<String>()
                );
                return Ok(());
            }
            Ok(None) => {
                progress();
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
    }
}

pub fn setup(
    dir: &Path,
    settings: &Settings,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    ensure!(
        runtime_ready(),
        "Speech engines are missing. Extract and install the complete Windows ZIP."
    );
    let model = settings.selected_model()?;
    let models = dir.join("models");
    fs::create_dir_all(&models)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(models.join(format!("{}.lock", model.id)))?;
    FileExt::try_lock_exclusive(&lock)
        .context("This model is already downloading in another ASP process")?;
    let path = models.join(model.filename);
    if settings.ready(dir) && hash_file(&path)? == model.sha256 {
        return Ok(());
    }
    let temp = tempfile::NamedTempFile::new_in(&models)?.into_temp_path();
    let url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        model.repository, model.filename
    );
    let mut command = Command::new("curl.exe");
    command
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--retry",
            "2",
            "--connect-timeout",
            "20",
            "--max-time",
            "1800",
            "--output",
        ])
        .arg(&temp)
        .arg(url);
    run_child(&mut command, cancel, Duration::from_secs(1900), || {
        progress(
            fs::metadata(&temp).map(|s| s.len()).unwrap_or(0),
            model.bytes,
        );
    })?;
    ensure!(
        fs::metadata(&temp)?.len() == model.bytes && hash_file(&temp)? == model.sha256,
        "Model checksum verification failed; retry Setup Dictation"
    );
    temp.persist(path)?;
    progress(model.bytes, model.bytes);
    Ok(())
}

pub fn transcribe(
    dir: &Path,
    settings: &Settings,
    samples: &[f32],
    cancel: &AtomicBool,
) -> Result<String> {
    ensure!(
        settings.ready(dir),
        "Download the selected model with Setup Dictation first"
    );
    ensure!(
        audio_samples::has_audio(samples),
        "No speech detected. Check the microphone or record for longer."
    );
    let temporary = tempfile::tempdir_in(dir)?;
    let audio = temporary.path().join("recording.wav");
    let output = temporary.path().join("transcript");
    audio_samples::write_wav(samples, &audio)?;
    let model = settings.selected_model()?;
    let engine = engine_path(&runtime_dir()?, model.parakeet, accelerated_cpu());
    let mut command = Command::new(engine);
    command
        .arg("-m")
        .arg(settings.model_path(dir)?)
        .arg("-f")
        .arg(&audio)
        .arg("-of")
        .arg(&output)
        .args(["-otxt", "-ng", "-np", "-t"])
        .arg(
            std::thread::available_parallelism()
                .map(|n| n.get().clamp(1, 8))
                .unwrap_or(4)
                .to_string(),
        );
    if !model.parakeet {
        command.args([
            "--no-timestamps",
            "--suppress-nst",
            "-l",
            &settings.language,
        ]);
        if !settings.vocabulary.trim().is_empty() {
            command.arg("--prompt").arg(&settings.vocabulary);
        }
    }
    run_child(&mut command, cancel, Duration::from_secs(600), || {})?;
    let text = audio_samples::normalized_text(&fs::read_to_string(output.with_extension("txt"))?);
    ensure!(!text.is_empty(), "No speech detected");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimized_engines_require_cpu_support_and_keep_a_portable_fallback() {
        let dir = tempfile::tempdir().unwrap();
        for (parakeet, name) in [(false, "whisper-cli"), (true, "parakeet-cli")] {
            let baseline = dir.path().join(format!("{name}.exe"));
            let optimized = dir.path().join(format!("{name}-avx2.exe"));
            assert_eq!(engine_path(dir.path(), parakeet, true), baseline);
            fs::write(&optimized, []).unwrap();
            assert_eq!(engine_path(dir.path(), parakeet, false), baseline);
            assert_eq!(engine_path(dir.path(), parakeet, true), optimized);
        }
    }
}
