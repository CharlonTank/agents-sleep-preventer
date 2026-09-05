use anyhow::{Context, Result};
use serde_json::json;
use std::{fs, path::Path};

pub(crate) fn toml_section_name(line: &str) -> Option<&str> {
    let code = line.split('#').next().unwrap_or("").trim();
    if !code.starts_with('[') || !code.ends_with(']') {
        return None;
    }
    Some(code.trim_matches(&['[', ']'][..]).trim())
}

pub(crate) fn set_toml_feature_true(content: &str, feature: &str) -> String {
    let mut lines = content.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let mut features_start = None;
    let mut features_end = lines.len();

    for (idx, line) in lines.iter().enumerate() {
        let Some(section) = toml_section_name(line) else {
            continue;
        };
        if section == "features" {
            features_start = Some(idx);
            features_end = lines.len();
        } else if features_start.is_some() {
            features_end = idx;
            break;
        }
    }

    if let Some(start) = features_start {
        for line in lines.iter_mut().take(features_end).skip(start + 1) {
            let code = line.split('#').next().unwrap_or("").trim_start();
            if let Some(rest) = code.strip_prefix(feature) {
                if rest.trim_start().starts_with('=') {
                    let indent = line
                        .chars()
                        .take_while(|ch| ch.is_whitespace())
                        .collect::<String>();
                    *line = format!("{}{} = true", indent, feature);
                    return format!("{}\n", lines.join("\n"));
                }
            }
        }
        let mut insert_at = features_end;
        while insert_at > start + 1
            && lines
                .get(insert_at - 1)
                .map(|line| line.trim().is_empty())
                .unwrap_or(false)
        {
            insert_at -= 1;
        }
        lines.insert(insert_at, format!("{} = true", feature));
    } else {
        if !lines.is_empty()
            && lines
                .last()
                .map(|line| !line.trim().is_empty())
                .unwrap_or(false)
        {
            lines.push(String::new());
        }
        lines.push("[features]".to_string());
        lines.push(format!("{} = true", feature));
    }

    format!("{}\n", lines.join("\n"))
}

pub(crate) fn remove_toml_feature(content: &str, feature: &str) -> String {
    let mut changed = false;
    let mut in_features = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if let Some(section) = toml_section_name(line) {
            in_features = section == "features";
        }

        if in_features {
            let code = line.split('#').next().unwrap_or("").trim_start();
            if let Some(rest) = code.strip_prefix(feature) {
                if rest.trim_start().starts_with('=') {
                    changed = true;
                    continue;
                }
            }
        }

        lines.push(line.to_string());
    }

    if changed {
        format!("{}\n", lines.join("\n"))
    } else {
        content.to_string()
    }
}

pub(crate) fn set_codex_hooks_feature(content: &str) -> String {
    let without_legacy = remove_toml_feature(content, "codex_hooks");
    set_toml_feature_true(&without_legacy, "hooks")
}

pub(crate) fn enable_codex_hooks_feature(config_file: &Path) -> Result<()> {
    let content = fs::read_to_string(config_file).unwrap_or_default();
    let updated = set_codex_hooks_feature(&content);
    if updated != content {
        fs::write(config_file, updated)
            .with_context(|| format!("Failed to write {}", config_file.display()))?;
    }
    Ok(())
}

pub(crate) fn hook_value_contains_marker(value: &serde_json::Value, markers: &[&str]) -> bool {
    match value {
        serde_json::Value::String(text) => markers.iter().any(|marker| text.contains(marker)),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| hook_value_contains_marker(value, markers)),
        serde_json::Value::Object(map) => map
            .values()
            .any(|value| hook_value_contains_marker(value, markers)),
        _ => false,
    }
}

pub(crate) fn remove_owned_hooks_from_group(
    group: &mut serde_json::Value,
    markers: &[&str],
) -> bool {
    let Some(hooks) = group
        .get_mut("hooks")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };

    let before = hooks.len();
    hooks.retain(|hook| !hook_value_contains_marker(hook, markers));
    before != hooks.len()
}

pub(crate) fn remove_owned_hook_groups(hooks: &mut serde_json::Value, markers: &[&str]) -> bool {
    let Some(events) = hooks.as_object_mut() else {
        return false;
    };

    let mut changed = false;
    for groups in events.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };

        for group in groups.iter_mut() {
            if remove_owned_hooks_from_group(group, markers) {
                changed = true;
            }
        }

        let before = groups.len();
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(serde_json::Value::as_array)
                .map(|hooks| !hooks.is_empty())
                .unwrap_or(true)
        });
        changed |= before != groups.len();
    }

    changed
}

pub(crate) fn prune_empty_hook_events(hooks: &mut serde_json::Value) {
    if let Some(events) = hooks.as_object_mut() {
        events.retain(|_, groups| {
            groups
                .as_array()
                .map(|groups| !groups.is_empty())
                .unwrap_or(true)
        });
    }
}

pub(crate) fn command_hook_group(command: &str, matcher: Option<&str>) -> serde_json::Value {
    let mut group = json!({
        "hooks": [
            {
                "type": "command",
                "command": command,
                "timeout": 5
            }
        ]
    });
    if let Some(matcher) = matcher {
        group["matcher"] = json!(matcher);
    }
    group
}

pub(crate) fn append_hook_group(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event_name: &str,
    group: serde_json::Value,
) {
    let event = hooks
        .entry(event_name.to_string())
        .or_insert_with(|| json!([]));
    if !event.is_array() {
        *event = json!([]);
    }
    if let Some(groups) = event.as_array_mut() {
        groups.push(group);
    }
}
