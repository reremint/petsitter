//! Optional agent hooks that report state petsitter cannot infer from files.
//!
//! Codex does not record approval prompts in its rollout files. When the user
//! opts in with `petsitter hooks install codex`, Codex runs `petsitter hook
//! codex` on lifecycle events; the hook writes a tiny status file per session
//! that the Codex collector reads.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::collectors::summarize_tool_input;

/// Hook events petsitter subscribes to.
const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
];

/// The subcommand petsitter's Codex hook runs, after its executable.
const HOOK_SUBCOMMAND: &str = " hook codex";
/// Status files older than this are removed when the hook runs.
const STATUS_TTL: std::time::Duration = std::time::Duration::from_hours(72);

/// Whether a hook command is petsitter's, wherever its executable lives
/// (quoted when its path has spaces).
fn is_petsitter_hook(command: &str) -> bool {
    command
        .split_once(HOOK_SUBCOMMAND)
        .is_some_and(|(exe, _)| exe.trim_matches('\'').ends_with("petsitter"))
}

/// Directory holding petsitter's own state (`$XDG_STATE_HOME/petsitter`).
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .unwrap_or_default()
        .join("petsitter")
}

pub fn codex_status_dir() -> PathBuf {
    state_dir().join("codex")
}

/// Handles one Codex hook invocation: reads the event from stdin and records
/// the resulting session state.
pub fn run_codex_hook() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let event: Value = serde_json::from_str(&input).wrap_err("hook input is not JSON")?;

    let Some(session_id) = event.get("session_id").and_then(Value::as_str) else {
        return Ok(());
    };
    // Session ids become file names; refuse anything that could escape the directory.
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Ok(());
    }

    let name = event
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (state, detail) = match name {
        "PermissionRequest" => {
            let tool = event
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("approval");
            let input = event.get("tool_input").cloned().unwrap_or(Value::Null);
            let summary = summarize_tool_input(&input, std::path::Path::new(""));
            let detail = if summary.is_empty() {
                tool.to_owned()
            } else {
                format!("{tool} {summary}")
            };
            ("waiting", Some(detail))
        }
        "Stop" | "SessionStart" => ("idle", None),
        _ => ("working", None),
    };

    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let dir = codex_status_dir();
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{session_id}.tmp"));
    fs::write(
        &tmp,
        json!({ "state": state, "detail": detail, "at": at }).to_string(),
    )?;
    fs::rename(tmp, dir.join(format!("{session_id}.json")))?;
    prune_status_files(&dir);
    Ok(())
}

/// Removes the status files of sessions not heard from for days.
fn prune_status_files(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > STATUS_TTL);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Adds petsitter's hook to `$CODEX_HOME/hooks.json`, keeping existing hooks.
pub fn install_codex() -> Result<PathBuf> {
    let path = crate::collectors::agent_home("CODEX_HOME", &[".codex"]).join("hooks.json");
    let mut config: Value = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .wrap_err_with(|| format!("{} is not valid JSON", path.display()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).wrap_err_with(|| format!("cannot read {}", path.display())),
    };

    let exe = std::env::current_exe().wrap_err("cannot locate the petsitter executable")?;
    let command = format!("{} hook codex", shell_quote(&exe.to_string_lossy()));
    add_codex_hooks(&mut config, &command)?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        fs::copy(&path, path.with_extension("json.petsitter-backup"))
            .wrap_err("cannot back up hooks.json")?;
    }
    fs::write(&path, serde_json::to_string_pretty(&config)? + "\n")?;
    Ok(path)
}

/// Removes petsitter's hook from `$CODEX_HOME/hooks.json`.
pub fn uninstall_codex() -> Result<PathBuf> {
    let path = crate::collectors::agent_home("CODEX_HOME", &[".codex"]).join("hooks.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(path);
    };
    let mut config: Value = serde_json::from_str(&text)
        .wrap_err_with(|| format!("{} is not valid JSON", path.display()))?;
    if remove_codex_hooks(&mut config) {
        fs::write(&path, serde_json::to_string_pretty(&config)? + "\n")?;
    }
    Ok(path)
}

/// Drops hook groups that run petsitter; returns whether any was removed.
fn remove_codex_hooks(config: &mut Value) -> bool {
    let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut removed = false;
    for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
        let before = groups.len();
        groups.retain(|group| {
            !group["hooks"].as_array().is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|h| h["command"].as_str().is_some_and(is_petsitter_hook))
            })
        });
        removed |= groups.len() != before;
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    removed
}

fn add_codex_hooks(config: &mut Value, command: &str) -> Result<()> {
    let Some(root) = config.as_object_mut() else {
        bail!("hooks.json must contain an object")
    };
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("\"hooks\" must be an object")
    };

    for event in CODEX_EVENTS {
        let groups = hooks.entry(*event).or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else {
            bail!("\"hooks.{event}\" must be an array")
        };
        let installed = groups.iter().any(|group| {
            group["hooks"].as_array().is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|h| h["command"].as_str().is_some_and(is_petsitter_hook))
            })
        });
        if !installed {
            groups.push(json!({
                "hooks": [{ "type": "command", "command": command, "timeout": 5 }]
            }));
        }
    }
    Ok(())
}

/// Quotes a value for a POSIX shell when it contains special characters.
pub(crate) fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_hooks_once_and_keeps_existing_ones() {
        let mut config = json!({
            "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": "other" }] }] }
        });
        add_codex_hooks(&mut config, "/bin/petsitter hook codex").unwrap();
        add_codex_hooks(&mut config, "/bin/petsitter hook codex").unwrap();

        let start = config["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2);
        assert_eq!(start[0]["hooks"][0]["command"], "other");
        for event in CODEX_EVENTS {
            assert_eq!(
                config["hooks"][event]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|g| is_petsitter_hook(g["hooks"][0]["command"].as_str().unwrap()))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn uninstalls_only_petsitter_hooks() {
        let mut config = json!({
            "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": "other" }] }] }
        });
        let original = config.clone();
        add_codex_hooks(&mut config, "/bin/petsitter hook codex").unwrap();
        assert!(remove_codex_hooks(&mut config));
        assert_eq!(config, original);
        assert!(!remove_codex_hooks(&mut config));
    }

    #[test]
    fn recognizes_its_hook_at_any_path() {
        assert!(is_petsitter_hook("/bin/petsitter hook codex"));
        assert!(is_petsitter_hook("'/Apps/My Tools/petsitter' hook codex"));
        assert!(!is_petsitter_hook("other hook codex"));
        // Reinstalling from a path with spaces does not add a second hook.
        let mut config = json!({});
        add_codex_hooks(&mut config, "'/Apps/My Tools/petsitter' hook codex").unwrap();
        add_codex_hooks(&mut config, "'/Apps/My Tools/petsitter' hook codex").unwrap();
        assert_eq!(config["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(remove_codex_hooks(&mut config));
    }

    #[test]
    fn quotes_paths_with_spaces() {
        assert_eq!(shell_quote("/usr/bin/petsitter"), "/usr/bin/petsitter");
        assert_eq!(
            shell_quote("/Apps/My Tools/petsitter"),
            "'/Apps/My Tools/petsitter'"
        );
    }
}
