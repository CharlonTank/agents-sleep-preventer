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

1. `cargo xtask release X.Y.Z` (bumps `Cargo.toml`, `Cargo.lock`, `Info.plist`, `README.md`, builds signed DMG, notarizes, generates signed appcast)
2. Review the generated app locally.
3. Commit and push the version bump/release changes.
4. `cargo xtask release X.Y.Z --upload` (requires a clean pushed HEAD; creates or updates `vX.Y.Z`, marks it latest, uploads the DMG and `appcast.xml`, verifies the release assets and latest Sparkle feed)
5. Bump the Homebrew cask: `$(brew --repository charlontank/tap)/Casks/agents-sleep-preventer.rb` (`version` + `sha256` of the DMG from `shasum -a 256`), `brew style` it, commit and push the tap.

**IMPORTANT**: Minimum macOS is `MACOS_DEPLOYMENT_TARGET` in xtask (14.0) = `LSMinimumSystemVersion` in Info.plist; keep them equal. swiftc gets `-target arm64-apple-macos14.0`, cmake `CMAKE_OSX_DEPLOYMENT_TARGET`, cargo/cc `MACOSX_DEPLOYMENT_TARGET`. Without them every tool defaults to the build machine's macOS (5.0.7–5.2.0 shipped requiring macOS 27). `verify_deployment_target` fails the build if any bundled binary's `minos` is higher. whisper.cpp needs ≥ 13.3 (new Accelerate `cblas_sgemm`). whisper-cli is built in `/tmp/whisper.cpp/build-portable` with `GGML_NATIVE=OFF` + `-march=armv8.4-a+dotprod+fp16` (M1 baseline), never `-mcpu=native` of the build Mac.

**IMPORTANT**: The app icon source is `AppIcon.icon` (Icon Composer package: `icon.json` + one full-bleed opaque 1024 px `Assets/artwork.png`, no rounded corners or margin). `compile_app_icon` runs `xcrun actool` at build time to produce `Assets.car` (macOS 26+, which draws the squircle itself) and `AppIcon.icns` (older macOS); Info.plist sets both `CFBundleIconName` and `CFBundleIconFile`. Never ship a hand-made icns alone: macOS 26+ shrinks legacy icons onto a gray plate. The artwork is rendered from `assets/logo-animated.svg`'s geometry.

**IMPORTANT**: The keychain profile is `"notary"` (NOT "notarytool").

**IMPORTANT**: Update the version number in README.md download links when releasing a new version.

**IMPORTANT**: The menu bar app uses Sparkle with `https://github.com/CharlonTank/agents-sleep-preventer/releases/latest/download/appcast.xml` as the feed URL. Keep semver tags in the `vX.Y.Z` format and publish both the DMG and `appcast.xml` asset on every release.

**IMPORTANT**: Sparkle appcast signing prefers the keychain account `"CharlonTank-agents-sleep-preventer"` and falls back to the legacy `"CharlonTank-claude-sleep-preventer"` account while migrating existing developer machines.

## Dictation Engines

Two engines behind the model picker in Settings (`src/settings/mod.rs` `ModelEngine`):

- **Whisper** (whisper.cpp): shells out to the bundled `whisper-cli` with a GGML `.bin` model. Supports language selection and vocabulary prompt.
- **Parakeet v3** (transcribe-rs crate, feature `onnx`, ONNX Runtime statically linked): in-process transcription, ~10x faster than Whisper, auto language (25 European languages), no vocabulary support. Model = 4 files downloaded from HF `istupakov/parakeet-tdt-0.6b-v3-onnx` into `models/parakeet-tdt-0.6b-v3-int8/`.

Vocabulary words: Whisper also gets them as `--prompt`, but Parakeet cannot be prompted, so every transcript (final text and live preview, macOS and Windows) goes through `src/vocabulary.rs` `correct()`: a word or 2-3 adjacent words whose rough pronunciation key equals a vocabulary entry, or (keys ≥ 5, same first two sounds) differs by one vowel/similar consonant or one extra consonant, is replaced by the entry. Real words that differ more ("climat") are deliberately left. Keep it strict; check changes against the real `dictation-history.txt` for false positives.

Shortcut gesture (`HotkeyGesture` in `src/dictation/mod.rs`, macOS): hold = push-to-talk; a press shorter than `TAP_MAX` (300 ms) is a tap and its recording is discarded; two taps whose presses are within `DOUBLE_TAP_WINDOW` (500 ms) lock recording on (hands-free) until the next press, which stops and transcribes (its release is ignored).

Integration test: `cargo test --test parakeet_integration` (skips if the model isn't downloaded). Requires rustc >= 1.88.

## Sleep Prevention Logic

- `sync_sleep_state`: `should_prevent = !thermal && !sleep_until_user_returns && match force { awake => true, sleep => false, when-done => active_pids > 0, auto => manual_enabled && active_pids > 0 }`. The force override (popover 4-segment control, `asp force awake|sleep|when-done|auto`) is stored in `settings.json` and read fresh on every sync so all asp processes react instantly. `asp reset` clears it back to auto.
- "Sleep when done" (`sleep_when_done_tick`, long-lived app process only): once no agent has been working for 30s and there was no user input for 60s, it saves the mode back to auto FIRST, then runs `pmset sleepnow`. If the save fails it does not sleep (otherwise every wake would sleep again).
- "Sleep when done" time limit (`sleep_prevention.sleep_by`, unix secs; `asp force when-done --within 1h30`; popover pull-down never / 30 min … 12 h plus "Custom…" (any duration typed by the user, parsed by `parse_duration_minutes`: 17h, 20 heures, 1h30, 45m), counted from the click so it also extends): past it the tick sleeps even with agents working (agents polling every few minutes would otherwise keep the Mac up all night), still only after 60s without input. It then sets `sleep_until_user_returns`, which makes `sync_sleep_state` stop preventing sleep (so a background wake can't be held awake by those agents) until input is seen again (`seconds_since_last_user_input() < 60`; the idle counter may not run during sleep, so never compare it with wall-clock time). Any `asp force`/`asp reset` clears both. The settings window must preserve them on save, like `force`.
- Re-enabling sleep only force-sleeps (`pmset sleepnow`) when the lid is closed AND no external display is active AND no input for 60s (`is_lid_closed_and_unattended`). A docked clamshell MacBook reports a closed lid and must never be slept mid-use (issue #6).
- Hook ↔ app state (working-PID markers, attention markers, notification spool) lives in the per-user temp dir (`src/runtime_dir.rs`, `confstr(_CS_DARWIN_USER_TEMP_DIR)`/AgentsSleepPreventer, mode 0700), never in the shared /tmp: other local users could forge markers there.
- Never log dictated text (plain log kept forever); log lengths only.
- The installed sudoers rule (`/etc/sudoers.d/agents-pmset`) only allows `pmset -a disablesleep 0|1`, `pmset -a sleep 5` and `pmset sleepnow`. Any new sudo pmset invocation must be added there too.
- Claude Code agent-team teammates (and post-update sessions) run the native binary directly as `…/claude/versions/<version>`: `classify_agent_process` must accept that path, not only an executable named `claude` (`is_claude_versioned_binary`).
- A descendant shell sourcing `…/shell-snapshots/snapshot-*` is a running Bash tool command; after Stop it means a background task, so `process_tree_is_busy` counts it even at 0% CPU (network-bound benchmarks, polling `sleep`).
- Stop hooks fire at end-of-turn even while background work (Claude Workflow/ultracode, Codex /ultra subagents — both in-process) continues. `cmd_stop` therefore keeps the PID marker while the agent's process tree is busy (self ≥ 0.5% CPU or any descendant ≥ 5%); `cleanup_stale_pids` removes it once the tree is quiet for 30s.
- Codex ≥ 0.160 runs the CLI sessions' turns and hooks in a shared `codex app-server --managed-daemon` (`~/.codex/packages/app-server-daemon/…`, child of launchd), so the session's terminal process is not an ancestor of the hook. When `find_agent_ancestor` meets a Codex service it reads the hook JSON (`session_id`, `cwd`) and `codex_hook_session_pid` picks the busiest Codex process in that cwd not already claimed by another live session, remembering the choice in `runtime_dir::codex_sessions_dir()/<session_id>`. No match (desktop-app thread) → the service pid holds the marker (keeps the Mac awake, no menu row). Codex services (`app-server`, also `codex -c … app-server`, and `exec-server`) are never agents: `codex_subcommand` skips options and their values.
- Claude Code keep-awake hook events: UserPromptSubmit, PreToolUse, PostToolUse, PreCompact, SubagentStart, SubagentStop (the subagent events refresh the marker during multi-agent orchestration).
- Claude Code profiles: sessions started with another `CLAUDE_CONFIG_DIR` (`~/.claude-evo`, `~/.claude-cc`…) read that profile's settings.json. `install_claude_profile_hooks` merges ASP's hooks (pointing at the single copy of the scripts in `~/.claude/hooks`) into every `~/.claude-*` dir that has a `projects/` dir, on install and from the agent loop at startup + every 60s. Running sessions pick the hooks up live. Uninstall strips them from every profile.
- `asp install` MERGES into `~/.claude/settings.json` hooks (prunes ASP-owned groups by marker, preserves user hooks). Uninstall/`xtask clean` strip only ASP-owned entries and only ASP's three scripts in `~/.claude/hooks/` — never `rm -rf` the hooks dir (users keep their own scripts there).

## Agent Notifications

Hooks spool JSON to `runtime_dir::notifications_dir()`; the running app (menubar or agent loop) drains it every ~1-2s and posts via `NSUserNotificationCenter` (`src/notifications.rs`).

- Task finished: `cmd_stop` notifies if the PID file is older than `TASK_DONE_MIN_SECS` (45s) — shorter tasks mean the user is still watching.
- Needs attention: Claude Code `Notification` hook → `~/.claude/hooks/agent-attention.sh` → `asp attention` (reads hook JSON on stdin, extracts `.message`). The attention marker file stores that message; `asp list` returns it as `attention_reason` and the menu shows it under the project.
- "Needs you" reconciliation reads Claude Code's own `<profile>/sessions/<pid>.json` status in every profile dir (`claude_config_dirs`), not only `~/.claude`: a session on `~/.claude-evo` otherwise stayed "Needs you" for up to 6h.
- `asp list` also returns each agent's running Bash tasks (`tasks`: outermost `shell-snapshots` wrapper + the command from its `eval '…'`). The menu lists them with a Stop button → `asp stop-tasks <agent pid>…`, which SIGTERMs each task's whole process tree (only Bash tasks of live agents, never the caller's own chain).
- `claude --chrome-native-host` is Chrome's extension bridge, not a session: `classify_agent_process` ignores it.
- Toggle in Settings tab 1 (`notifications.enabled`, default true).

## macOS Permissions Notes

The app requests only TWO permissions: Microphone and Accessibility.

- **Install location**: the Swift app (`relocateToApplicationsIfNeeded`) asks to move itself to /Applications at the very start of `applicationDidFinishLaunching`, before the agent or any permission UI: translocated copies (DMG, ~/Downloads) show the Accessibility switch on while `AXIsProcessTrusted()` stays false. It quits other running instances, copies with `ditto --noqtn` via a staging bundle, relaunches, and terminates itself. The Rust agent no longer asks (it only exited the agent and left the translocated app running).
- **Single permissions UI**: the Swift "ASP Permissions" panel (shown at launch when something is missing, and from the menu). The Rust first-launch onboarding (`run_onboarding_if_needed`) only offers the dictation model download; don't reintroduce a second permissions window.
- **Stale grant**: the permissions panel offers "Switch already on but still red? Reset it…" → `tccutil reset Accessibility <bundle id>` + re-prompt, for a grant recorded against another copy/signature.
- **Microphone**: App must call `AVCaptureDevice.requestAccessForMediaType:` to appear in System Preferences list. The system dialog triggers automatically.
- **Accessibility**: Check with `AXIsProcessTrusted()`. Request (Swift panel) with `AXIsProcessTrustedWithOptions` + `kAXTrustedCheckOptionPrompt` — this shows the system dialog AND auto-adds the app to the Accessibility list (user just flips the switch, no manual "+"). The prompt shows only once per app; later calls are no-ops, so also open `x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility` as fallback.
- **Input Monitoring is NOT requested**: in TCC, Accessibility is a superset that covers listen-only CGEventTaps (same model as espanso/Hammerspoon). Since text injection via CGEventPost needs Accessibility anyway, Input Monitoring would be redundant. Do not re-add it.

## AppleScript Gotchas

- `--` in AppleScript starts a comment. Use short flags like `-y` instead of `--yes` when running commands via AppleScript.
- Use `osascript -e "..."` via `Command::new()` instead of `NSAppleScript` - it's more reliable.
