# Claude Code Guidelines

## Meta

- **Keep this file updated**: When you add new scripts, processes, or important patterns, update this CLAUDE.md file so future sessions have accurate context.

## Code Quality

- NEVER use `_variable` patterns to silence unused variable warnings. This indicates bad design or legacy code. If a variable is unused, remove the logic that produces it entirely. We NEVER want legacy code.

## Testing / Clean Install

Before testing a new build, run the Rust cleanup task to ensure a fresh state:

```bash
cargo xtask clean
```

Optional: keep Whisper models (~500 MB) and whisper-cli:

```bash
cargo xtask clean --keep-model
```

Default workflow after any code change:

```bash
cargo xtask complete-test --skip-notarize --keep-model
```

This cleans the system, builds the DMG, and opens it so the new app can be installed and launched.
When I make any installation-related change, I will run `cargo xtask complete-test --skip-notarize --keep-model` immediately after so you can test without waiting.
If you want to run xtask without password prompts, see `SUDOERS_SETUP.md`.

This removes:
- App from /Applications
- App data, logs, caches (optionally keeping models)
- LaunchAgents
- Claude Code hooks
- ASP-owned Codex hooks from `~/.codex/hooks.json`
- Sudoers config
- TCC permissions (Input Monitoring, Microphone, Accessibility)
- Whisper CLI + models (Homebrew paths and /tmp build), unless `--keep-model`

## Dev scripts (Rust only)

- `cargo xtask complete-test --skip-notarize` (clean system, build DMG, open it)
- `cargo xtask complete-test --skip-notarize --keep-model` (same but keeps models + whisper-cli)
- `cargo xtask build-dmg --skip-notarize` (local DMG build only)
- `cargo xtask replace-app --open` (rebuild + replace /Applications app)
- `cargo xtask release X.Y.Z` (bump, build DMG, notarize, generate signed appcast)
- `cargo xtask release X.Y.Z --upload` (only after committing/pushing the version bump; creates/updates GitHub release, marks it latest, uploads DMG + appcast, verifies Sparkle feed)

## Uninstall

- `asp uninstall` removes app data by default; use `-k`/`--keep-model` to preserve Whisper models (~500 MB).
- `asp install` configures Claude Code hooks in `~/.claude/settings.json` and Codex hooks in `~/.codex/hooks.json`; it also enables `hooks = true` in `~/.codex/config.toml`.

## Release Process

To publish a new version:

1. `cargo xtask release X.Y.Z` (bumps `Cargo.toml`, `Cargo.lock`, `Info.plist`, `README.md`, package distribution XML, builds signed DMG, notarizes, generates signed appcast)
2. Review the generated app locally.
3. Commit and push the version bump/release changes.
4. `cargo xtask release X.Y.Z --upload` (requires a clean pushed HEAD; creates or updates `vX.Y.Z`, marks it latest, uploads the DMG and `appcast.xml`, verifies the release assets and latest Sparkle feed)

**IMPORTANT**: The keychain profile is `"notary"` (NOT "notarytool").

**IMPORTANT**: Update the version number in README.md download links when releasing a new version.

**IMPORTANT**: The menu bar app uses Sparkle with `https://github.com/CharlonTank/agents-sleep-preventer/releases/latest/download/appcast.xml` as the feed URL. Keep semver tags in the `vX.Y.Z` format and publish both the DMG and `appcast.xml` asset on every release.

**IMPORTANT**: Sparkle appcast signing prefers the keychain account `"CharlonTank-agents-sleep-preventer"` and falls back to the legacy `"CharlonTank-claude-sleep-preventer"` account while migrating existing developer machines.

## Dictation Engines

Two engines behind the model picker in Settings (`src/settings/mod.rs` `ModelEngine`):

- **Whisper** (whisper.cpp): shells out to the bundled `whisper-cli` with a GGML `.bin` model. Supports language selection and vocabulary prompt.
- **Parakeet v3** (transcribe-rs crate, feature `onnx`, ONNX Runtime statically linked): in-process transcription, ~10x faster than Whisper, auto language (25 European languages), no vocabulary support. Model = 4 files downloaded from HF `istupakov/parakeet-tdt-0.6b-v3-onnx` into `models/parakeet-tdt-0.6b-v3-int8/`.

Integration test: `cargo test --test parakeet_integration` (skips if the model isn't downloaded). Requires rustc >= 1.88.

## Sleep Prevention Logic

- `sync_sleep_state`: `should_prevent = !thermal && match force { awake => true, sleep => false, auto => manual_enabled && active_pids > 0 }`. The force override (popover tri-state, `asp force awake|sleep|auto`) is stored in `settings.json` and read fresh on every sync so all asp processes react instantly. `asp reset` clears it back to auto.
- Stop hooks fire at end-of-turn even while background work (Claude Workflow/ultracode, Codex /ultra subagents — both in-process) continues. `cmd_stop` therefore keeps the PID marker while the agent's process tree is busy (self ≥ 0.5% CPU or any descendant ≥ 5%); `cleanup_stale_pids` removes it once the tree is quiet for 30s.
- Claude Code keep-awake hook events: UserPromptSubmit, PreToolUse, PostToolUse, PreCompact, SubagentStart, SubagentStop (the subagent events refresh the marker during multi-agent orchestration).
- `asp install` MERGES into `~/.claude/settings.json` hooks (prunes ASP-owned groups by marker, preserves user hooks). Uninstall/`xtask clean` strip only ASP-owned entries and only ASP's three scripts in `~/.claude/hooks/` — never `rm -rf` the hooks dir (users keep their own scripts there).

## Agent Notifications

Hooks spool JSON to `/tmp/asp_notifications/`; the running app (menubar or agent loop) drains it every ~1-2s and posts via `NSUserNotificationCenter` (`src/notifications.rs`).

- Task finished: `cmd_stop` notifies if the PID file is older than `TASK_DONE_MIN_SECS` (45s) — shorter tasks mean the user is still watching.
- Needs attention: Claude Code `Notification` hook → `~/.claude/hooks/agent-attention.sh` → `asp attention` (reads hook JSON on stdin, extracts `.message`).
- Toggle in Settings tab 1 (`notifications.enabled`, default true).

## macOS Permissions Notes

The app requests only TWO permissions: Microphone and Accessibility.

- **Microphone**: App must call `AVCaptureDevice.requestAccessForMediaType:` to appear in System Preferences list. The system dialog triggers automatically.
- **Accessibility**: Check with `AXIsProcessTrusted()`. Request with `AXIsProcessTrustedWithOptions` + `kAXTrustedCheckOptionPrompt` — this shows the system dialog AND auto-adds the app to the Accessibility list (user just flips the switch, no manual "+"). The prompt shows only once per app; later calls are no-ops, so also open `x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility` as fallback.
- **Input Monitoring is NOT requested**: in TCC, Accessibility is a superset that covers listen-only CGEventTaps (same model as espanso/Hammerspoon). Since text injection via CGEventPost needs Accessibility anyway, Input Monitoring would be redundant. Do not re-add it.

## AppleScript Gotchas

- `--` in AppleScript starts a comment. Use short flags like `-y` instead of `--yes` when running commands via AppleScript.
- Use `osascript -e "..."` via `Command::new()` instead of `NSAppleScript` - it's more reliable.
