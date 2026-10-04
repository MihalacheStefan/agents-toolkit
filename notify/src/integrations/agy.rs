use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};

use crate::{
    atomic_write,
    error::{AppError, Result},
};

use super::IntegrationStatus;

/// One Antigravity hook installed by ai-notify.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookSpec {
    pub event: &'static str,
    pub command: &'static str,
    pub matcher: Option<&'static str>,
}

/// The single source of truth for installed Antigravity hooks.
pub const HOOK_SPECS: &[HookSpec] = &[
    HookSpec { event: "Stop", command: "ai-notify event agy", matcher: None },
    HookSpec { event: "PreToolUse", command: "ai-notify event agy", matcher: Some("ask_question") },
];

/// The outcome of ensuring an Antigravity hooks file has our hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgyHooksUpdate {
    pub path: PathBuf,
    pub changed: bool,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub skipped: BTreeMap<String, String>,
}

/// Aggregate state across Antigravity's active global and project settings files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgyHooksReport {
    pub status: IntegrationStatus,
    pub paths: Vec<PathBuf>,
    pub missing_events: Vec<String>,
    pub errors: BTreeMap<PathBuf, String>,
    pub ignored_paths: Vec<PathBuf>,
}

impl AgyHooksReport {
    /// The first hooks file that contributes an ai-notify command.
    pub fn path(&self) -> Option<&Path> {
        self.paths.first().map(PathBuf::as_path)
    }

    /// A CLI should use this to turn malformed inspected files into a non-zero result.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Return default Antigravity hooks path: $AGY_CONFIG_DIR/hooks.json or ~/.gemini/config/hooks.json.
pub fn default_hook_path() -> Result<PathBuf> {
    let root = env::var_os("AGY_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME").filter(|home| !home.is_empty()).map(|home| PathBuf::from(home).join(".gemini/config"))
        })
        .ok_or_else(|| AppError::configuration("HOME or AGY_CONFIG_DIR must be set"))?;
    Ok(root.join("hooks.json"))
}

/// Add missing ai-notify Antigravity hooks while preserving all unrelated JSON data.
pub fn ensure_agy_hooks(path: &Path, force: bool, dry_run: bool) -> Result<AgyHooksUpdate> {
    let mut data = load_settings(path)?;
    let root = data.as_object_mut().expect("load_settings validates object roots");
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| AppError::integration(format!("{}: hooks field must be an object", path.display())))?;

    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut skipped = BTreeMap::new();

    for spec in HOOK_SPECS {
        match hooks.get_mut(spec.event) {
            Some(existing @ Value::Array(_)) => {
                if !spec_present(existing, *spec) {
                    existing.as_array_mut().expect("matched an array").push(build_group(*spec));
                    added.push(spec.event.to_owned());
                }
            }
            Some(existing) if command_present(existing, spec.command) => {
                *existing = Value::Array(vec![build_group(*spec)]);
                updated.push(spec.event.to_owned());
            }
            Some(existing) if force => {
                *existing = Value::Array(vec![build_group(*spec)]);
                updated.push(spec.event.to_owned());
            }
            Some(existing) => {
                skipped.insert(spec.event.to_owned(), summarize_hook(existing));
            }
            None => {
                hooks.insert(spec.event.to_owned(), Value::Array(vec![build_group(*spec)]));
                added.push(spec.event.to_owned());
            }
        }
    }

    let changed = !added.is_empty() || !updated.is_empty();
    if changed && !dry_run {
        let rendered = serde_json::to_string_pretty(&data)
            .map_err(|error| AppError::integration(format!("failed to render {}: {error}", path.display())))? +
            "\n";
        atomic_write::replace(path, rendered)?;
    }

    Ok(AgyHooksUpdate { path: path.to_path_buf(), changed, added, updated, skipped })
}

/// Inspect Antigravity's global and project hooks locations.
pub fn inspect_agy_hooks(config_root: &Path, project_root: &Path) -> AgyHooksReport {
    let active_paths = [
        config_root.join("hooks.json"),
        project_root.join(".gemini/config/hooks.json"),
        project_root.join(".agents/hooks.json"),
    ];
    let ignored_paths = Vec::new();

    let mut installed_events = BTreeSet::new();
    let mut errors = BTreeMap::new();
    let mut paths = Vec::new();

    for path in active_paths {
        if !path.exists() {
            continue;
        }

        let data = match load_settings(&path) {
            Ok(data) => data,
            Err(error) => {
                errors.insert(path, error.message);
                continue;
            }
        };
        let Some(hooks) = data.get("hooks") else {
            continue;
        };
        let Some(hooks) = hooks.as_object() else {
            errors.insert(path, "hooks field must be an object".to_owned());
            continue;
        };

        let mut path_has_command = false;
        for spec in HOOK_SPECS {
            if hooks.get(spec.event).is_some_and(|value| spec_present(value, *spec)) {
                path_has_command = true;
                installed_events.insert(spec.event);
            }
        }
        if path_has_command {
            paths.push(path);
        }
    }

    let missing_events = HOOK_SPECS
        .iter()
        .filter(|spec| !installed_events.contains(spec.event))
        .map(|spec| spec.event.to_owned())
        .collect::<Vec<_>>();
    let status = if missing_events.is_empty() {
        IntegrationStatus::Ok
    } else if paths.is_empty() {
        IntegrationStatus::Missing
    } else {
        IntegrationStatus::Partial
    };

    AgyHooksReport { status, paths, missing_events, errors, ignored_paths }
}

fn load_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let text = fs::read_to_string(path)?;
    let data: Value = serde_json::from_str(&text)
        .map_err(|error| AppError::integration(format!("failed to parse {}: {error}", path.display())))?;
    if !data.is_object() {
        return Err(AppError::integration(format!("{} must contain a JSON object at the root", path.display())));
    }
    Ok(data)
}

fn build_group(spec: HookSpec) -> Value {
    let mut group = Map::new();
    if let Some(matcher) = spec.matcher {
        group.insert("matcher".to_owned(), Value::String(matcher.to_owned()));
    }
    group.insert("hooks".to_owned(), json!([{ "type": "command", "command": spec.command }]));
    Value::Object(group)
}

fn iter_hook_commands(value: &Value) -> Vec<String> {
    let mut commands = Vec::new();
    collect_hook_commands(value, &mut commands);
    commands
}

fn collect_hook_commands(value: &Value, commands: &mut Vec<String>) {
    match value {
        Value::String(command) => commands.push(command.clone()),
        Value::Array(items) => {
            for item in items {
                collect_hook_commands(item, commands);
            }
        }
        Value::Object(object) => {
            if let Some(Value::String(command)) = object.get("command") {
                commands.push(command.clone());
            }
            if let Some(hooks) = object.get("hooks") {
                collect_hook_commands(hooks, commands);
            }
        }
        _ => {}
    }
}

fn command_present(value: &Value, expected: &str) -> bool {
    iter_hook_commands(value).iter().any(|command| command.trim() == expected)
}

fn spec_present(value: &Value, spec: HookSpec) -> bool {
    value.as_array().is_some_and(|groups| {
        groups.iter().any(|group| {
            let Some(group) = group.as_object() else {
                return false;
            };
            let matcher = group.get("matcher").and_then(Value::as_str);
            let matcher_matches = match spec.matcher {
                Some(expected) => matcher == Some(expected),
                None => matches!(matcher, None | Some("") | Some("*")),
            };
            matcher_matches &&
                group.get("hooks").and_then(Value::as_array).is_some_and(|hooks| {
                    hooks.iter().any(|hook| {
                        hook.as_object().is_some_and(|hook| {
                            hook.get("type").and_then(Value::as_str) == Some("command") &&
                                hook.get("command")
                                    .and_then(Value::as_str)
                                    .is_some_and(|command| command.trim() == spec.command)
                        })
                    })
                })
        })
    })
}

fn summarize_hook(value: &Value) -> String {
    match value {
        Value::String(command) => command.clone(),
        Value::Object(object) => object
            .get("command")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "<object>".to_owned()),
        Value::Array(items) => format!("<list:{}>", items.len()),
        _ => "<unknown>".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn installs_all_hooks_with_the_pre_tool_matcher() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("hooks.json");

        let update = ensure_agy_hooks(&path, false, false).unwrap();
        let settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        assert_eq!(update.added.len(), HOOK_SPECS.len());
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "ask_question");
        assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["type"], "command");
        assert!(fs::read_to_string(path).unwrap().ends_with('\n'));
    }

    #[test]
    fn preserves_list_groups_and_migrates_trimmed_legacy_commands() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("hooks.json");
        fs::write(
            &path,
            serde_json::to_string(&json!({
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": "ai-coord hook agy"}]}],
                    "PreToolUse": {"command": " ai-notify event agy  "}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let update = ensure_agy_hooks(&path, false, false).unwrap();
        let settings: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();

        assert!(update.added.contains(&"Stop".to_owned()));
        assert!(update.updated.contains(&"PreToolUse".to_owned()));
        assert_eq!(iter_hook_commands(&settings["hooks"]["Stop"]).len(), 2);
    }

    #[test]
    fn skips_foreign_non_lists_unless_forced_and_never_writes_invalid_json() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("hooks.json");
        fs::write(&path, r#"{"hooks":{"Stop":{"command":"echo stop"}}}"#).unwrap();

        let update = ensure_agy_hooks(&path, false, false).unwrap();
        assert_eq!(update.skipped["Stop"], "echo stop");
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap()["hooks"]["Stop"]["command"],
            "echo stop"
        );

        fs::write(&path, "{").unwrap();
        assert!(ensure_agy_hooks(&path, false, false).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{");
    }

    #[test]
    fn inspector_merges_active_locations() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config");
        let project = directory.path().join("project");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(project.join(".gemini/config")).unwrap();

        let groups = |specs: &[HookSpec]| {
            let mut hooks = Map::new();
            for spec in specs {
                hooks.insert(spec.event.to_owned(), Value::Array(vec![build_group(*spec)]));
            }
            Value::Object(Map::from_iter([(String::from("hooks"), Value::Object(hooks))]))
        };
        fs::write(config.join("hooks.json"), serde_json::to_string(&groups(&HOOK_SPECS[..1])).unwrap()).unwrap();
        fs::write(project.join(".gemini/config/hooks.json"), serde_json::to_string(&groups(&HOOK_SPECS[1..])).unwrap())
            .unwrap();

        let report = inspect_agy_hooks(&config, &project);
        assert_eq!(report.status, IntegrationStatus::Ok);
        assert_eq!(report.paths.len(), 2);
    }
}
