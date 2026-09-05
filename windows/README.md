# Agents Sleep Preventer for Windows

Windows 10/11, Intel/AMD x64. Local Whisper and Parakeet engines automatically use AVX2 acceleration on compatible CPUs, with a portable fallback for older PCs. Both engines are included; no Rust, Python, CUDA, Visual C++ installer, or administrator rights are required to run the app.

## Install

1. Extract the **entire ZIP**, including the `speech` folder.
2. Double-click `asp.exe`. The tray icon appears near the clock (possibly under hidden icons).
3. Choose **Install agent hooks and start at login** from the tray menu, or run `.\asp.exe install --yes` in PowerShell from the extracted folder.
4. Restart native Windows Claude Code/Codex sessions to load their hooks.

Installation copies the app and speech engines to `%LOCALAPPDATA%\Programs\AgentsSleepPreventer`, creates Start menu/login shortcuts, and preserves other hooks. Original agent configuration files have `*.asp-backup` backups. The Windows release is currently unsigned.

## Dictation

1. Open **Dictation Settings…** from the tray menu.
2. **Parakeet v3** (669 MB) is selected by default for fast CPU dictation and automatic language detection. You can also choose **Whisper Turbo** (574 MB) or **Whisper Tiny** (78 MB). Select a language and optional custom vocabulary for Whisper. Parakeet detects its supported languages automatically.
3. Choose **Download Dictation Model…**. Download progress stays visible; models are verified with SHA-256 before use.
4. Focus an editable text field and press **Ctrl+Alt+Space** once. Speak, then press it again to finish. A floating indicator and optional sounds show recording/transcription state.
5. The text is inserted when transcription finishes. It remains editable: ASP does not press Enter or submit messages.

The shortcut is configurable. English is the default Whisper language; choose `fr` for French or `auto` for detection. Allow the microphone under **Windows Settings > Privacy > Microphone > Allow desktop apps to access your microphone**, accessible from **Microphone Permissions…** in the tray menu.

Recordings are limited to two minutes and automatically transcribed at that limit. Microphone buffers and temporary WAVs are removed after processing. Audio stays local; only model setup downloads from the internet. The last 100 transcripts are kept locally in `dictation-history.txt`, accessible through **Dictation History…**.

If the active window changes, modifier keys remain held, or Windows blocks typing into an elevated application, the transcript stays available under **Copy Last Dictation**. Automatic insertion preserves your clipboard. Choose **Cancel Dictation** to discard a recording or cancel a download/transcription.

## Sleep prevention

Automatic mode follows registered agent activity. Use **Keep awake** or **Allow sleep** for manual control. Recording and transcription also keep the machine awake unless Allow sleep is selected. Explicit Sleep, battery protection, and the configured lid/power-button actions remain under Windows control. To work with a laptop lid closed, configure **Choose what closing the lid does** in Windows Power Options; ASP does not change it.

Native Claude Code/Codex hooks are installed automatically. Automatic Hermes/WSL hooks, macOS Fn shortcuts, macOS agent notifications, and Sparkle are platform-specific. For another native process use `asp.exe start --pid <PID>` / `asp.exe stop --pid <PID>`, or Keep awake. WSL Linux PIDs are not Windows PIDs.

## CLI

```powershell
.\asp.exe status
.\asp.exe list
.\asp.exe force awake    # awake | sleep | auto
.\asp.exe cleanup
.\asp.exe reset
.\asp.exe dictation setup --model parakeet-v3
.\asp.exe dictation configure --language fr --hotkey 'Ctrl+Alt+D'
.\asp.exe dictation transcribe recording.wav
.\asp.exe dictation record --seconds 10
.\asp.exe quit
.\asp.exe uninstall
```

The CLI is not added to PATH. Settings, models, history, and diagnostics are stored under `%LOCALAPPDATA%\AgentsSleepPreventer`. Hook errors go to `hook-errors.log`. Exited/reused PIDs are removed automatically; markers expire after six hours without a hook refresh. Reset clears interrupted turns immediately.

## Updates and removal

**Check for Updates…** opens the latest release. Quit the installed app before running the new ZIP's `asp.exe install --yes`. Models and dictation preferences are preserved.

`asp.exe uninstall` stops the app and removes its agent hooks and startup shortcuts. After it exits, delete `%LOCALAPPDATA%\Programs\AgentsSleepPreventer`. Delete `%LOCALAPPDATA%\AgentsSleepPreventer` separately to remove models, preferences, and history.

## Build and verify

Development needs Rust/MSVC, Visual Studio C++ Build Tools, Git, and CMake:

```powershell
cargo test --locked --target x86_64-pc-windows-msvc
cargo xtask build-windows
```

`build-windows` builds static Whisper/Parakeet engines from a pinned upstream revision and creates a complete ZIP under `dist/`. It does not publish. The Windows workflow checks PowerShell syntax, Rust tests, real multi-agent sleep requests, and real Whisper/Parakeet transcription of an audio fixture. Microphone hardware and the user's target app still need an interactive check.

Cross-builds on macOS use cargo-xwin plus `ASP_WINDOWS_SPEECH_DIR` pointing to the `speech` folder of the tested native Windows artifact. Windows engine source and license information are included in that folder.
