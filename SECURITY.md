# Security Policy

## Supported Versions

Only the latest release gets security fixes. The macOS app updates itself through Sparkle, and Homebrew users get it with `brew upgrade --cask agents-sleep-preventer`.

| Version | Supported          |
| ------- | ------------------ |
| 5.2.x   | :white_check_mark: |
| < 5.2   | :x:                |

## Reporting a Vulnerability

Please report vulnerabilities privately through GitHub: open the [Security tab](https://github.com/CharlonTank/agents-sleep-preventer/security) and click **Report a vulnerability**. Do not open a public issue for a security problem.

Include the app version (on macOS: `/Applications/AgentsSleepPreventer.app/Contents/MacOS/asp --version`), your OS version, and the steps to reproduce.

This is a one-person project. You can expect:

- an acknowledgement within a week;
- a fix in a new release as soon as the problem is confirmed, with credit in the release notes if you want it;
- an explanation if the report is declined.

## What counts as a vulnerability

Agents Sleep Preventer runs with a few privileges that matter:

- **A sudoers rule** (`/etc/sudoers.d/agents-pmset`) that lets the app run `pmset -a disablesleep 0|1`, `pmset -a sleep 5` and `pmset sleepnow` without a password, and nothing else.
- **Agent hooks** it adds to `~/.claude/settings.json` (and the other Claude Code profiles) and to `~/.codex/hooks.json`.
- **State shared between the hooks and the app**: working and attention markers in the per-user temporary directory (mode 0700), not in the shared `/tmp`.
- **Microphone and Accessibility permissions** for dictation. Audio is transcribed on the Mac and never sent anywhere. Dictated text is never written to the logs.
- **Updates**: the DMG is signed with a Developer ID and notarized by Apple, and the Sparkle feed is signed with an EdDSA key.

Examples of what to report: running any other command through the sudoers rule, another local user forging markers or notifications, getting dictated text out of the app, or installing an update that was not signed by the project.

Out of scope: attacks that need root or control of your user account already, or that only keep a Mac awake or put it to sleep when you could already do that yourself.
