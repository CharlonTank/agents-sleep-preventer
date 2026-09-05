use crate::hook_config::{
    append_hook_group, command_hook_group, enable_codex_hooks_feature, prune_empty_hook_events,
    remove_owned_hook_groups,
};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{fs, path::Path};

// Stable ownership marker, independent of where asp.exe was downloaded.
const SCRIPT_HEADER: &str = "# Agents Sleep Preventer Windows\n";

fn encode(script: &str) -> String {
    STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}

fn owned_marker() -> String {
    // The header is 33 UTF-16 code units: no base64 padding, so this prefix
    // identifies our commands regardless of installation path or action.
    format!("-EncodedCommand {}", encode(SCRIPT_HEADER))
}

pub fn command(binary: &Path, action: &str) -> String {
    // Forward slashes and double quotes work in both cmd.exe and Git Bash.
    // PowerShell is invoked explicitly by its executable, avoiding an assumed
    // hook shell. EncodedCommand also handles spaces, $, &, and apostrophes.
    let path = binary
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "''");
    let script = format!("{SCRIPT_HEADER}& '{path}' {action} --asp-hook; exit $LASTEXITCODE");
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {}",
        encode(&script)
    )
}

pub fn update_file(path: &Path, binary: Option<&Path>, claude: bool) -> Result<()> {
    if binary.is_none() && !path.exists() {
        return Ok(());
    }
    let original = if path.exists() {
        fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?
    } else {
        "{}".into()
    };
    let mut config: Value = serde_json::from_str(&original)
        .with_context(|| format!("Invalid JSON in {}; file left unchanged", path.display()))?;
    ensure!(
        config.is_object(),
        "{} must contain a JSON object",
        path.display()
    );
    if config.get("hooks").is_none() {
        config["hooks"] = json!({});
    }
    ensure!(
        config["hooks"].is_object(),
        "hooks must be an object in {}",
        path.display()
    );
    ensure!(
        config["hooks"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_array),
        "Hook events must contain arrays in {}; file left unchanged",
        path.display()
    );
    // Identify owned hooks from the encoded script header, with no extra
    // fields outside the agents' documented command hook schema.
    remove_owned_hook_groups(&mut config["hooks"], &[&owned_marker()]);
    prune_empty_hook_events(&mut config["hooks"]);
    if let Some(binary) = binary {
        let hooks = config["hooks"].as_object_mut().unwrap();
        let mut events = vec![
            ("UserPromptSubmit", "start", None),
            ("PreToolUse", "refresh", Some("*")),
            ("PostToolUse", "refresh", Some("*")),
            ("Stop", "stop", None),
        ];
        if claude {
            events.extend([
                ("PostToolUseFailure", "refresh", Some("*")),
                ("PreCompact", "refresh", None),
                ("SubagentStart", "refresh", None),
                ("SubagentStop", "refresh", None),
                (
                    "Notification",
                    "attention",
                    Some("permission_prompt|idle_prompt"),
                ),
                ("SessionEnd", "stop", None),
            ]);
        }
        for (event, action, matcher) in events {
            let group = command_hook_group(&command(binary, action), matcher);
            append_hook_group(hooks, event, group);
        }
    }
    let updated = serde_json::to_string_pretty(&config)?;
    if updated != original {
        fs::create_dir_all(path.parent().context("Missing hooks directory")?)?;
        // Keep the user's original config recoverable across repeated installs.
        let backup = path.with_extension("json.asp-backup");
        if path.exists() && !backup.exists() {
            fs::copy(path, backup)?;
        }
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        use std::io::Write;
        temp.write_all(updated.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
    }
    Ok(())
}

pub fn install(home: &Path, binary: &Path) -> Result<()> {
    update_file(&home.join(".claude/settings.json"), Some(binary), true)?;
    update_file(&home.join(".codex/hooks.json"), Some(binary), false)?;
    let config = home.join(".codex/config.toml");
    let backup = config.with_extension("toml.asp-backup");
    if config.exists() && !backup.exists() {
        fs::copy(&config, backup)?;
    }
    enable_codex_hooks_feature(&config)
}

pub fn uninstall(home: &Path) -> Result<()> {
    update_file(&home.join(".claude/settings.json"), None, true)?;
    update_file(&home.join(".codex/hooks.json"), None, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reinstall_and_uninstall_preserve_other_hooks_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = json!({"theme": "dark", "hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "echo custom"},
            {"type": "command", "command": command(Path::new("C:/Old/asp.exe"), "stop")}
        ]}]}});
        fs::write(&path, original.to_string()).unwrap();
        let binary = Path::new("C:\\Users\\O'Brien & Co\\asp.exe");
        update_file(&path, Some(binary), true).unwrap();
        let installed = fs::read_to_string(&path).unwrap();
        update_file(&path, Some(binary), true).unwrap();
        assert_eq!(installed, fs::read_to_string(&path).unwrap());
        update_file(&path, None, true).unwrap();
        let removed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(removed["theme"], "dark");
        assert_eq!(removed["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            removed["hooks"]["Stop"][0]["hooks"],
            json!([{"type":"command","command":"echo custom"}])
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                &fs::read_to_string(path.with_extension("json.asp-backup")).unwrap()
            )
            .unwrap(),
            original
        );
    }

    #[test]
    fn invalid_config_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        for content in ["{broken", "[]", "{\"hooks\":[]}"] {
            fs::write(&path, content).unwrap();
            assert!(update_file(&path, Some(Path::new("asp.exe")), false).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn encoded_commands_preserve_special_paths_and_have_stable_ownership() {
        let command = command(Path::new("C:\\Users\\O'Brien & $Co\\asp.exe"), "start");
        assert!(command.contains(&owned_marker()));
        let encoded = command.split_whitespace().last().unwrap();
        let bytes = STANDARD.decode(encoded).unwrap();
        let words = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect::<Vec<_>>();
        let decoded = String::from_utf16(&words).unwrap();
        assert_eq!(decoded, format!("{SCRIPT_HEADER}& 'C:/Users/O''Brien & $Co/asp.exe' start --asp-hook; exit $LASTEXITCODE"));
    }

    #[test]
    fn install_migrates_codex_flag_and_uninstall_keeps_shared_flag() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        let config = dir.path().join(".codex/config.toml");
        fs::write(&config, "[features]\ncodex_hooks = true\nother = true\n").unwrap();
        install(dir.path(), Path::new("C:/Apps/asp.exe")).unwrap();
        uninstall(dir.path()).unwrap();
        let result = fs::read_to_string(config).unwrap();
        assert!(result.contains("hooks = true"));
        assert!(result.contains("other = true"));
        assert!(!result.contains("codex_hooks"));
    }
}
