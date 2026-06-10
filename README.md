<div align="center">

# ☕ Agents Sleep Preventer

### Keep your Mac awake while coding agents are working
**Sleep prevention, local dictation, and agent controls in one macOS menu bar app.**

<br>

[![Download DMG](https://img.shields.io/badge/Download-DMG%20Installer-blue?style=for-the-badge&logo=apple)](https://github.com/CharlonTank/agents-sleep-preventer/releases/latest/download/AgentsSleepPreventer-4.0.3.dmg)

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

Install this tool. Now your Mac stays awake while your agent works, even with the lid closed. When it finishes, normal sleep resumes. The same menu bar app also provides local speech-to-text dictation for any app.

<div align="center">

| Before | After |
|--------|-------|
| 😴 Lid closed = Mac sleeps | ☕ Lid closed = agent keeps working |
| 🔄 Come back to interrupted work | ✅ Come back to finished work |

</div>

---

## Features

- Sleep prevention while supported coding agents are active
- Local voice dictation with Fn+Shift
- English transcription by default, with language selection in Settings
- Custom vocabulary words to improve transcription accuracy
- Sparkle updates from signed releases

---

## Installation

### 🍎 Download DMG (Easiest)

1. [Download the latest DMG](https://github.com/CharlonTank/agents-sleep-preventer/releases/latest/download/AgentsSleepPreventer-4.0.3.dmg)
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

1. Open the app and allow Input Monitoring, Microphone, and Accessibility permissions.
2. Download the local Whisper model when prompted.
3. Press and hold Fn+Shift to record, then release to transcribe and insert text.

Dictation starts in English by default. To change it, open `Settings...`, select the `Dictation` tab, and choose a language or `Auto-detect`.

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
