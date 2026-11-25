//! Core data types shared by collectors and the UI.

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// The coding agent that owns a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
}

impl Agent {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    /// Product name, for headings.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::OpenCode => "opencode",
        }
    }

    /// Arguments that resume the given session (without the program name).
    pub fn resume_args(self, session_id: &str) -> String {
        // Ids come from agents' files: quoted unless they are plain.
        let id = crate::hooks::shell_quote(session_id);
        match self {
            Self::Claude => format!("--resume {id}"),
            Self::Codex => format!("resume {id}"),
            Self::OpenCode => format!("--session {id}"),
        }
    }

    /// Shell command that resumes the given session.
    pub fn resume_command(self, session_id: &str) -> String {
        format!("{} {}", self.label(), self.resume_args(session_id))
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What a session is doing right now, as reported by its agent.
///
/// The UI derives the displayed section (active, inactive, exited) from this
/// state combined with how long ago the session last did something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// The agent is generating or running a tool.
    Working,
    /// The agent is blocked on the user (permission prompt, question...).
    Waiting { reason: String },
    /// The agent finished its turn and is waiting for the next prompt.
    Idle,
    /// The process is gone; the session can be resumed.
    Exited,
}

/// A tool invocation seen in a session transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    /// A short, human readable summary of the input (command, path, pattern...).
    pub detail: String,
    pub started_at: Option<SystemTime>,
}

/// Everything petsitter knows about one agent session.
#[derive(Debug, Clone)]
pub struct Session {
    pub agent: Agent,
    pub id: String,
    /// Display name: custom title, session name, or a shortened id.
    pub name: String,
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub activity: Activity,
    /// Last time the session showed any sign of life.
    pub last_active: SystemTime,
    pub started_at: Option<SystemTime>,
    pub model: Option<String>,
    /// Reasoning effort the model runs at (`low`, `medium`, `high`...).
    pub effort: Option<String>,
    /// Tokens currently in the context window, when the agent reports it.
    pub context_tokens: Option<u64>,
    /// Tokens processed over the whole conversation: input (cache writes
    /// included, cache reads excluded) plus output.
    pub tokens_used: Option<u64>,
    /// Size of the model's context window, when known.
    pub context_window: Option<u64>,
    pub git_branch: Option<String>,
    pub last_prompt: Option<String>,
    /// When the user last prompted the session.
    pub last_prompt_at: Option<SystemTime>,
    /// The tool call currently in flight, if any.
    pub current_tool: Option<ToolCall>,
    /// Most recent tool calls, newest last.
    pub recent_tools: Vec<ToolCall>,
    /// Whether the session runs in the background (no terminal attached).
    pub background: bool,
    /// Background commands it keeps running: shells, and monitors (which
    /// watch a command's output for it).
    pub tasks: Vec<BackgroundTask>,
    /// Its turn is over but it still has background work going (shells,
    /// monitors, sub-agents), which will wake it: it counts as working.
    pub background_work: bool,
    /// Rex terminal pane hosting the session, when known.
    pub rex: Option<RexTarget>,
    /// The transcript file, for agents that keep one per session.
    pub transcript: Option<PathBuf>,
    /// Sub-agents currently running on behalf of the session.
    pub subagents: Vec<SubAgent>,
    /// Set when the session runs in a linked git worktree.
    pub worktree: Option<Worktree>,
    /// CPU use of the agent and all its child processes, in percent of one core.
    pub cpu: Option<f32>,
    /// Resident memory of the agent and all its child processes, in bytes.
    pub memory: Option<u64>,
}

/// A linked git worktree (`git worktree add`) and the repository it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Working directory of the main repository.
    pub main: PathBuf,
    /// Branch checked out in the worktree (a short hash when detached).
    pub branch: Option<String>,
}

/// A command a session keeps running in the background.
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundTask {
    /// A monitor (it watches the command's output), else a shell.
    pub monitor: bool,
    pub command: String,
    pub started_at: Option<SystemTime>,
    /// CPU of the command's processes, in percent of one core.
    pub cpu: Option<f32>,
}

/// A sub-agent working for a session (Claude Code's Task/Agent tool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubAgent {
    /// What it was asked to do.
    pub description: String,
    /// The kind of agent, e.g. `general-purpose`.
    pub kind: Option<String>,
    pub current_tool: Option<ToolCall>,
    /// What it is doing now, in a few words (its latest step).
    pub activity: Option<String>,
    /// Tokens it processed so far, counted as for sessions.
    pub tokens_used: Option<u64>,
    /// The model it runs on and how hard it thinks, from its transcript.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub last_active: SystemTime,
}

impl Session {
    /// Background shells running.
    pub fn shells(&self) -> usize {
        self.tasks.iter().filter(|t| !t.monitor).count()
    }

    /// Background monitors running.
    pub fn monitors(&self) -> usize {
        self.tasks.iter().filter(|t| t.monitor).count()
    }

    /// A session skeleton with every optional field left empty.
    pub fn new(agent: Agent, id: impl Into<String>, cwd: PathBuf, last_active: SystemTime) -> Self {
        let id = id.into();
        Self {
            agent,
            name: short_id(&id).to_owned(),
            id,
            cwd,
            pid: None,
            activity: Activity::Idle,
            last_active,
            started_at: None,
            model: None,
            effort: None,
            context_tokens: None,
            tokens_used: None,
            context_window: None,
            git_branch: None,
            last_prompt: None,
            last_prompt_at: None,
            current_tool: None,
            recent_tools: Vec::new(),
            background: false,
            tasks: Vec::new(),
            background_work: false,
            rex: None,
            transcript: None,
            subagents: Vec::new(),
            worktree: None,
            cpu: None,
            memory: None,
        }
    }

    pub fn resume_command(&self) -> String {
        self.agent.resume_command(&self.id)
    }

    /// Share of the context window in use, from 0 to 100.
    #[allow(clippy::cast_precision_loss)] // token counts are far below 2^52
    pub fn context_percent(&self) -> Option<f64> {
        let (used, window) = (self.context_tokens?, self.context_window?);
        (window > 0).then(|| (used as f64 / window as f64 * 100.0).min(100.0))
    }
}

/// Address of a Rex pane (a "block" in Rex terms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RexTarget {
    pub session: Option<String>,
    pub block: String,
}

/// Quota usage of the account an agent is signed in with.
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub agent: Agent,
    /// Subscription plan, e.g. "Max" or "plus".
    pub plan: Option<String>,
    pub limits: Vec<Limit>,
    /// When the agent last fetched these numbers.
    pub updated_at: Option<SystemTime>,
}

/// One rate-limit window.
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    /// Short label: "5h", "week", or a model name for model-specific limits.
    pub label: String,
    /// Share of the window already used, from 0 to 100.
    pub percent: f64,
    pub resets_at: Option<SystemTime>,
    /// When this number was last reported; sources refresh at different paces.
    pub updated_at: Option<SystemTime>,
}

/// Returns the first segment of a UUID-like identifier.
pub fn short_id(id: &str) -> &str {
    let end = id
        .char_indices()
        .find(|&(i, c)| c == '-' && i >= 6)
        .or_else(|| id.char_indices().nth(12))
        .map_or(id.len(), |(i, _)| i);
    &id[..end]
}

/// Whether an id from an agent's files is safe to pass to that agent's
/// command line: letters, digits, `-` and `_`, not starting with `-`.
pub fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Number of recent tool calls kept per session.
pub const RECENT_TOOLS: usize = 6;

/// Pushes a tool call, keeping only the most recent [`RECENT_TOOLS`].
pub fn push_recent(tools: &mut Vec<ToolCall>, call: ToolCall) {
    tools.push(call);
    if tools.len() > RECENT_TOOLS {
        tools.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_takes_first_uuid_segment() {
        assert_eq!(short_id("0f8c1d2e-4a5b-4c6d-8e9f-0a1b2c3d4e5f"), "0f8c1d2e");
        assert_eq!(short_id("ses_0123456789abcdef"), "ses_01234567");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(
            short_id("ééééééééééééé"),
            "éééééééééééé",
            "cut between characters"
        );
        assert!(is_plain_id("ses_01-ab") && !is_plain_id("--help") && !is_plain_id("a b"));
    }

    #[test]
    fn push_recent_is_bounded() {
        let mut tools = Vec::new();
        for i in 0..10 {
            push_recent(
                &mut tools,
                ToolCall {
                    name: format!("t{i}"),
                    detail: String::new(),
                    started_at: None,
                },
            );
        }
        assert_eq!(tools.len(), RECENT_TOOLS);
        assert_eq!(tools.last().map(|t| t.name.as_str()), Some("t9"));
    }
}
