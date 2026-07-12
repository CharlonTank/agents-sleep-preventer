# Claude Code Guidelines

## Meta

- **Keep this file updated**: When you add new scripts, processes, or important patterns, update this AGENTS.md file so future sessions have accurate context.

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

## Menu UI preview

- `ASP_UI_PREVIEW=1` makes the Swift menu bar executable show deterministic fixture data and skips the agent process, hooks prompt, hotkeys, refresh timer, and updater startup.
- Use this mode for visual checks without replacing the app in `/Applications` or changing system configuration.

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

## macOS Permissions Notes

- **Microphone**: App must call `AVCaptureDevice.requestAccessForMediaType:` to appear in System Preferences list. The system dialog triggers automatically.
- **Accessibility**: Check with `AXIsProcessTrusted()`. Open preferences with `x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility`
- **Input Monitoring**: Probe with a listen-only CGEventTap; open preferences with `x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent`

## AppleScript Gotchas

- `--` in AppleScript starts a comment. Use short flags like `-y` instead of `--yes` when running commands via AppleScript.
- Use `osascript -e "..."` via `Command::new()` instead of `NSAppleScript` - it's more reliable.
