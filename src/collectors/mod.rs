//! Agent-specific session discovery.
//!
//! Each collector turns an agent's on-disk state (and, for opencode, its
//! local HTTP API) into [`Session`] values. Collectors are read-only: they
//! never modify agent files.

pub mod claude;
pub mod codex;
pub mod opencode;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::model::{Session, Usage};
use crate::procs::ProcessTable;

/// Shared inputs of one collection cycle.
#[derive(Debug)]
pub struct Context<'a> {
    /// Only sessions whose working directory is `root` or inside it are kept.
    pub root: &'a Path,
    pub procs: &'a ProcessTable,
    pub now: SystemTime,
    /// How long exited sessions stay listed.
    pub exited_window: Duration,
}

impl Context<'_> {
    /// Whether `cwd` is the monitored root or lives below it.
    pub fn in_scope(&self, cwd: &Path) -> bool {
        cwd.starts_with(self.root)
    }

    /// Whether something last touched at `time` is recent enough to be listed as exited.
    pub fn within_exited_window(&self, time: SystemTime) -> bool {
        self.now
            .duration_since(time)
            .map_or(true, |age| age <= self.exited_window)
    }
}

/// A source of agent sessions.
pub trait Collector: Send {
    /// Discovers the sessions of this agent, live and recently exited.
    fn collect(&mut self, ctx: &Context<'_>) -> Vec<Session>;

    /// Directories whose changes should trigger a refresh.
    fn watch_paths(&self) -> Vec<PathBuf>;

    /// Quota usage of the signed-in account, when the agent records it.
    fn usage(&mut self, _ctx: &Context<'_>) -> Option<Usage> {
        None
    }
}

/// Builds every supported collector.
pub fn all() -> Vec<Box<dyn Collector>> {
    vec![
        Box::new(claude::ClaudeCollector::from_env()),
        Box::new(codex::CodexCollector::from_env()),
        Box::new(opencode::OpenCodeCollector::from_env()),
    ]
}

/// Resolves an agent home directory from an environment variable, falling back
/// to a path under the user's home directory.
pub(crate) fn agent_home(env_var: &str, default: &[&str]) -> PathBuf {
    if let Some(dir) = std::env::var_os(env_var).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let mut path = dirs::home_dir().unwrap_or_default();
    path.extend(default);
    path
}

/// Parses an RFC 3339 timestamp.
pub(crate) fn parse_timestamp(value: &str) -> Option<SystemTime> {
    value
        .parse::<jiff::Timestamp>()
        .ok()
        .and_then(|t| SystemTime::try_from(t).ok())
}

/// Short label for a rate-limit window given in minutes: `5h`, `week`, `30d`.
pub(crate) fn window_label(minutes: u64) -> String {
    match minutes {
        10_080 => "week".to_owned(),
        m if m >= 1440 && m % 1440 == 0 => format!("{}d", m / 1440),
        m if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// Converts seconds since the Unix epoch.
pub(crate) fn from_secs(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(secs))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Converts milliseconds since the Unix epoch.
pub(crate) fn from_millis(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_millis(ms))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

pub(crate) fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether a process that started at `start_time` (Unix seconds) can be the one
/// that opened a session at `session_start`.
///
/// PIDs are recycled, so a stale state file may point at an unrelated process
/// that started after the session did.
pub(crate) fn pid_matches(start_time: u64, session_start: SystemTime) -> bool {
    const SLACK: Duration = Duration::from_secs(60);
    let proc_start = SystemTime::UNIX_EPOCH + Duration::from_secs(start_time);
    proc_start <= session_start + SLACK
}

/// Summarizes a tool call input into a single short line.
///
/// Agents name their tools differently, so this looks for the argument that
/// best describes the call (a command, a path, a pattern...).
pub(crate) fn summarize_tool_input(input: &Value, cwd: &Path) -> String {
    const KEYS: &[&str] = &[
        "command",
        "cmd",
        "file_path",
        "filePath",
        "path",
        "notebook_path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
        "skill",
    ];

    let input = match input {
        // Some agents send tool arguments as a JSON-encoded string.
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed @ Value::Object(_)) => parsed,
            _ => return first_line(raw),
        },
        other => other.clone(),
    };
    let Some(map) = input.as_object() else {
        return String::new();
    };

    let value = KEYS
        .iter()
        .find_map(|key| map.get(*key))
        .or_else(|| map.values().find(|v| v.is_string() || v.is_array()));
    let text = match value {
        Some(Value::String(s)) => s.clone(),
        // Commands are sometimes argv arrays, e.g. ["bash", "-lc", "cargo test"].
        Some(Value::Array(items)) => {
            let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [shell, flag, script] if flag.starts_with('-') && is_shell(shell) => {
                    (*script).to_owned()
                }
                _ => parts.join(" "),
            }
        }
        _ => return String::new(),
    };

    let text = first_line(&text);
    match Path::new(&text).strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
        _ => text,
    }
}

fn is_shell(program: &str) -> bool {
    matches!(
        Path::new(program).file_name().and_then(|n| n.to_str()),
        Some("bash" | "sh" | "zsh" | "fish")
    )
}

/// First non-empty line of `text`, trimmed.
pub(crate) fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarizes_common_tool_inputs() {
        let cwd = Path::new("/work/repo");
        assert_eq!(
            summarize_tool_input(
                &json!({"command": "cargo test\n--release", "timeout": 5}),
                cwd
            ),
            "cargo test"
        );
        assert_eq!(
            summarize_tool_input(&json!({"file_path": "/work/repo/src/main.rs"}), cwd),
            "src/main.rs"
        );
        assert_eq!(
            summarize_tool_input(&json!({"command": ["bash", "-lc", "ls -la"]}), cwd),
            "ls -la"
        );
        assert_eq!(
            summarize_tool_input(&json!(r#"{"cmd":"git status"}"#), cwd),
            "git status"
        );
        assert_eq!(summarize_tool_input(&json!({"n": 3}), cwd), "");
    }

    #[test]
    fn labels_limit_windows() {
        assert_eq!(window_label(300), "5h");
        assert_eq!(window_label(10_080), "week");
        assert_eq!(window_label(43_200), "30d");
        assert_eq!(window_label(45), "45m");
    }

    #[test]
    fn scope_is_root_and_below() {
        let procs = ProcessTable::new();
        let ctx = Context {
            root: Path::new("/lab"),
            procs: &procs,
            now: SystemTime::now(),
            exited_window: Duration::from_secs(60),
        };
        assert!(ctx.in_scope(Path::new("/lab/a/b")));
        assert!(ctx.in_scope(Path::new("/lab")));
        assert!(!ctx.in_scope(Path::new("/laboratory/a")));
    }
}
