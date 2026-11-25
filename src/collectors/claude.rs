//! Claude Code sessions.
//!
//! Claude Code keeps one state file per running process in
//! `$CLAUDE_CONFIG_DIR/sessions/<pid>.json` (status `busy`, `idle` or
//! `waiting`), and appends every session to a JSONL transcript in
//! `$CLAUDE_CONFIG_DIR/projects/<sanitized cwd>/<session id>.jsonl`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;

use super::{
    Collector, Context, agent_home, first_line, from_millis, modified, parse_timestamp,
    pid_matches, summarize_tool_input,
};
use crate::model::{
    Activity, Agent, BackgroundTask, Limit, Session, SubAgent, ToolCall, Usage, push_recent,
    short_id,
};
use crate::tail::{self, JsonlTail};

/// Contents of `sessions/<pid>.json`. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveFile {
    pid: u32,
    session_id: String,
    cwd: PathBuf,
    name: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    waiting_for: Option<String>,
    started_at: Option<u64>,
    status_updated_at: Option<u64>,
    #[serde(default)]
    spare: bool,
    /// The job id of a session running in the background.
    job_id: Option<String>,
    /// On a terminal session that was moved to the background: the job it
    /// became. The terminal stays open and shows that job.
    parked_job_id: Option<String>,
}

#[derive(Debug)]
pub struct ClaudeCollector {
    home: PathBuf,
    /// Global config file, where Claude Code caches the account's usage.
    config_file: PathBuf,
    /// Rate limits recorded by the status line relay, when installed.
    relay_file: PathBuf,
    transcripts: HashMap<PathBuf, Transcript>,
    usage: Option<(SystemTime, Option<Usage>)>,
}

impl ClaudeCollector {
    pub fn from_env() -> Self {
        // `.claude.json` sits next to `.claude/` unless a config dir is set.
        let config_file = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map_or_else(
                || agent_home("HOME", &[]).join(".claude.json"),
                |dir| PathBuf::from(dir).join(".claude.json"),
            );
        Self::new(
            agent_home("CLAUDE_CONFIG_DIR", &[".claude"]),
            config_file,
            crate::statusline::usage_file(),
        )
    }

    pub fn new(home: PathBuf, config_file: PathBuf, relay_file: PathBuf) -> Self {
        Self {
            home,
            config_file,
            relay_file,
            transcripts: HashMap::new(),
            usage: None,
        }
    }

    fn live_files(&self) -> Vec<LiveFile> {
        let Ok(entries) = fs::read_dir(self.home.join("sessions")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|p| serde_json::from_slice::<LiveFile>(&fs::read(p).ok()?).ok())
            .filter(|f| !f.spare)
            .collect()
    }

    fn transcript_path(&self, cwd: &Path, session_id: &str) -> Option<PathBuf> {
        let file = format!("{session_id}.jsonl");
        let projects = self.home.join("projects");
        let direct = projects.join(sanitize(cwd)).join(&file);
        if direct.is_file() {
            return Some(direct);
        }
        fs::read_dir(&projects)
            .ok()?
            .flatten()
            .map(|e| e.path().join(&file))
            .find(|p| p.is_file())
    }

    /// Sub-agents of a session that have not finished yet.
    ///
    /// Claude Code keeps them next to the session transcript, in
    /// `<session id>/subagents/agent-<id>.jsonl`, each with a `.meta.json`
    /// holding its description and a `finished` flag.
    fn running_subagents(&mut self, transcript: &Path, now: SystemTime) -> Vec<SubAgent> {
        /// A sub-agent silent for this long is assumed gone (crash, kill).
        const STALE: Duration = Duration::from_mins(15);
        let dir = transcript.with_extension("").join("subagents");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let metas: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".meta.json"))
            .collect();

        let mut agents = Vec::new();
        for meta_path in metas {
            let Some(meta) = fs::read(&meta_path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            else {
                continue;
            };
            if meta["finished"].as_bool() == Some(true) {
                continue;
            }
            let log = PathBuf::from(meta_path.to_string_lossy().replace(".meta.json", ".jsonl"));
            let Some(mtime) = modified(&log) else {
                continue;
            };
            if now.duration_since(mtime).is_ok_and(|age| age > STALE) {
                continue;
            }
            let summary = self.subagent_transcript(&log);
            agents.push(SubAgent {
                description: meta["description"]
                    .as_str()
                    .unwrap_or("sub-agent")
                    .to_owned(),
                kind: meta["agentType"].as_str().map(str::to_owned),
                current_tool: summary.pending.last().map(|(_, call)| call.clone()),
                activity: summary.activity.clone(),
                tokens_used: (summary.tokens_used > 0 && !summary.tokens_pending)
                    .then_some(summary.tokens_used),
                model: summary.model.clone(),
                effort: summary.effort.clone(),
                last_active: summary.last_timestamp.unwrap_or(mtime).max(mtime),
            });
        }
        agents.sort_by_key(|a| std::cmp::Reverse(a.last_active));
        agents
    }

    fn subagent_transcript(&mut self, path: &Path) -> &TranscriptSummary {
        let transcript = self
            .transcripts
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                let mut t = Transcript::new(path);
                t.summary.sidechain = true;
                t
            });
        transcript.refresh();
        &transcript.summary
    }

    fn transcript(&mut self, path: &Path) -> &TranscriptSummary {
        let transcript = self
            .transcripts
            .entry(path.to_path_buf())
            .or_insert_with(|| Transcript::new(path));
        transcript.refresh();
        &transcript.summary
    }

    fn live_session(
        &mut self,
        ctx: &Context<'_>,
        file: LiveFile,
        viewers: &HashMap<String, u32>,
    ) -> Option<Session> {
        if !ctx.in_scope(&file.cwd) {
            return None;
        }
        let proc = ctx.procs.get(file.pid)?;
        let started = file.started_at.map(from_millis);
        if started.is_some_and(|s| !pid_matches(proc.start_time, s))
            || !proc.cmd.iter().any(|a| a.contains("claude"))
        {
            return None;
        }

        let status_time = file.status_updated_at.map_or(ctx.now, from_millis);
        let mut session = Session::new(
            Agent::Claude,
            &file.session_id,
            file.cwd.clone(),
            status_time,
        );
        session.pid = Some(file.pid);
        session.started_at = started;
        session.background = file.kind.as_deref() == Some("bg");
        let job_tasks = self.job_tasks(&file.session_id);
        // A background job's process holds the pane it was started from, not
        // the one showing it: that is its viewer's, if one is open.
        session.rex = match &file.job_id {
            Some(job) => viewers
                .get(job)
                .and_then(|&viewer| ctx.procs.rex_target(viewer)),
            None => ctx.procs.rex_target(file.pid),
        };
        session.activity = match file.status.as_deref() {
            Some("busy") => Activity::Working,
            Some("waiting") => Activity::Waiting {
                reason: file
                    .waiting_for
                    .clone()
                    .unwrap_or_else(|| "input".to_owned()),
            },
            // `shell`: the turn is over and background shells run.
            Some("shell") => {
                session.background_work = true;
                Activity::Working
            }
            _ => Activity::Idle,
        };

        if let Some(path) = self.transcript_path(&file.cwd, &file.session_id) {
            let summary = self.transcript(&path);
            if summary.continued_in.is_some() {
                return None;
            }
            summary.apply_to(&mut session);
            session.subagents = self.running_subagents(&path, ctx.now);
            if let Some(latest) = session.subagents.iter().map(|a| a.last_active).max() {
                session.last_active = session.last_active.max(latest);
            }
            session.transcript = Some(path);
        }
        session.tasks = background_tasks(ctx, file.pid, job_tasks, &session);
        // A finished turn with background work going is not the user's turn
        // yet: Claude Code wakes the session when that work ends.
        if session.activity == Activity::Idle
            && (!session.tasks.is_empty() || !session.subagents.is_empty())
        {
            session.activity = Activity::Working;
            session.background_work = true;
        }
        if let Some(name) = file
            .name
            .filter(|n| n != short_id(&file.session_id) && !n.is_empty())
        {
            session.name = name;
        }
        Some(session)
    }

    /// The background shells and monitors of a session Claude Code runs as
    /// a job (`jobs/<short id>/state.json`, whose `fan` lists its background
    /// tasks); `None` for other sessions.
    fn job_tasks(&self, session_id: &str) -> Option<Vec<BackgroundTask>> {
        let path = self
            .home
            .join("jobs")
            .join(short_id(session_id))
            .join("state.json");
        let state: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        Some(running_tasks(&state))
    }

    fn exited_sessions(&mut self, ctx: &Context<'_>, live: &HashSet<String>) -> Vec<Session> {
        let prefix = sanitize(ctx.root);
        let Ok(projects) = fs::read_dir(self.home.join("projects")) else {
            return Vec::new();
        };

        let candidates: Vec<(PathBuf, SystemTime)> = projects
            .flatten()
            .filter(|dir| dir.file_name().to_string_lossy().starts_with(&prefix))
            .filter_map(|dir| fs::read_dir(dir.path()).ok())
            .flat_map(|files| files.flatten().map(|f| f.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
            .filter(|p| {
                p.file_stem()
                    .is_some_and(|s| !live.contains(&*s.to_string_lossy()))
            })
            .filter_map(|p| modified(&p).map(|m| (p, m)))
            .filter(|(_, m)| ctx.within_exited_window(*m))
            .collect();

        let mut sessions = Vec::new();
        for (path, mtime) in candidates {
            let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let summary = self.transcript(&path);
            let Some(cwd) = summary.cwd.clone().filter(|c| ctx.in_scope(c)) else {
                continue;
            };
            if !summary.has_conversation() || summary.continued_in.is_some() {
                continue;
            }
            let mut session = Session::new(Agent::Claude, id, cwd, mtime);
            session.activity = Activity::Exited;
            summary.apply_to(&mut session);
            session.transcript = Some(path.clone());
            sessions.push(session);
        }
        sessions
    }
}

impl Collector for ClaudeCollector {
    fn collect(&mut self, ctx: &Context<'_>) -> Vec<Session> {
        for transcript in self.transcripts.values_mut() {
            transcript.seen = false;
        }

        let files = self.live_files();
        // Background jobs are shown in the terminal of the session that
        // became them, when it is still open.
        let viewers: HashMap<String, u32> = files
            .iter()
            .filter(|f| ctx.procs.get(f.pid).is_some())
            .filter_map(|f| Some((f.parked_job_id.clone()?, f.pid)))
            .collect();
        let mut sessions: Vec<Session> = files
            .into_iter()
            .filter_map(|f| self.live_session(ctx, f, &viewers))
            .collect();
        // A state file whose process is gone means the session exited, so only
        // sessions confirmed running are excluded from the exited scan.
        let running: HashSet<String> = sessions.iter().map(|s| s.id.clone()).collect();
        sessions.extend(self.exited_sessions(ctx, &running));

        // Forget transcripts that were not needed during this cycle.
        self.transcripts.retain(|_, t| t.seen);
        sessions
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.home.join("sessions"), self.home.join("projects")];
        paths.extend(self.relay_file.parent().map(Path::to_path_buf));
        paths
    }

    fn usage(&mut self, ctx: &Context<'_>) -> Option<Usage> {
        if let Some(mtime) = modified(&self.config_file)
            && self.usage.as_ref().is_none_or(|(seen, _)| *seen != mtime)
        {
            let usage = fs::read(&self.config_file)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|config| parse_usage(&config));
            self.usage = Some((mtime, usage));
        }
        let cached = self.usage.as_ref().and_then(|(_, usage)| usage.clone());
        let relayed = fs::read(&self.relay_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|record| parse_relay(&record, ctx.now));
        merge_usage(cached, relayed)
    }
}

/// Reads what the status line relay recorded: the 5-hour and weekly windows.
fn parse_relay(record: &Value, now: SystemTime) -> Option<(SystemTime, Vec<Limit>)> {
    let at = from_millis(record["at"].as_u64()?);
    let limits: Vec<Limit> = [("five_hour", "5h"), ("seven_day", "week")]
        .iter()
        .filter_map(|(key, label)| {
            let window = record.pointer(&format!("/rate_limits/{key}"))?;
            let resets_at = window["resets_at"].as_u64().map(super::from_secs);
            // A window that already reset says nothing about the current one.
            if resets_at.is_some_and(|r| r <= now) {
                return None;
            }
            Some(Limit {
                label: (*label).to_owned(),
                percent: window["used_percentage"].as_f64()?,
                resets_at,
                updated_at: Some(at),
            })
        })
        .collect();
    (!limits.is_empty()).then_some((at, limits))
}

/// Combines the cached usage with fresher numbers from the relay.
///
/// The relay only knows the 5-hour and weekly windows; per-model limits and
/// the plan name still come from the cache.
fn merge_usage(cached: Option<Usage>, relayed: Option<(SystemTime, Vec<Limit>)>) -> Option<Usage> {
    let Some((at, live)) = relayed else {
        return cached;
    };
    let Some(mut usage) = cached else {
        return Some(Usage {
            agent: Agent::Claude,
            plan: None,
            limits: live,
            updated_at: Some(at),
        });
    };
    if usage.updated_at.is_some_and(|cached_at| cached_at > at) {
        return Some(usage);
    }
    let replaced: Vec<&str> = live.iter().map(|l| l.label.as_str()).collect();
    usage
        .limits
        .retain(|l| !replaced.contains(&l.label.as_str()));
    let mut limits = live;
    limits.append(&mut usage.limits);
    usage.limits = limits;
    usage.updated_at = Some(at);
    Some(usage)
}

/// Context window of a Claude model, from its id (`claude-opus-5-5`).
///
/// Every model since the 4.6 generation has a 1M-token window; earlier ones
/// (including Haiku 4.5) have 200K.
fn context_window(model: &str) -> u64 {
    const LARGE: u64 = 1_000_000;
    const SMALL: u64 = 200_000;
    let numbers: Vec<u32> = model
        .split(['-', '.'])
        .filter_map(|part| part.parse().ok())
        .take(2)
        .collect();
    match numbers.as_slice() {
        [major, ..] if *major >= 5 => LARGE,
        [4, minor, ..] if *minor >= 6 => LARGE,
        [] => LARGE,
        _ => SMALL,
    }
}

/// A short label for a tool call: the description the agent gave (Bash
/// commands carry one), or a verb and the target.
fn activity_label(tool: &str, input: Option<&Value>, detail: &str) -> String {
    if let Some(description) = input
        .and_then(|i| i.get("description"))
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        return first_line(description);
    }
    let target = Path::new(detail)
        .file_name()
        .map_or_else(|| detail.to_owned(), |n| n.to_string_lossy().into_owned());
    let verb = match tool {
        "Read" => "Reading",
        "Edit" | "MultiEdit" | "NotebookEdit" => "Editing",
        "Write" => "Writing",
        "Grep" | "Glob" => "Searching",
        "WebFetch" => "Fetching",
        "WebSearch" => "Searching the web for",
        "Bash" => "Running",
        _ => {
            return if detail.is_empty() {
                tool.to_owned()
            } else {
                format!("{tool} {detail}")
            };
        }
    };
    if target.is_empty() {
        verb.to_owned()
    } else {
        format!("{verb} {target}")
    }
}

/// Running shells and monitors among a job's background tasks: those of
/// its `fan` not yet done.
fn running_tasks(state: &Value) -> Vec<BackgroundTask> {
    state["fan"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|task| task.get("doneAt").is_none_or(Value::is_null))
        .filter_map(|task| {
            let monitor = match task["kind"].as_str()? {
                "shell" => false,
                kind if kind.contains("monitor") => true,
                _ => return None,
            };
            Some(BackgroundTask {
                monitor,
                command: task["label"].as_str().unwrap_or_default().to_owned(),
                started_at: task["startedAt"].as_u64().map(from_millis),
                cpu: None,
            })
        })
        .collect()
}

/// The shells a Claude Code process runs, from its child processes (each
/// command runs in a shell sourcing one of its shell snapshots): the
/// command, when it started, and the CPU of its processes.
fn shell_processes(ctx: &Context<'_>, pid: u32) -> Vec<BackgroundTask> {
    let mut shells: Vec<BackgroundTask> = ctx
        .procs
        .children(pid)
        .filter(|child| child.cmd.iter().any(|arg| arg.contains("shell-snapshots")))
        .map(|child| BackgroundTask {
            monitor: false,
            command: shell_command(&child.cmd.join(" ")),
            started_at: Some(std::time::UNIX_EPOCH + Duration::from_secs(child.start_time)),
            cpu: ctx.procs.tree_usage(child.pid).map(|(cpu, _)| cpu),
        })
        .collect();
    shells.sort_by_key(|s| s.started_at);
    shells
}

/// The command a Claude Code shell runs: what follows `eval '` in its
/// command line, unquoted.
fn shell_command(cmd: &str) -> String {
    cmd.split_once("eval '").map_or_else(
        || cmd.to_owned(),
        |(_, rest)| {
            let rest = rest.rsplit_once('\'').map_or(rest, |(body, _)| body);
            rest.replace("'\\''", "'")
        },
    )
}

/// The session's background commands: a job's own list, each with the CPU
/// of its shell; otherwise its shell processes, less the one running a
/// command in the foreground (the latest started).
fn background_tasks(
    ctx: &Context<'_>,
    pid: u32,
    job: Option<Vec<BackgroundTask>>,
    session: &Session,
) -> Vec<BackgroundTask> {
    let shells = shell_processes(ctx, pid);
    if let Some(mut tasks) = job {
        for task in &mut tasks {
            let label = task.command.trim_end_matches('…');
            task.cpu = shells
                .iter()
                .find(|shell| shell.command.starts_with(label) || label.starts_with(&shell.command))
                .and_then(|shell| shell.cpu);
        }
        return tasks;
    }
    let foreground = session.activity == Activity::Working
        && session
            .current_tool
            .as_ref()
            .is_some_and(|t| t.name == "Bash");
    let mut shells = shells;
    if foreground {
        shells.pop();
    }
    shells
}

/// Reads the usage Claude Code caches in its global config file.
fn parse_usage(config: &Value) -> Option<Usage> {
    let cached = config.get("cachedUsageUtilization")?;
    let account = config
        .pointer("/oauthAccount/accountUuid")
        .and_then(Value::as_str);
    // The cache belongs to whichever account fetched it; skip it after a switch.
    if cached["accountUuid"]
        .as_str()
        .is_some_and(|a| Some(a) != account)
    {
        return None;
    }
    let utilization = &cached["utilization"];
    let fetched = cached["fetchedAtMs"].as_u64().map(from_millis);
    let resets = |v: &Value| v["resets_at"].as_str().and_then(parse_timestamp);

    let mut limits = Vec::new();
    if let Some(list) = utilization["limits"].as_array() {
        for limit in list {
            let Some(percent) = limit["percent"].as_f64() else {
                continue;
            };
            let label = limit
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str)
                .map_or_else(
                    || match limit["group"].as_str().or(limit["kind"].as_str()) {
                        Some("weekly") => "week".to_owned(),
                        Some("session" | "five_hour" | "rolling") => "5h".to_owned(),
                        Some(other) => other.replace('_', " "),
                        None => "limit".to_owned(),
                    },
                    str::to_owned,
                );
            limits.push(Limit {
                label,
                percent,
                resets_at: resets(limit),
                updated_at: fetched,
            });
        }
    } else {
        // Older cache shape: one object per window.
        for (key, label) in [("five_hour", "5h"), ("seven_day", "week")] {
            let window = &utilization[key];
            if let Some(percent) = window["utilization"].as_f64() {
                limits.push(Limit {
                    label: label.to_owned(),
                    percent,
                    resets_at: resets(window),
                    updated_at: fetched,
                });
            }
        }
    }
    if limits.is_empty() {
        return None;
    }
    Some(Usage {
        agent: Agent::Claude,
        plan: config
            .pointer("/oauthAccount/planDisplayName")
            .and_then(Value::as_str)
            .map(str::to_owned),
        limits,
        updated_at: fetched,
    })
}

/// Mirrors how Claude Code names project directories: every character that is
/// not an ASCII letter or digit becomes `-`.
fn sanitize(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[derive(Debug)]
struct Transcript {
    path: PathBuf,
    tail: JsonlTail,
    summary: TranscriptSummary,
    /// Tokens counted in the part of the file the tail skipped, arriving
    /// from the background scanner; `None` once received (or not needed).
    prefix: Option<mpsc::Receiver<u64>>,
    /// Whether the transcript was used during the current collection cycle.
    seen: bool,
}

impl Transcript {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            tail: JsonlTail::new(path),
            summary: TranscriptSummary::default(),
            prefix: None,
            seen: true,
        }
    }

    fn refresh(&mut self) {
        self.seen = true;
        if !self.tail.is_open() {
            // The tail only reads the end of a long file: count the tokens
            // of the rest in the background.
            let skipped = std::fs::metadata(&self.path)
                .map_or(0, |m| m.len().saturating_sub(tail::INITIAL_WINDOW));
            if skipped > 0 {
                self.prefix = Some(scan_tokens(
                    self.path.clone(),
                    skipped,
                    self.summary.sidechain,
                ));
                self.summary.tokens_pending = true;
            }
        }
        if let Some(prefix) = &self.prefix {
            match prefix.try_recv() {
                Ok(tokens) => {
                    self.summary.tokens_used += tokens;
                    self.summary.tokens_pending = false;
                    self.prefix = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.prefix = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Ok(records) = self.tail.read_new() {
            for record in &records {
                self.summary.apply(record);
            }
        }
    }
}

/// A request to the token scanner: the file, how many bytes of it to cover,
/// whether it is a sub-agent transcript, and where to send the count.
type ScanJob = (PathBuf, u64, bool, mpsc::Sender<u64>);

/// Counts, on a background thread, the tokens of the first `bytes` of a
/// transcript (through the end of the line that crosses that point, which
/// the tail skips).
fn scan_tokens(path: PathBuf, bytes: u64, sidechain: bool) -> mpsc::Receiver<u64> {
    static SCANNER: OnceLock<Mutex<mpsc::Sender<ScanJob>>> = OnceLock::new();
    let (reply, result) = mpsc::channel();
    let scanner = SCANNER.get_or_init(|| {
        let (jobs, queue) = mpsc::channel::<ScanJob>();
        // One thread for every transcript, so the disk is read in turn.
        std::thread::spawn(move || {
            for (path, bytes, sidechain, reply) in queue {
                let _ = reply.send(count_tokens(&path, bytes, sidechain).unwrap_or(0));
            }
        });
        Mutex::new(jobs)
    });
    if let Ok(jobs) = scanner.lock() {
        let _ = jobs.send((path, bytes, sidechain, reply));
    }
    result
}

/// Sums the tokens of the assistant messages in the first `bytes` of a
/// transcript, as `TranscriptSummary` does, without parsing whole records.
fn count_tokens(path: &Path, bytes: u64, sidechain: bool) -> io::Result<u64> {
    let mut reader = io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
    let mut line = Vec::new();
    let (mut read, mut total) = (0_u64, 0_u64);
    let mut last: Option<(Vec<u8>, u64)> = None;
    while read < bytes {
        line.clear();
        let n = io::BufRead::read_until(&mut reader, b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        read += n as u64;
        if !sidechain && contains(&line, br#""isSidechain":true"#) {
            continue;
        }
        let Some(usage) = find(&line, br#""usage":{"#) else {
            continue;
        };
        let Some(Ok(usage)) = serde_json::Deserializer::from_slice(&line[usage + 8..])
            .into_iter::<Value>()
            .next()
        else {
            continue;
        };
        let used = consumed(&usage);
        let id = find(&line, br#""id":"msg_"#)
            .map(|at| line[at..].iter().take(64).copied().collect::<Vec<u8>>())
            .unwrap_or_default();
        match &mut last {
            Some((previous, counted)) if *previous == id && !id.is_empty() => {
                total = total - *counted + used;
                *counted = used;
            }
            _ => {
                total += used;
                last = Some((id, used));
            }
        }
    }
    Ok(total)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

/// Tokens a message consumed: input and cache writes, plus output. Cache
/// reads are left out: they re-read the context already counted.
fn consumed(usage: &Value) -> u64 {
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    count("input_tokens") + count("cache_creation_input_tokens") + count("output_tokens")
}

/// What a transcript tells about its session, folded record by record.
#[derive(Debug, Default)]
struct TranscriptSummary {
    /// Whether this is a sub-agent transcript, whose records are all marked
    /// as side chains and must not be skipped.
    sidechain: bool,
    /// Title set by the user (`/rename`).
    title: Option<String>,
    /// Title generated by Claude Code from the conversation.
    ai_title: Option<String>,
    last_prompt: Option<String>,
    last_prompt_at: Option<SystemTime>,
    model: Option<String>,
    effort: Option<String>,
    context_tokens: Option<u64>,
    /// Tokens processed so far (see `Session::tokens_used`).
    tokens_used: u64,
    /// Whether the start of a long file is still being counted.
    tokens_pending: bool,
    /// The message whose usage was counted last, and what it counted: a
    /// message spans several records that repeat (and grow) its usage.
    counted: Option<(String, u64)>,
    git_branch: Option<String>,
    cwd: Option<PathBuf>,
    last_timestamp: Option<SystemTime>,
    /// Tool calls without a result yet, in call order.
    pending: Vec<(String, ToolCall)>,
    recent_tools: Vec<ToolCall>,
    /// A one-line description of what the agent is doing, from its latest
    /// tool call or message (used for sub-agents).
    activity: Option<String>,
    /// Set when the conversation moved to another session id (for example
    /// after leaving plan mode); the old id is then a stale duplicate.
    continued_in: Option<String>,
}

impl TranscriptSummary {
    fn has_conversation(&self) -> bool {
        self.model.is_some() || self.last_prompt.is_some()
    }

    fn apply(&mut self, record: &Value) {
        let str_field = |key: &str| record.get(key).and_then(Value::as_str);

        if let Some(cwd) = str_field("cwd") {
            self.cwd = Some(PathBuf::from(cwd));
        }
        if let Some(branch) = str_field("gitBranch").filter(|b| !b.is_empty()) {
            self.git_branch = Some(branch.to_owned());
        }
        let timestamp = str_field("timestamp").and_then(parse_timestamp);
        if timestamp.is_some() {
            self.last_timestamp = timestamp;
        }
        if !self.sidechain && record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }

        match str_field("type") {
            Some("custom-title") => {
                self.title = str_field("customTitle").map(str::to_owned);
            }
            Some("ai-title") => {
                self.ai_title = str_field("aiTitle").map(str::to_owned);
            }
            Some("continued-in") => {
                self.continued_in = str_field("continuedInSessionId").map(str::to_owned);
            }
            Some("last-prompt") => {
                self.last_prompt = str_field("lastPrompt").map(first_line);
            }
            Some("assistant") => self.apply_assistant(record, timestamp),
            Some("user") => self.apply_user(record, timestamp),
            _ => {}
        }
    }

    fn apply_assistant(&mut self, record: &Value, timestamp: Option<SystemTime>) {
        let Some(message) = record.get("message") else {
            return;
        };
        if let Some(effort) = record.get("effort").and_then(Value::as_str) {
            self.effort = Some(effort.to_owned());
        }
        if let Some(model) = message.get("model").and_then(Value::as_str)
            && !model.starts_with('<')
        {
            self.model = Some(model.to_owned());
        }
        if let Some(usage) = message.get("usage") {
            let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            let total = count("input_tokens")
                + count("cache_read_input_tokens")
                + count("cache_creation_input_tokens");
            if total > 0 {
                self.context_tokens = Some(total);
            }
            let used = consumed(usage);
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            match &mut self.counted {
                Some((last, counted)) if *last == id && !id.is_empty() => {
                    self.tokens_used = self.tokens_used - *counted + used;
                    *counted = used;
                }
                _ => {
                    self.tokens_used += used;
                    self.counted = Some((id, used));
                }
            }
        }
        let cwd = self.cwd.clone().unwrap_or_default();
        for block in content_blocks(message) {
            // What the agent says it is doing, for a live one-line activity.
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(Value::as_str)
            {
                let line = first_line(text);
                if !line.is_empty() {
                    self.activity = Some(line);
                }
            }
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            if self.pending.iter().any(|(pending, _)| pending == id) {
                continue;
            }
            let call = ToolCall {
                name: name.to_owned(),
                detail: block
                    .get("input")
                    .map(|i| summarize_tool_input(i, &cwd))
                    .unwrap_or_default(),
                started_at: timestamp,
            };
            self.activity = Some(activity_label(name, block.get("input"), &call.detail));
            push_recent(&mut self.recent_tools, call.clone());
            self.pending.push((id.to_owned(), call));
            if self.pending.len() > 32 {
                self.pending.remove(0);
            }
        }
    }

    fn apply_user(&mut self, record: &Value, timestamp: Option<SystemTime>) {
        let Some(message) = record.get("message") else {
            return;
        };
        let is_meta = record.get("isMeta").and_then(Value::as_bool) == Some(true);
        if let Some(text) = message.get("content").and_then(Value::as_str) {
            if !is_meta {
                self.set_prompt_from_user_text(text, timestamp);
            }
            return;
        }
        for block in content_blocks(message) {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        self.pending.retain(|(pending, _)| pending != id);
                    }
                }
                Some("text") if !is_meta => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.set_prompt_from_user_text(text, timestamp);
                    }
                }
                _ => {}
            }
        }
    }

    /// Fallback for transcripts without `last-prompt` records. Text starting
    /// with `<` is injected markup (slash commands, reminders), not a prompt.
    fn set_prompt_from_user_text(&mut self, text: &str, timestamp: Option<SystemTime>) {
        let line = first_line(text);
        if !line.is_empty() && !line.starts_with('<') {
            self.last_prompt = Some(line);
            self.last_prompt_at = timestamp.or(self.last_prompt_at);
            // A new prompt means earlier unanswered tool calls were abandoned.
            self.pending.clear();
        }
    }

    fn apply_to(&self, session: &mut Session) {
        if let Some(title) = self.title.as_ref().or(self.ai_title.as_ref()) {
            session.name.clone_from(title);
        }
        if let Some(ts) = self.last_timestamp {
            session.last_active = session.last_active.max(ts);
        }
        session.last_prompt.clone_from(&self.last_prompt);
        session.last_prompt_at = self.last_prompt_at;
        session.model.clone_from(&self.model);
        session.effort.clone_from(&self.effort);
        session.context_tokens = self.context_tokens;
        session.tokens_used =
            (self.tokens_used > 0 && !self.tokens_pending).then_some(self.tokens_used);
        session.context_window = self.model.as_deref().map(context_window);
        session.git_branch.clone_from(&self.git_branch);
        session.recent_tools.clone_from(&self.recent_tools);
        session.current_tool = self.pending.last().map(|(_, call)| call.clone());
    }
}

fn content_blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/claude/transcript.jsonl");

    fn summary_of(text: &str) -> TranscriptSummary {
        let mut summary = TranscriptSummary::default();
        for line in text.lines() {
            summary.apply(&serde_json::from_str(line).unwrap());
        }
        summary
    }

    #[test]
    fn sanitizes_like_claude_code() {
        assert_eq!(
            sanitize(Path::new("/home/dev/lab/my_repo.rs")),
            "-home-dev-lab-my-repo-rs"
        );
    }

    #[test]
    fn folds_transcript_records() {
        let summary = summary_of(FIXTURE);
        assert_eq!(summary.title.as_deref(), Some("fix-parser"));
        assert_eq!(summary.ai_title.as_deref(), Some("Parser regression test"));
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some("Add a regression test for the parser")
        );
        assert_eq!(summary.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(summary.effort.as_deref(), Some("high"));
        assert_eq!(
            summary.last_prompt_at,
            parse_timestamp("2026-01-01T10:00:00.000Z"),
            "time of the user's message, not of tool results"
        );
        assert_eq!(summary.context_tokens, Some(1200 + 30_000 + 500));
        assert_eq!(summary.git_branch.as_deref(), Some("main"));
        assert_eq!(
            summary.cwd.as_deref(),
            Some(Path::new("/home/dev/lab/demo"))
        );

        let names: Vec<&str> = summary
            .recent_tools
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, ["Read", "Bash"]);
        let (_, current) = summary.pending.last().expect("bash call is still running");
        assert_eq!(current.name, "Bash");
        assert_eq!(current.detail, "cargo test --release");
        assert_eq!(summary.recent_tools[0].detail, "src/parser.rs");
    }

    #[test]
    fn knows_context_windows() {
        assert_eq!(context_window("claude-fable-5-2"), 1_000_000);
        assert_eq!(context_window("claude-opus-4-6"), 1_000_000);
        assert_eq!(context_window("claude-haiku-4-5"), 200_000);
        assert_eq!(context_window("claude-sonnet-4-5-20250929"), 200_000);
    }

    #[test]
    fn reads_cached_usage() {
        let config: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/claude/config.json")).unwrap();
        let usage = parse_usage(&config).expect("usage is cached");
        assert_eq!(usage.plan.as_deref(), Some("Max"));
        let limits: Vec<(&str, f64)> = usage
            .limits
            .iter()
            .map(|l| (l.label.as_str(), l.percent))
            .collect();
        assert_eq!(limits, [("5h", 12.0), ("week", 23.0), ("Opus", 38.0)]);
        assert!(usage.limits[1].resets_at.is_some());
        assert!(usage.updated_at.is_some());

        let mut other = config.clone();
        other["oauthAccount"]["accountUuid"] = "someone-else".into();
        assert_eq!(parse_usage(&other), None, "cache of another account");
    }

    #[test]
    fn prefers_fresher_relayed_limits() {
        let now = from_millis(1_767_261_700_000);
        let record = serde_json::json!({
            "at": 1_767_261_690_000_u64,
            "rate_limits": {
                "five_hour": { "used_percentage": 30.0, "resets_at": 1_767_272_400 },
                "seven_day": { "used_percentage": 25.0, "resets_at": 1_767_000_000 }
            }
        });
        let relayed = parse_relay(&record, now).expect("relay record");
        assert_eq!(relayed.1.len(), 1, "the weekly window already reset");

        let config: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/claude/config.json")).unwrap();
        let merged = merge_usage(parse_usage(&config), Some(relayed.clone())).unwrap();
        let limits: Vec<(&str, f64)> = merged
            .limits
            .iter()
            .map(|l| (l.label.as_str(), l.percent))
            .collect();
        assert_eq!(limits, [("5h", 30.0), ("week", 23.0), ("Opus", 38.0)]);
        assert_eq!(merged.plan.as_deref(), Some("Max"));
        assert_eq!(merged.updated_at, Some(relayed.0));

        let stale = (from_millis(1_000), relayed.1);
        let kept = merge_usage(parse_usage(&config), Some(stale)).unwrap();
        assert_eq!(kept.limits[0].percent, 12.0, "an older relay record loses");
    }

    #[test]
    fn reads_background_tasks() {
        let state = serde_json::json!({"fan": [
            {"kind": "shell", "label": "make", "startedAt": 1_000},
            {"kind": "shell", "label": "old", "doneAt": 1},
            {"kind": "monitor", "label": "tail -f log"},
            {"kind": "agent", "label": "Review"},
        ]});
        let tasks = running_tasks(&state);
        let kinds: Vec<(bool, &str)> = tasks
            .iter()
            .map(|t| (t.monitor, t.command.as_str()))
            .collect();
        assert_eq!(kinds, [(false, "make"), (true, "tail -f log")]);
        assert_eq!(running_tasks(&serde_json::json!({})), []);
    }

    #[test]
    fn extracts_a_shell_command() {
        let cmd =
            r"/bin/zsh -c source /x/shell-snapshots/s.sh && eval 'echo '\''a'\'' b' < /dev/null";
        assert_eq!(shell_command(cmd), "echo 'a' b");
    }

    #[test]
    fn counts_each_message_once() {
        let mut summary = TranscriptSummary::default();
        for line in [
            r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":5000,"output_tokens":1}}}"#,
            r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":5000,"output_tokens":40}}}"#,
            r#"{"type":"assistant","message":{"id":"m2","usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":5150,"output_tokens":20}}}"#,
        ] {
            summary.apply(&serde_json::from_str(line).unwrap());
        }
        // Cache reads re-read the same context, so they are left out.
        assert_eq!(summary.tokens_used, 10 + 100 + 40 + 5 + 20);
    }

    #[test]
    fn labels_activities() {
        let bash = serde_json::json!({"command": "cargo test", "description": "Run the tests"});
        assert_eq!(
            activity_label("Bash", Some(&bash), "cargo test"),
            "Run the tests"
        );
        assert_eq!(
            activity_label("Read", None, "src/parser.rs"),
            "Reading parser.rs"
        );
        assert_eq!(activity_label("TodoWrite", None, ""), "TodoWrite");
    }

    #[test]
    fn lists_running_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("session.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let agents = dir.path().join("session").join("subagents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("agent-a.meta.json"),
            r#"{"agentType":"general-purpose","description":"Find the colors"}"#,
        )
        .unwrap();
        std::fs::write(
            agents.join("agent-a.jsonl"),
            r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-01-01T10:00:00Z","message":{"model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t1","name":"WebFetch","input":{"url":"https://example.com"}}]}}"#
                .to_owned()
                + "\n",
        )
        .unwrap();
        std::fs::write(
            agents.join("agent-b.meta.json"),
            r#"{"description":"Done already","finished":true}"#,
        )
        .unwrap();
        std::fs::write(agents.join("agent-b.jsonl"), "").unwrap();

        let mut collector = ClaudeCollector::new(
            dir.path().to_path_buf(),
            dir.path().join("config.json"),
            dir.path().join("relay.json"),
        );
        let running = collector.running_subagents(&transcript, SystemTime::now());
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].description, "Find the colors");
        assert_eq!(running[0].kind.as_deref(), Some("general-purpose"));
        let tool = running[0].current_tool.as_ref().expect("tool in flight");
        assert_eq!(
            (tool.name.as_str(), tool.detail.as_str()),
            ("WebFetch", "https://example.com")
        );
    }

    #[test]
    fn detects_continued_sessions() {
        let summary = summary_of(
            r#"{"type":"continued-in","sessionId":"a","continuedInSessionId":"b","timestamp":"2026-01-01T10:00:00Z"}"#,
        );
        assert_eq!(summary.continued_in.as_deref(), Some("b"));
    }

    #[test]
    fn sidechain_records_are_ignored() {
        let summary = summary_of(
            r#"{"type":"assistant","isSidechain":true,"message":{"model":"claude-haiku-5-5","content":[{"type":"tool_use","id":"t1","name":"Grep","input":{"pattern":"x"}}]}}"#,
        );
        assert!(summary.model.is_none());
        assert_eq!(summary.pending.len(), 0);
    }
}
