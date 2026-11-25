//! opencode sessions.
//!
//! opencode stores sessions in a SQLite database
//! (`$XDG_DATA_HOME/opencode/opencode.db`), which petsitter opens read-only.
//! Pending permission requests only live in the opencode service, so they are
//! fetched from its local HTTP API when the service is running.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use serde_json::Value;

use super::{Collector, Context, first_line, from_millis, summarize_tool_input};
use crate::model::{Activity, Agent, Limit, Session, ToolCall, Usage, push_recent};

/// opencode subcommands that do not host an interactive session.
const SERVICE_SUBCOMMANDS: &[&str] = &[
    "serve", "service", "api", "acp", "auth", "mcp", "models", "stats", "session", "debug",
];

#[derive(Debug)]
pub struct OpenCodeCollector {
    data_dir: PathBuf,
    state_dir: PathBuf,
    db: Option<Connection>,
    http: ureq::Agent,
    /// Last usage fetched from the service, refreshed every [`USAGE_TTL`].
    usage: Option<(Instant, Option<Usage>)>,
}

/// How often the account usage is fetched from the opencode service.
const USAGE_TTL: Duration = Duration::from_secs(60);

impl OpenCodeCollector {
    pub fn from_env() -> Self {
        let xdg = |var: &str, default: &[&str]| super::agent_home(var, default).join("opencode");
        Self::new(
            xdg("XDG_DATA_HOME", &[".local", "share"]),
            xdg("XDG_STATE_HOME", &[".local", "state"]),
        )
    }

    pub fn new(data_dir: PathBuf, state_dir: PathBuf) -> Self {
        let http = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(500)))
            .http_status_as_error(false)
            // Never follow the service elsewhere.
            .max_redirects(0)
            .build()
            .new_agent();
        Self {
            data_dir,
            state_dir,
            db: None,
            http,
            usage: None,
        }
    }

    fn db(&mut self) -> Option<&Connection> {
        if self.db.is_none() {
            let path = self.data_dir.join("opencode.db");
            if !path.is_file() {
                return None;
            }
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = Connection::open_with_flags(path, flags).ok()?;
            conn.busy_timeout(Duration::from_millis(200)).ok()?;
            self.db = Some(conn);
        }
        self.db.as_ref()
    }

    fn query_sessions(&mut self, ctx: &Context<'_>) -> rusqlite::Result<Vec<Session>> {
        let since = i64::try_from(
            ctx.now
                .checked_sub(ctx.exited_window)
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis()),
        )
        .unwrap_or(0);
        let Some(db) = self.db() else {
            return Ok(Vec::new());
        };

        let mut stmt = db.prepare_cached(
            "SELECT id, directory, title, slug, model, time_created, time_updated, time_busy, time_idle
               FROM session_v2
              WHERE parent_id IS NULL AND time_archived IS NULL AND time_updated >= ?1
              ORDER BY time_updated DESC",
        )?;
        let rows = stmt.query_map(params![since], |row| {
            Ok(Row {
                id: row.get(0)?,
                directory: row.get(1)?,
                title: row.get(2)?,
                slug: row.get(3)?,
                model: row.get(4)?,
                created: row.get(5)?,
                updated: row.get(6)?,
                busy: row.get(7)?,
                idle: row.get(8)?,
            })
        })?;

        let mut sessions = Vec::new();
        for row in rows {
            let row = row?;
            let cwd = PathBuf::from(&row.directory);
            if !ctx.in_scope(&cwd) {
                continue;
            }
            let mut session = Session::new(
                Agent::OpenCode,
                &row.id,
                cwd,
                from_millis(row.updated.unsigned_abs()),
            );
            session.started_at = Some(from_millis(row.created.unsigned_abs()));
            session.model = row.model.as_deref().and_then(model_name);
            if let Some(title) = row.title.filter(|t| !t.is_empty()).or(row.slug) {
                session.name = title;
            }
            session.activity = match (row.busy, row.idle) {
                (Some(busy), idle) if idle.is_none_or(|idle| busy > idle) => Activity::Working,
                _ => Activity::Idle,
            };
            load_activity(db, &mut session)?;
            sessions.push(session);
        }
        Ok(sessions)
    }

    /// Calls the local opencode service at `path`; `None` when it is not
    /// running or does not answer.
    fn api_get(&self, path: &str) -> Option<Value> {
        let service = self.service()?;
        let mut request = self.http.get(format!("{}{path}", service.url));
        if let Some(password) = &service.password {
            let token =
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
            request = request.header("Authorization", format!("Basic {token}"));
        }
        let mut response = request.call().ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.body_mut().read_json::<Value>().ok()
    }

    /// Session ids with a pending permission request, mapped to what is asked.
    fn pending_permissions(&self) -> HashMap<String, String> {
        let Some(body) = self.api_get("/api/permission/request") else {
            return HashMap::new();
        };
        body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|req| {
                let session = req["sessionID"].as_str()?.to_owned();
                let action = req["action"].as_str().unwrap_or("permission");
                let resource = req["resources"]
                    .as_array()
                    .and_then(|r| r.first())
                    .and_then(Value::as_str)
                    .map(first_line);
                let detail = match resource {
                    Some(resource) => format!("{action} {resource}"),
                    None => action.to_owned(),
                };
                Some((session, detail))
            })
            .collect()
    }

    /// The background service address, from `$XDG_STATE_HOME/opencode/service.json`.
    fn service(&self) -> Option<Service> {
        let info: Value =
            serde_json::from_slice(&fs::read(self.state_dir.join("service.json")).ok()?).ok()?;
        let url = info["url"].as_str()?;
        // Only ever send the password to a loopback address.
        if !is_loopback(url) {
            return None;
        }
        Some(Service {
            url: url.trim_end_matches('/').to_owned(),
            password: info["password"].as_str().map(str::to_owned),
        })
    }
}

/// Whether `url` is plain HTTP to this machine: `127.0.0.1`, `localhost` or
/// `[::1]`, with an optional port and no user info.
fn is_loopback(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        // An IPv6 address: only a port may follow it.
        return bracketed
            .split_once(']')
            .is_some_and(|(ip, port)| ip == "::1" && (port.is_empty() || port.starts_with(':')));
    }
    let host = authority.split(':').next().unwrap_or_default();
    matches!(host, "127.0.0.1" | "localhost")
}

#[derive(Debug)]
struct Service {
    url: String,
    password: Option<String>,
}

#[derive(Debug)]
struct Row {
    id: String,
    directory: String,
    title: Option<String>,
    slug: Option<String>,
    model: Option<String>,
    created: i64,
    updated: i64,
    busy: Option<i64>,
    idle: Option<i64>,
}

impl Collector for OpenCodeCollector {
    fn collect(&mut self, ctx: &Context<'_>) -> Vec<Session> {
        let Ok(mut sessions) = self.query_sessions(ctx) else {
            // The schema may have changed or the file was replaced: reopen next time.
            self.db = None;
            return Vec::new();
        };

        let permissions = if sessions.iter().any(|s| s.activity == Activity::Working) {
            self.pending_permissions()
        } else {
            HashMap::new()
        };

        // Directories with an opencode terminal UI open, and its process.
        let mut terminals: HashMap<PathBuf, u32> = ctx
            .procs
            .iter()
            .filter(|p| p.program() == "opencode")
            .filter(|p| !SERVICE_SUBCOMMANDS.iter().any(|s| p.has_arg(s)))
            .filter_map(|p| Some((p.cwd.clone()?, p.pid)))
            .collect();
        let mut seen_dirs = HashSet::new();

        // Sessions are sorted newest first: the most recent session of a
        // directory is the one its terminal shows.
        for session in &mut sessions {
            if let Some(reason) = permissions.get(&session.id) {
                session.activity = Activity::Waiting {
                    reason: reason.clone(),
                };
            }
            let first_in_dir = seen_dirs.insert(session.cwd.clone());
            let terminal = if first_in_dir {
                terminals.remove(&session.cwd)
            } else {
                None
            };
            if let Some(pid) = terminal {
                session.pid = Some(pid);
                session.rex = ctx.procs.rex_target(pid);
            } else if session.activity == Activity::Idle {
                // Busy sessions keep running inside the service without a terminal.
                session.activity = Activity::Exited;
                session.current_tool = None;
            } else {
                session.background = true;
            }
        }
        sessions
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.data_dir.clone()]
    }

    fn usage(&mut self, _ctx: &Context<'_>) -> Option<Usage> {
        if self
            .usage
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > USAGE_TTL)
        {
            let usage = self
                .api_get("/api/experimental/usage/limits")
                .and_then(|body| parse_usage(&body));
            self.usage = Some((Instant::now(), usage));
        }
        self.usage.as_ref().and_then(|(_, usage)| usage.clone())
    }
}

/// Fills prompt, tokens and tool calls from the latest messages and parts.
fn load_activity(db: &Connection, session: &mut Session) -> rusqlite::Result<()> {
    let prompt: Option<(String, i64)> = db
        .prepare_cached(
            "SELECT p.data, m.time_created FROM session_part p JOIN session_message m ON m.id = p.message_id
              WHERE p.session_id = ?1 AND m.type = 'user' AND p.type = 'text'
              ORDER BY m.seq DESC, p.ordinal ASC LIMIT 1",
        )?
        .query_row(params![session.id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    session.last_prompt_at = prompt
        .as_ref()
        .map(|(_, at)| from_millis(at.unsigned_abs()));
    session.last_prompt = prompt
        .and_then(|(data, _)| serde_json::from_str::<Value>(&data).ok())
        .and_then(|v| v["text"].as_str().map(first_line))
        .filter(|p| !p.is_empty());

    let tokens: Option<String> = db
        .prepare_cached(
            "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant'
              ORDER BY seq DESC LIMIT 1",
        )?
        .query_row(params![session.id], |row| row.get(0))
        .optional()?;
    if let Some(message) = tokens.and_then(|data| serde_json::from_str::<Value>(&data).ok()) {
        let tokens = &message["tokens"];
        let input =
            tokens["input"].as_u64().unwrap_or(0) + tokens["cache"]["read"].as_u64().unwrap_or(0);
        if input > 0 {
            session.context_tokens = Some(input);
        }
        if session.model.is_none() {
            session.model = message["modelID"].as_str().map(str::to_owned);
        }
    }

    let mut stmt = db.prepare_cached(
        "SELECT data, time_created FROM session_part WHERE session_id = ?1 AND type = 'tool'
          ORDER BY time_created DESC LIMIT ?2",
    )?;
    let limit = i64::try_from(crate::model::RECENT_TOOLS).unwrap_or(6);
    let mut tools: Vec<(Value, i64)> = stmt
        .query_map(params![session.id, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get(1)?))
        })?
        .filter_map(Result::ok)
        .filter_map(|(data, time)| Some((serde_json::from_str(&data).ok()?, time)))
        .collect();
    tools.reverse();

    for (part, created) in tools {
        let name = part["tool"].as_str().unwrap_or("tool").to_owned();
        let state = &part["state"];
        let call = ToolCall {
            name,
            detail: summarize_tool_input(&state["input"], &session.cwd),
            started_at: Some(from_millis(created.unsigned_abs())),
        };
        if matches!(state["status"].as_str(), Some("pending" | "running")) {
            session.current_tool = Some(call.clone());
        }
        push_recent(&mut session.recent_tools, call);
    }
    Ok(())
}

/// Reads the limits of opencode's own subscription (`/api/experimental/usage/limits`).
///
/// The endpoint is experimental; only the shape published in its `OpenAPI`
/// description is handled, and percentages are assumed to range from 0 to 100.
fn parse_usage(body: &Value) -> Option<Usage> {
    let workspace = body["data"].as_array()?.first()?;
    let limits: Vec<Limit> = [("rolling", "5h"), ("weekly", "week"), ("monthly", "month")]
        .iter()
        .filter_map(|(key, label)| {
            let window = workspace.pointer(&format!("/go/{key}"))?;
            Some(Limit {
                label: (*label).to_owned(),
                percent: window["percent"].as_f64()?,
                resets_at: match &window["resetsAt"] {
                    Value::Number(n) => n.as_u64().map(|v| {
                        // Accept both seconds and milliseconds since the epoch.
                        if v > 100_000_000_000 {
                            from_millis(v)
                        } else {
                            super::from_secs(v)
                        }
                    }),
                    Value::String(s) => super::parse_timestamp(s),
                    _ => None,
                },
                updated_at: Some(SystemTime::now()),
            })
        })
        .collect();
    (!limits.is_empty()).then(|| Usage {
        agent: Agent::OpenCode,
        plan: workspace
            .pointer("/workspace/name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        limits,
        updated_at: Some(SystemTime::now()),
    })
}

/// Extracts a model name from the `model` column, which holds either a plain
/// identifier or a JSON object such as `{"providerID": "...", "modelID": "..."}`.
fn model_name(raw: &str) -> Option<String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => map
            .get("modelID")
            .or_else(|| map.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        Ok(Value::String(s)) => Some(s),
        _ => Some(raw.rsplit('/').next().unwrap_or(raw).to_owned()).filter(|s| !s.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::is_loopback;

    #[test]
    fn sends_the_password_only_to_this_machine() {
        for url in [
            "http://127.0.0.1:4096",
            "http://localhost:4096/",
            "http://[::1]:4096",
        ] {
            assert!(is_loopback(url), "{url}");
        }
        for url in [
            "https://127.0.0.1:4096",
            "http://localhost:80@example.com/",
            "http://127.0.0.1.example.com",
            "http://example.com/127.0.0.1",
            "http://[::1].example.com",
        ] {
            assert!(!is_loopback(url), "{url}");
        }
    }

    use super::*;
    use std::path::Path;

    #[test]
    fn parses_usage_limits() {
        let body = serde_json::json!({"data": [{
            "workspace": {"id": "w", "name": "personal"},
            "go": {
                "rolling": {"percent": 20.0, "limited": false, "resetsAt": 1_767_272_400_000_u64},
                "weekly": {"percent": 55.5, "limited": false, "resetsAt": "2026-01-05T00:00:00Z"},
                "monthly": null
            }
        }]});
        let usage = parse_usage(&body).expect("limits");
        assert_eq!(usage.plan.as_deref(), Some("personal"));
        assert_eq!(usage.limits.len(), 2);
        assert!(usage.limits.iter().all(|l| l.resets_at.is_some()));
        assert_eq!(parse_usage(&serde_json::json!({"data": null})), None);
    }

    #[test]
    fn parses_model_column_variants() {
        assert_eq!(
            model_name(r#"{"providerID":"anthropic","modelID":"claude-sonnet-5-5"}"#).as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(
            model_name("anthropic/claude-sonnet-5-5").as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(model_name(r#""gpt-5.5""#).as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn reads_sessions_from_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("opencode.db")).unwrap();
        db.execute_batch(include_str!("../../tests/fixtures/opencode/schema.sql"))
            .unwrap();
        drop(db);

        let procs = crate::procs::ProcessTable::new();
        let now = from_millis(1_767_261_700_000);
        let ctx = Context {
            root: Path::new("/home/dev/lab"),
            procs: &procs,
            now,
            exited_window: Duration::from_hours(12),
        };
        let mut collector =
            OpenCodeCollector::new(dir.path().to_path_buf(), dir.path().join("none"));
        let sessions = collector.collect(&ctx);

        assert_eq!(
            sessions.len(),
            2,
            "child and out-of-scope sessions are skipped"
        );
        let busy = &sessions[0];
        assert_eq!(busy.name, "Refactor the lexer");
        assert_eq!(busy.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(busy.activity, Activity::Working);
        assert!(
            busy.background,
            "busy without a terminal runs in the service"
        );
        assert_eq!(busy.last_prompt.as_deref(), Some("Refactor the lexer"));
        assert_eq!(busy.context_tokens, Some(4_000 + 9_000));
        assert_eq!(
            busy.current_tool.as_ref().map(|t| t.detail.as_str()),
            Some("cargo check")
        );
        assert_eq!(busy.recent_tools.len(), 2);

        assert_eq!(sessions[1].activity, Activity::Exited);
    }
}
