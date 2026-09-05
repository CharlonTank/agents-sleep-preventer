<div align="center">

# ☕ Agents Sleep Preventer

### Keep your computer awake while coding agents are working
**Sleep prevention and local voice dictation on macOS and Windows.**

<br>

[![Download DMG](https://img.shields.io/badge/Download-DMG%20Installer-blue?style=for-the-badge&logo=apple)](https://github.com/CharlonTank/agents-sleep-preventer/releases/latest/download/AgentsSleepPreventer-5.0.0.dmg)

<br>

![macOS](https://img.shields.io/badge/macOS-000000?style=flat&logo=apple&logoColor=white)
![Rust](https://img.shields.io/badge/Rust-000000?style=flat&logo=rust&logoColor=white)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![GitHub release](https://img.shields.io/github/v/release/CharlonTank/agents-sleep-preventer)](https://github.com/CharlonTank/agents-sleep-preventer/releases)

</div>

---

## The Problem

You ask your coding agent to refactor your codebase. It's going to take 10 minutes. You close your MacBook lid to grab coffee...

**💀 Mac sleeps. The agent stops. Work lost.**

## The Solution

Install this tool. Your computer stays awake while your agent works; normal sleep resumes when it finishes. On macOS, it can also keep working with the lid closed and provides local speech-to-text dictation. Windows prevents automatic idle sleep and respects the configured lid and power-button actions.

<div align="center">

| Before (macOS) | After (macOS) |
|--------|-------|
| 😴 Lid closed = Mac sleeps | ☕ Lid closed = agent keeps working |
| 🔄 Come back to interrupted work | ✅ Come back to finished work |

</div>

---

## Features

- Sleep prevention while supported coding agents are active, on macOS and Windows
- A macOS menu bar app or Windows tray app, with automatic and manual sleep controls

- Local voice dictation with a customizable hotkey: hold Fn+Shift on macOS, toggle Ctrl+Alt+Space on Windows
- Two dictation engines: Whisper (best accuracy, custom vocabulary) or Parakeet v3 (near-instant transcription)
- English transcription by default, with language selection in Settings
- Dictation settings, recording cues, and local transcription history
- macOS extras: agent notifications, terminal controls, and signed Sparkle updates

---

## Installation

### Windows 10 / 11

[Download the Windows ZIP](https://github.com/CharlonTank/agents-sleep-preventer/releases/latest/download/AgentsSleepPreventer-5.0.0-windows-x86_64.zip), extract it, and run `asp.exe` for the system tray app. To install native Claude Code/Codex hooks and start at login, run this in PowerShell from the extracted folder:

```powershell
.\asp.exe install --yes
```

No administrator rights are required. See the [Windows guide](windows/README.md) for build prerequisites, commands, verification, and platform differences. For dictation, open **Dictation Settings…** in the tray menu, choose Whisper or Parakeet, then **Download Dictation Model…**. Press Ctrl+Alt+Space to start recording and again to insert the transcript.

### 🍎 Download DMG (Easiest)

1. [Download the latest DMG](https://github.com/CharlonTank/agents-sleep-preventer/releases/latest/download/AgentsSleepPreventer-5.0.0.dmg)
2. Drag `AgentsSleepPreventer.app` to Applications
3. Launch the app - it will auto-configure on first run
4. Restart your coding agents

The menu bar app uses Sparkle for in-app updates and can download + install new signed releases directly.

### 🍺 Homebrew

```bash
brew tap CharlonTank/tap
brew install agents-sleep-preventer
asp install
```

### 🦀 Build from Source

```bash
git clone https://github.com/CharlonTank/agents-sleep-preventer.git
cd agents-sleep-preventer
cargo build --release
./target/release/asp install
```

---

## How It Works

### Sleep Prevention

```
You send a prompt
       ↓
   Agent starts working → 🔒 Sleep disabled
       ↓
   Agent finishes → 🔓 Sleep re-enabled
```

Sleep prevention works automatically after setup.

### Dictation

Agents Sleep Preventer includes local speech-to-text dictation:

1. Open the app and allow Microphone and Accessibility permissions.
2. Download the local Whisper model when prompted.
3. Press and hold Fn+Shift (default) to record, then release to transcribe and insert text.

Dictation starts in English by default. To change the language, the hotkey, or the engine, open `Settings...` and select the `Dictation` tab.

Two engines are available:

| Model | Best for |
|---|---|
| **Whisper Turbo** (574 MB / 1.6 GB) | Best accuracy, 99 languages, custom vocabulary support |
| **Parakeet v3** (670 MB) | Near-instant transcription, English + 24 European languages |

---

## Commands

```bash
asp status     # Check current state
asp settings   # Open sleep prevention and dictation settings
asp cleanup    # Clean up after interrupts
asp uninstall  # Remove completely
```

---

## FAQ

**Does it drain my battery?**
No more than usual. Your Mac just stays awake instead of sleeping.

**What if I interrupt an agent with Ctrl+C?**
Run `asp cleanup` or the tool auto-detects idle sessions after 30 seconds.

**Does it work with multiple agent instances?**
Yes! Mac stays awake until ALL instances finish.

**How do app updates work?**
The menu bar app uses Sparkle. Use `Check for Updates...` from the menu bar app, or let Sparkle check automatically in the background. New signed releases are installed through Sparkle instead of just opening a DMG download.

---

<div align="center">

Made with ☕ for coding agent users

[Report Issue](https://github.com/CharlonTank/agents-sleep-preventer/issues) · [View Releases](https://github.com/CharlonTank/agents-sleep-preventer/releases)

</div>
