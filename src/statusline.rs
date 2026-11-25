//! Claude Code status line relay.
//!
//! Claude Code sends its status line command a JSON document after every
//! assistant message; for subscribers it includes the account's rate limits
//! (`rate_limits.five_hour` and `rate_limits.seven_day`). Those numbers are
//! fresher than anything else Claude Code stores on disk.
//!
//! `petsitter hooks install claude` wraps the user's status line command:
//! Claude Code runs `petsitter statusline --then '<original command>'`, which
//! records the rate limits and pipes the same JSON to the original command, so
//! the status line looks exactly as before.

use std::fs;
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::hooks::{shell_quote, state_dir};

/// Subcommand Claude Code runs once the relay is installed.
const SUBCOMMAND: &str = "statusline";
/// Separates the relay from the wrapped command in `statusLine.command`.
const THEN: &str = "--then";

/// Where the relay records the latest rate limits.
pub fn usage_file() -> PathBuf {
    state_dir().join("claude-usage.json")
}

/// Runs the relay: records the rate limits, then hands the input to `then`.
/// Returns the exit code of the wrapped command.
pub fn run(then: Option<&str>) -> Result<i32> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;

    // Recording is best effort: the status line must render regardless.
    if let Ok(status) = serde_json::from_slice::<Value>(&input) {
        let _ = record(&status, &usage_file());
    }

    let Some(command) = then else {
        return Ok(0);
    };
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .spawn()
        .wrap_err("cannot start the wrapped status line command")?;
    if let Some(mut stdin) = child.stdin.take() {
        // The wrapped command may exit without reading everything.
        let _ = stdin.write_all(&input);
    }
    Ok(child.wait()?.code().unwrap_or(1))
}

/// Saves the `rate_limits` object of a status line document, if present.
fn record(status: &Value, path: &Path) -> io::Result<()> {
    let Some(limits) = status.get("rate_limits").filter(|l| l.is_object()) else {
        return Ok(());
    };
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let record = json!({ "at": at, "rate_limits": limits });

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, record.to_string())?;
    fs::rename(tmp, path)
}

/// `$CLAUDE_CONFIG_DIR/settings.json`.
fn settings_path() -> PathBuf {
    crate::collectors::agent_home("CLAUDE_CONFIG_DIR", &[".claude"]).join("settings.json")
}

/// Wraps the configured status line with the relay. Returns the settings path.
pub fn install() -> Result<PathBuf> {
    let path = settings_path();
    let mut settings = read_settings(&path)?;
    let exe = std::env::current_exe().wrap_err("cannot locate the petsitter executable")?;
    let relay = format!("{} {SUBCOMMAND}", shell_quote(&exe.to_string_lossy()));
    if !wrap(&mut settings, &relay)? {
        return Ok(path);
    }
    write_settings(&path, &settings)?;
    Ok(path)
}

/// Restores the status line command the relay wrapped.
pub fn uninstall() -> Result<PathBuf> {
    let path = settings_path();
    let mut settings = read_settings(&path)?;
    if unwrap(&mut settings)? {
        write_settings(&path, &settings)?;
    }
    Ok(path)
}

fn read_settings(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .wrap_err_with(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).wrap_err_with(|| format!("cannot read {}", path.display())),
    }
}

fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    if path.exists() {
        fs::copy(path, path.with_extension("json.petsitter-backup"))
            .wrap_err("cannot back up the settings file")?;
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string_pretty(settings)? + "\n")?;
    Ok(())
}

/// Whether a status line command is the relay.
fn is_relay(command: &str) -> bool {
    command
        .split_once(&format!(" {SUBCOMMAND}"))
        .is_some_and(|(exe, _)| exe.trim_matches('\'').ends_with("petsitter"))
}

/// Points `statusLine` at the relay, keeping the previous command as `--then`.
/// Returns whether anything changed.
fn wrap(settings: &mut Value, relay: &str) -> Result<bool> {
    let Some(root) = settings.as_object_mut() else {
        bail!("settings must contain a JSON object");
    };
    let line = root
        .entry("statusLine")
        .or_insert_with(|| json!({ "type": "command" }));
    let Some(line) = line.as_object_mut() else {
        bail!("\"statusLine\" must be an object");
    };
    let current = line
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if is_relay(current) {
        return Ok(false);
    }
    let command = if current.trim().is_empty() {
        relay.to_owned()
    } else {
        format!("{relay} {THEN} {}", shell_quote(current))
    };
    line.insert("type".into(), "command".into());
    line.insert("command".into(), command.into());
    Ok(true)
}

/// Undoes [`wrap`]. Returns whether anything changed.
fn unwrap(settings: &mut Value) -> Result<bool> {
    let Some(line) = settings
        .get_mut("statusLine")
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };
    let current = line
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !is_relay(&current) {
        return Ok(false);
    }
    let marker = format!(" {THEN} ");
    match current.split_once(&marker) {
        Some((_, quoted)) => {
            let Some(original) = shell_unquote(quoted) else {
                bail!("cannot read the wrapped command back from {current:?}");
            };
            line.insert("command".into(), original.into());
        }
        None => {
            if let Some(root) = settings.as_object_mut() {
                root.remove("statusLine");
            }
        }
    }
    Ok(true)
}

/// Reverses [`shell_quote`]: a bare word, or a single-quoted string where
/// `'\''` stands for a quote.
fn shell_unquote(text: &str) -> Option<String> {
    let text = text.trim();
    let Some(inner) = text.strip_prefix('\'') else {
        return (!text.contains(char::is_whitespace)).then(|| text.to_owned());
    };
    let inner = inner.strip_suffix('\'')?;
    Some(inner.replace(r"'\''", "'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAY: &str = "/usr/local/bin/petsitter statusline";

    #[test]
    fn wraps_and_restores_the_status_line() {
        let original = json!({
            "model": "opus",
            "statusLine": { "type": "command", "command": "bash ~/it's/statusline.sh", "padding": 0 }
        });
        let mut settings = original.clone();
        assert!(wrap(&mut settings, RELAY).unwrap());
        let command = settings["statusLine"]["command"].as_str().unwrap();
        assert_eq!(
            command,
            r"/usr/local/bin/petsitter statusline --then 'bash ~/it'\''s/statusline.sh'"
        );
        assert_eq!(
            settings["statusLine"]["padding"], 0,
            "other fields are kept"
        );
        assert!(
            !wrap(&mut settings, RELAY).unwrap(),
            "installing twice is a no-op"
        );

        assert!(unwrap(&mut settings).unwrap());
        assert_eq!(settings, original);
        assert!(!unwrap(&mut settings).unwrap());
    }

    #[test]
    fn installs_without_a_previous_status_line() {
        let mut settings = json!({ "model": "opus" });
        assert!(wrap(&mut settings, RELAY).unwrap());
        assert_eq!(settings["statusLine"]["command"], RELAY);
        assert!(unwrap(&mut settings).unwrap());
        assert_eq!(settings, json!({ "model": "opus" }));
    }

    #[test]
    fn records_rate_limits_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        record(&json!({ "model": {} }), &path).unwrap();
        assert!(!path.exists());

        let limits =
            json!({ "five_hour": { "used_percentage": 12.5, "resets_at": 1_767_272_400 } });
        record(&json!({ "rate_limits": limits }), &path).unwrap();
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["rate_limits"], limits);
        assert!(saved["at"].as_u64().is_some());
    }
}
