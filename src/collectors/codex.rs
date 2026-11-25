//! `OpenAI` Codex CLI sessions.
//!
//! Codex appends every session to a rollout file,
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<timestamp>-<id>.jsonl`, where each
//! line is `{"timestamp", "type", "payload"}`. Running sessions are matched to
//! `codex` processes by working directory and recency.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::{
    Collector, Context, agent_home, first_line, from_secs, modified, parse_timestamp,
    summarize_tool_input, window_label,
};
use crate::model::{Activity, Agent, Limit, Session, ToolCall, Usage, push_recent};
use crate::procs::ProcInfo;
use crate::tail::JsonlTail;

/// Codex subcommands that are services rather than interactive sessions.
const SERVICE_SUBCOMMANDS: &[&str] = &[
    "app-server",
    "mcp",
    "mcp-server",
    "exec-server",
    "remote-control",
    "login",
    "doctor",
];

#[derive(Debug)]
pub struct CodexCollector {
    home: PathBuf,
    rollouts: HashMap<PathBuf, Rollout>,
    status_dir: PathBuf,
}

impl CodexCollector {
    pub fn from_env() -> Self {
        Self::new(
            agent_home("CODEX_HOME", &[".codex"]),
            crate::hooks::codex_status_dir(),
        )
    }

    pub fn new(home: PathBuf, status_dir: PathBuf) -> Self {
        Self {
            home,
            rollouts: HashMap::new(),
            status_dir,
        }
    }

    /// Rollout files modified within the exited window.
    ///
    /// Only the day directories that can contain such files are listed, so the
    /// cost does not grow with the size of the history.
    fn recent_rollouts(&self, ctx: &Context<'_>) -> Vec<(PathBuf, SystemTime)> {
        let sessions = self.home.join("sessions");
        let tz = jiff::tz::TimeZone::system();
        let now = jiff::Timestamp::try_from(ctx.now).unwrap_or_else(|_| jiff::Timestamp::now());
        let window_days = i64::try_from(ctx.exited_window.as_secs() / 86_400).unwrap_or(0) + 2;

        let mut files = Vec::new();
        for days_ago in 0..=window_days {
            let Ok(date) = now
                .to_zoned_datetime(tz.clone())
                .date()
                .checked_sub(jiff::Span::new().days(days_ago))
            else {
                continue;
            };
            let dir = sessions
                .join(format!("{:04}", date.year()))
                .join(format!("{:02}", date.month()))
                .join(format!("{:02}", date.day()));
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            files.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension().is_some_and(|ext| ext == "jsonl")
                            && p.file_name()
                                .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
                    })
                    .filter_map(|p| modified(&p).map(|m| (p, m)))
                    .filter(|(_, m)| ctx.within_exited_window(*m)),
            );
        }
        files
    }

    fn thread_names(&self) -> HashMap<String, String> {
        let Ok(text) = fs::read_to_string(self.home.join("session_index.jsonl")) else {
            return HashMap::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|v| {
                let id = v.get("id")?.as_str()?.to_owned();
                let name = v.get("thread_name")?.as_str()?.to_owned();
                Some((id, name))
            })
            .collect()
    }

    /// Activity reported by the optional petsitter hook, if it is fresh.
    fn hook_status(&self, session_id: &str, last_active: SystemTime) -> Option<HookStatus> {
        let path = self.status_dir.join(format!("{session_id}.json"));
        let status: HookStatus = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        (super::from_millis(status.at) >= last_active).then_some(status)
    }
}

#[derive(Debug, serde::Deserialize)]
struct HookStatus {
    state: String,
    #[serde(default)]
    detail: Option<String>,
    /// Milliseconds since the Unix epoch.
    at: u64,
}

impl Collector for CodexCollector {
    fn collect(&mut self, ctx: &Context<'_>) -> Vec<Session> {
        let files = self.recent_rollouts(ctx);
        let seen: HashSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();
        self.rollouts
            .retain(|path, _| seen.contains(path.as_path()));
        for (path, mtime) in &files {
            let rollout = self
                .rollouts
                .entry(path.clone())
                .or_insert_with(|| Rollout::new(path));
            rollout.refresh(*mtime);
        }

        let processes = codex_processes(ctx);
        let assignments = assign_processes(&processes, &self.rollouts);
        let names = self.thread_names();

        let mut sessions = Vec::new();
        for (path, rollout) in &self.rollouts {
            let summary = &rollout.summary;
            let (Some(id), Some(cwd)) = (&summary.id, &summary.cwd) else {
                continue;
            };
            if !ctx.in_scope(cwd) || !summary.has_conversation() {
                continue;
            }
            let mut session = Session::new(Agent::Codex, id, cwd.clone(), rollout.mtime);
            summary.apply_to(&mut session);
            if let Some(name) = names.get(id) {
                session.name.clone_from(name);
            }

            if let Some(proc) = assignments.get(path) {
                session.pid = Some(proc.pid);
                session.rex = ctx.procs.rex_target(proc.pid);
                session.started_at =
                    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(proc.start_time));
                if let Some(hook) = self.hook_status(id, session.last_active) {
                    session.activity = match hook.state.as_str() {
                        "waiting" => Activity::Waiting {
                            reason: hook.detail.unwrap_or_else(|| "approval".to_owned()),
                        },
                        "working" => Activity::Working,
                        _ => Activity::Idle,
                    };
                }
            } else {
                session.activity = Activity::Exited;
                session.current_tool = None;
            }
            sessions.push(session);
        }
        sessions
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.home.join("sessions"), self.status_dir.clone()]
    }

    fn usage(&mut self, _ctx: &Context<'_>) -> Option<Usage> {
        // Every rollout reports the account's limits; the newest report wins,
        // whatever directory its session ran in.
        let (at, limits) = self
            .rollouts
            .values()
            .filter_map(|r| r.summary.rate_limits.as_ref())
            .max_by_key(|(at, _)| *at)?;
        parse_rate_limits(limits, *at)
    }
}

/// Interactive `codex` processes, excluding wrappers whose child is also codex.
fn codex_processes<'a>(ctx: &'a Context<'_>) -> Vec<&'a ProcInfo> {
    let is_codex = |p: &ProcInfo| {
        p.program() == "codex"
            || p.cmd
                .get(1)
                .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "codex"))
    };
    let candidates: Vec<&ProcInfo> = ctx
        .procs
        .iter()
        .filter(|p| is_codex(p))
        .filter(|p| !SERVICE_SUBCOMMANDS.iter().any(|s| p.has_arg(s)))
        .collect();
    let parents: HashSet<u32> = candidates.iter().filter_map(|p| p.parent).collect();
    candidates
        .into_iter()
        .filter(|p| !parents.contains(&p.pid))
        .collect()
}

/// Matches each live process with the rollout it is most likely writing.
///
/// A process launched with `resume <id>` is matched by id. Otherwise the most
/// recently written rollout in the same directory that started after the
/// process wins; each rollout is assigned to at most one process.
fn assign_processes<'a>(
    processes: &[&'a ProcInfo],
    rollouts: &HashMap<PathBuf, Rollout>,
) -> HashMap<PathBuf, &'a ProcInfo> {
    let mut assigned: HashMap<PathBuf, &ProcInfo> = HashMap::new();
    let mut by_recency: Vec<(&PathBuf, &Rollout)> = rollouts.iter().collect();
    by_recency.sort_by_key(|(_, r)| std::cmp::Reverse(r.mtime));

    // Newest processes first, so that an older process does not steal the
    // rollout of a session started later in the same directory.
    let mut processes = processes.to_vec();
    processes.sort_by_key(|p| std::cmp::Reverse(p.start_time));

    for proc in processes {
        let resumed = proc
            .cmd
            .iter()
            .position(|a| a == "resume" || a == "fork")
            .and_then(|i| proc.cmd.get(i + 1));
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(proc.start_time);
        let found = by_recency.iter().find(|(path, rollout)| {
            if assigned.contains_key(*path) {
                return false;
            }
            if let (Some(want), Some(id)) = (resumed, &rollout.summary.id)
                && want == id
            {
                return true;
            }
            rollout.summary.cwd.as_ref() == proc.cwd.as_ref()
                && rollout
                    .summary
                    .started_at
                    .is_none_or(|s| s + Duration::from_secs(5) >= start)
        });
        if let Some((path, _)) = found {
            assigned.insert((*path).clone(), proc);
        }
    }
    assigned
}

#[derive(Debug)]
struct Rollout {
    tail: JsonlTail,
    summary: RolloutSummary,
    mtime: SystemTime,
}

impl Rollout {
    fn new(path: &Path) -> Self {
        Self {
            tail: JsonlTail::new(path),
            summary: RolloutSummary::default(),
            mtime: SystemTime::UNIX_EPOCH,
        }
    }

    fn refresh(&mut self, mtime: SystemTime) {
        if mtime == self.mtime {
            return;
        }
        self.mtime = mtime;
        if let Ok(records) = self.tail.read_new() {
            for record in &records {
                self.summary.apply(record);
            }
        }
    }
}

/// What a rollout tells about its session, folded record by record.
#[derive(Debug, Default)]
struct RolloutSummary {
    id: Option<String>,
    cwd: Option<PathBuf>,
    started_at: Option<SystemTime>,
    model: Option<String>,
    effort: Option<String>,
    git_branch: Option<String>,
    last_prompt: Option<String>,
    last_prompt_at: Option<SystemTime>,
    context_tokens: Option<u64>,
    /// Tokens processed so far: uncached input plus output.
    tokens_used: Option<u64>,
    context_window: Option<u64>,
    last_timestamp: Option<SystemTime>,
    /// Latest account rate limits reported by the server, with their time.
    rate_limits: Option<(SystemTime, Value)>,
    /// Whether a turn is in progress (between `task_started` and its end).
    in_turn: bool,
    /// Set when the agent asked for approval and has not resumed since.
    approval: Option<String>,
    pending: Vec<(String, ToolCall)>,
    recent_tools: Vec<ToolCall>,
}

impl RolloutSummary {
    fn has_conversation(&self) -> bool {
        self.last_prompt.is_some() || self.model.is_some()
    }

    fn apply(&mut self, record: &Value) {
        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        if timestamp.is_some() {
            self.last_timestamp = timestamp;
        }
        let Some(payload) = record.get("payload") else {
            return;
        };
        let field = |key: &str| payload.get(key).and_then(Value::as_str);

        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                self.id = field("id").map(str::to_owned);
                self.cwd = field("cwd").map(PathBuf::from);
                self.started_at = field("timestamp").and_then(parse_timestamp).or(timestamp);
                self.git_branch = payload
                    .pointer("/git/branch")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("turn_context") => {
                if let Some(cwd) = field("cwd") {
                    self.cwd = Some(PathBuf::from(cwd));
                }
                if let Some(model) = field("model") {
                    self.model = Some(model.to_owned());
                }
                if let Some(effort) = field("effort") {
                    self.effort = Some(effort.to_owned());
                }
            }
            Some("event_msg") => self.apply_event(payload, timestamp),
            Some("response_item") => self.apply_item(payload, timestamp),
            _ => {}
        }
    }

    fn apply_event(&mut self, payload: &Value, timestamp: Option<SystemTime>) {
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "user_message" => {
                if let Some(text) = payload.get("message").and_then(Value::as_str) {
                    self.set_prompt(text, timestamp);
                }
                self.in_turn = true;
                self.pending.clear();
            }
            // Recent Codex versions report the prompt as a completed item.
            "item_completed"
                if payload.pointer("/item/type").and_then(Value::as_str) == Some("UserMessage") =>
            {
                let text: Vec<&str> = payload
                    .pointer("/item/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.get("text").and_then(Value::as_str))
                    .collect();
                self.set_prompt(&text.join("\n"), timestamp);
            }
            "thread_settings_applied" => {
                let settings = &payload["thread_settings"];
                if let Some(effort) = settings["reasoning_effort"].as_str() {
                    self.effort = Some(effort.to_owned());
                }
                if let Some(model) = settings["model"].as_str() {
                    self.model = Some(model.to_owned());
                }
            }
            "task_started" | "turn_started" => {
                self.in_turn = true;
                if let Some(window) = payload["model_context_window"].as_u64() {
                    self.context_window = Some(window);
                }
            }
            "task_complete" | "turn_complete" | "turn_aborted" | "shutdown_complete" => {
                self.in_turn = false;
                self.approval = None;
                self.pending.clear();
            }
            "token_count" => {
                if let Some(total) = payload.pointer("/info/total_token_usage") {
                    let count = |key: &str| total.get(key).and_then(Value::as_u64).unwrap_or(0);
                    self.tokens_used = Some(
                        count("input_tokens").saturating_sub(count("cached_input_tokens"))
                            + count("output_tokens"),
                    );
                }
                let usage = payload.pointer("/info/last_token_usage");
                if let Some(total) = usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(Value::as_u64)
                {
                    self.context_tokens = Some(total);
                }
                if let Some(window) = payload
                    .pointer("/info/model_context_window")
                    .and_then(Value::as_u64)
                {
                    self.context_window = Some(window);
                }
                if let (Some(limits), Some(at)) = (payload.get("rate_limits"), timestamp)
                    && !limits.is_null()
                {
                    self.rate_limits = Some((at, limits.clone()));
                }
            }
            "exec_command_begin" | "patch_apply_begin" => self.approval = None,
            _ if kind.ends_with("approval_request") => {
                let detail = payload
                    .get("command")
                    .map(|c| {
                        summarize_tool_input(&serde_json::json!({ "command": c }), Path::new(""))
                    })
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "approval".to_owned());
                self.approval = Some(detail);
            }
            _ => {}
        }
    }

    fn apply_item(&mut self, payload: &Value, timestamp: Option<SystemTime>) {
        let field = |key: &str| payload.get(key).and_then(Value::as_str);
        match field("type") {
            Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                let name = field("name").unwrap_or("shell").to_owned();
                let input = payload
                    .get("arguments")
                    .or_else(|| payload.get("input"))
                    .or_else(|| payload.get("action"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let cwd = self.cwd.clone().unwrap_or_default();
                let detail = input
                    .as_str()
                    .and_then(|raw| script_detail(raw, &cwd))
                    .unwrap_or_else(|| summarize_tool_input(&input, &cwd));
                let call = ToolCall {
                    name,
                    detail,
                    started_at: timestamp,
                };
                push_recent(&mut self.recent_tools, call.clone());
                if let Some(id) = field("call_id").or_else(|| field("id")) {
                    self.pending.push((id.to_owned(), call));
                    if self.pending.len() > 32 {
                        self.pending.remove(0);
                    }
                }
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                if let Some(id) = field("call_id") {
                    self.pending.retain(|(pending, _)| pending != id);
                }
                self.approval = None;
            }
            Some("message") if field("role") == Some("assistant") => self.approval = None,
            _ => {}
        }
    }

    fn set_prompt(&mut self, text: &str, timestamp: Option<SystemTime>) {
        let line = first_line(text);
        if !line.is_empty() && !line.starts_with('<') {
            self.last_prompt = Some(line);
            self.last_prompt_at = timestamp.or(self.last_prompt_at);
        }
    }

    fn apply_to(&self, session: &mut Session) {
        if let Some(ts) = self.last_timestamp {
            session.last_active = session.last_active.max(ts);
        }
        session.started_at = self.started_at;
        session.model.clone_from(&self.model);
        session.effort.clone_from(&self.effort);
        session.git_branch.clone_from(&self.git_branch);
        session.last_prompt.clone_from(&self.last_prompt);
        session.last_prompt_at = self.last_prompt_at;
        session.context_tokens = self.context_tokens;
        session.tokens_used = self.tokens_used;
        session.context_window = self.context_window;
        session.recent_tools.clone_from(&self.recent_tools);
        session.current_tool = self.pending.last().map(|(_, call)| call.clone());
        session.activity = if let Some(reason) = &self.approval {
            Activity::Waiting {
                reason: reason.clone(),
            }
        } else if self.in_turn {
            Activity::Working
        } else {
            Activity::Idle
        };
    }
}

/// Converts a Codex `rate_limits` object into a [`Usage`].
fn parse_rate_limits(limits: &Value, at: SystemTime) -> Option<Usage> {
    let windows: Vec<Limit> = ["primary", "secondary"]
        .iter()
        .filter_map(|key| {
            let window = limits.get(*key)?;
            Some(Limit {
                label: window["window_minutes"]
                    .as_u64()
                    .map_or_else(|| (*key).to_owned(), window_label),
                percent: window["used_percent"].as_f64()?,
                resets_at: window["resets_at"].as_u64().map(from_secs),
                updated_at: Some(at),
            })
        })
        .collect();
    if windows.is_empty() {
        return None;
    }
    Some(Usage {
        agent: Agent::Codex,
        plan: limits["plan_type"].as_str().map(str::to_owned),
        limits: windows,
        updated_at: Some(at),
    })
}

/// Describes free-form tool inputs: Codex runs commands through a script
/// (`tools.exec_command({cmd: "..."})`) and edits files with patches.
fn script_detail(raw: &str, cwd: &Path) -> Option<String> {
    const PATCH_HEADERS: &[&str] = &["*** Update File: ", "*** Add File: ", "*** Delete File: "];
    if let Some(path) = raw.lines().find_map(|line| {
        PATCH_HEADERS
            .iter()
            .find_map(|h| line.trim().strip_prefix(h))
    }) {
        let path = Path::new(path.trim());
        return Some(path.strip_prefix(cwd).unwrap_or(path).display().to_string());
    }
    let start = raw.find("cmd:")? + "cmd:".len();
    let literal = raw[start..].trim_start();
    let command = serde_json::Deserializer::from_str(literal)
        .into_iter::<String>()
        .next()?
        .ok()?;
    Some(first_line(&command))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/codex/rollout.jsonl");

    fn summary_of(text: &str) -> RolloutSummary {
        let mut summary = RolloutSummary::default();
        for line in text.lines() {
            summary.apply(&serde_json::from_str(line).unwrap());
        }
        summary
    }

    #[test]
    fn folds_rollout_records() {
        let summary = summary_of(FIXTURE);
        assert_eq!(
            summary.id.as_deref(),
            Some("00000000-0000-4000-8000-0000000000c0")
        );
        assert_eq!(
            summary.cwd.as_deref(),
            Some(Path::new("/home/dev/lab/demo"))
        );
        assert_eq!(summary.model.as_deref(), Some("gpt-5.5-codex"));
        assert_eq!(summary.git_branch.as_deref(), Some("main"));
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some("List the examples directory")
        );
        assert_eq!(summary.context_tokens, Some(12_000));
        assert!(
            summary.in_turn,
            "a new turn started after the first one completed"
        );
        let (_, current) = summary.pending.last().expect("exec call is running");
        assert_eq!(current.name, "exec");
        assert_eq!(current.detail, "cargo build");
        let details: Vec<&str> = summary
            .recent_tools
            .iter()
            .map(|t| t.detail.as_str())
            .collect();
        assert_eq!(details, ["cat build.rs", "cargo build", "cargo build"]);
    }

    #[test]
    fn reads_rate_limits_and_context_window() {
        let summary = summary_of(FIXTURE);
        assert_eq!(summary.context_window, Some(258_400));
        let (at, limits) = summary.rate_limits.clone().expect("limits reported");
        let usage = parse_rate_limits(&limits, at).expect("limits parse");
        assert_eq!(usage.plan.as_deref(), Some("plus"));
        let windows: Vec<(&str, f64)> = usage
            .limits
            .iter()
            .map(|l| (l.label.as_str(), l.percent))
            .collect();
        assert_eq!(windows, [("5h", 41.0), ("week", 12.5)]);
        assert!(usage.limits[0].resets_at.is_some());
    }

    #[test]
    fn describes_script_and_patch_inputs() {
        let cwd = Path::new("/work");
        assert_eq!(
            script_detail(
                r#"const r = await tools.exec_command({cmd:"ls -la && pwd",yield_time_ms:10000});"#,
                cwd
            )
            .as_deref(),
            Some("ls -la && pwd")
        );
        assert_eq!(
            script_detail(
                "*** Begin Patch\n*** Update File: /work/src/lib.rs\n@@",
                cwd
            )
            .as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(script_detail("print('hi')", cwd), None);
    }

    #[test]
    fn turn_completion_clears_state() {
        let mut summary = summary_of(FIXTURE);
        summary.apply(&serde_json::json!({
            "timestamp": "2026-01-01T10:01:00Z",
            "type": "event_msg",
            "payload": {"type": "task_complete"}
        }));
        let mut session = Session::new(Agent::Codex, "x", PathBuf::new(), SystemTime::UNIX_EPOCH);
        summary.apply_to(&mut session);
        assert_eq!(session.activity, Activity::Idle);
        assert!(session.current_tool.is_none());
    }
}
