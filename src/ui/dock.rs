//! The details panel docked at the bottom of both layouts: for a session,
//! what it is doing, the commands it ran, its background shells and its
//! sub-agents; for a git folder, the branches its sessions work on, and the
//! files and diff of a commit opened from there.

use std::time::SystemTime;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr as _;

use super::{State, agent_glyph, format, justify};
use crate::app::App;
use crate::git::Overview;
use crate::model::{Activity, BackgroundTask, Session, SubAgent};
use crate::view::{self, Section};

/// Earlier commands listed under what the session does now.
const DOCK_TOOLS: usize = 3;
/// Indentation of the panel's body.
const INDENT: &str = "    ";

/// The details docked under the list for the selected session: the last
/// prompt, what it does now and the commands before, then its background
/// work as Claude Code lists it (shells, monitors, agents), and facts about
/// it.
///
/// ```text
/// ── ✻ fix-parser  feat/parser ────────────── claude · opus-5-5 (high) ──
///     › add a regression test for the parser                        2h ago
///     ● Bash  cargo test --release                                     12s
///       10:41  Edit  src/parser.rs
///
///     Background
///     1 active shell · 1 active agent
///
///     Shells (1)
///     ● cargo watch -x check                       running · CPU:3% · 1h12
///
///     Agents (1)
///     ● Explore - Reading lexer.rs                   running · 48k · 2m
///
///     context 43% of 1M  ·  2.3M tokens  ·  up 3h  ·  CPU:9% · MEM:598M
///     ~/code/api-server  ·  claude --resume 0f8c1d2e-…
/// ```
pub(super) fn dock_lines(app: &App, session: &Session, width: usize) -> Vec<Line<'static>> {
    let t = app.theme;
    let quiet = view::section(session, app.now, app.idle_after) == Section::Inactive;
    let (state, color) = match &session.activity {
        _ if quiet => (State::Faded, t.muted()),
        Activity::Working => (State::Working, t.working()),
        Activity::Waiting { .. } => (State::Blocked, t.waiting()),
        Activity::Idle => (State::Ready, t.ready()),
        Activity::Exited => (State::Faded, t.muted()),
    };
    let mut lines = vec![Line::default(), dock_title(app, session, width)];

    if let Some(prompt) = &session.last_prompt {
        let age = session
            .last_prompt_at
            .map(|at| format!("{} ago ", format::since(app.now, at)))
            .unwrap_or_default();
        lines.push(justify(
            vec![
                Span::raw(INDENT),
                Span::styled("› ", t.muted()),
                Span::styled(prompt.clone(), t.faint_italic()),
            ],
            vec![Span::styled(age, t.muted())],
            width,
        ));
    }

    // Now: the running command, or the state; then the commands before.
    let current = session
        .current_tool
        .as_ref()
        .filter(|_| session.activity == Activity::Working && !quiet);
    let mut left = vec![
        Span::raw(INDENT),
        Span::styled("● ", t.fg_bold(t.state_color(state))),
    ];
    left.extend(now_spans(app, session, quiet, color));
    let running_for = current
        .and_then(|tool| tool.started_at)
        .map(|at| format!("{} ", format::since(app.now, at)))
        .unwrap_or_default();
    lines.push(justify(
        left,
        vec![Span::styled(running_for, t.muted())],
        width,
    ));
    for tool in session
        .recent_tools
        .iter()
        .rev()
        .filter(|tool| Some(*tool) != current)
        .take(DOCK_TOOLS)
    {
        let at = tool.started_at.map(clock).unwrap_or_default();
        lines.push(justify(
            vec![
                Span::raw(format!("{INDENT}  ")),
                Span::styled(format!("{at:<7}"), t.muted()),
                Span::styled(format!("{}  ", tool.name), t.tool()),
                Span::styled(tool.detail.clone(), t.muted()),
            ],
            Vec::new(),
            width,
        ));
    }

    if session.activity != Activity::Exited {
        lines.extend(background_lines(app, session, width));
    }
    lines.extend(dock_facts(app, session, width));
    lines
}

/// What the session does now: its running command, or its state.
fn now_spans(app: &App, session: &Session, quiet: bool, color: Style) -> Vec<Span<'static>> {
    let t = app.theme;
    let current = session
        .current_tool
        .as_ref()
        .filter(|_| session.activity == Activity::Working && !quiet);
    match (&session.activity, current) {
        _ if quiet => vec![Span::styled(
            format!("quiet for {}", format::since(app.now, session.last_active)),
            t.muted(),
        )],
        (Activity::Working, Some(tool)) => vec![
            Span::styled(format!("{}  ", tool.name), t.tool()),
            Span::raw(tool.detail.clone()),
        ],
        (Activity::Working, None) if session.background_work => vec![Span::styled(
            "turn over, waiting for its background work",
            color,
        )],
        (Activity::Working, None) => vec![Span::styled("thinking", color)],
        (Activity::Waiting { reason }, _) => {
            vec![Span::styled(format!("blocked  {reason}"), color)]
        }
        (Activity::Idle, _) => vec![Span::styled("your turn", color)],
        (Activity::Exited, _) => vec![Span::styled(
            format!("exited {} ago", format::since(app.now, session.last_active)),
            color,
        )],
    }
}

/// The session's background work, as Claude Code's background view lists
/// it: a summary, then its shells, monitors and agents, one per line.
fn background_lines(app: &App, session: &Session, width: usize) -> Vec<Line<'static>> {
    let t = app.theme;
    let shells: Vec<&BackgroundTask> = session.tasks.iter().filter(|t| !t.monitor).collect();
    let monitors: Vec<&BackgroundTask> = session.tasks.iter().filter(|t| t.monitor).collect();
    let agents = &session.subagents;
    if shells.is_empty() && monitors.is_empty() && agents.is_empty() {
        return Vec::new();
    }
    let count = |n: usize, what: &str| match n {
        0 => None,
        1 => Some(format!("1 active {what}")),
        n => Some(format!("{n} active {what}s")),
    };
    let summary: Vec<String> = [
        count(shells.len(), "shell"),
        count(monitors.len(), "monitor"),
        count(agents.len(), "agent"),
    ]
    .into_iter()
    .flatten()
    .collect();

    let heading = |title: &str, n: usize| {
        vec![
            Line::default(),
            Line::from(vec![
                Span::raw(INDENT),
                Span::styled(title.to_owned(), t.bold()),
                Span::styled(format!(" ({n})"), t.muted()),
            ]),
        ]
    };
    // `lead` goes between the indentation and the bullet.
    let item = |lead: Vec<Span<'static>>, text: Vec<Span<'static>>, right: Vec<String>| {
        let mut left = vec![Span::raw(INDENT)];
        left.extend(lead);
        left.push(Span::styled("● ", t.muted()));
        left.extend(text);
        justify(
            left,
            vec![Span::styled(format!("{} ", right.join(" · ")), t.muted())],
            width,
        )
    };
    let task_facts = |task: &BackgroundTask| {
        let mut right = vec!["running".to_owned()];
        if let Some(cpu) = task.cpu {
            right.push(format!("CPU:{cpu:.0}%"));
        }
        if let Some(started) = task.started_at {
            right.push(format::since(app.now, started));
        }
        right
    };

    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::raw(INDENT),
            Span::styled("Background", t.branch().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::raw(INDENT),
            Span::styled(summary.join(" · "), t.muted()),
        ]),
    ];
    for (title, tasks) in [("Shells", &shells), ("Monitors", &monitors)] {
        if tasks.is_empty() {
            continue;
        }
        lines.extend(heading(title, tasks.len()));
        for task in tasks {
            lines.push(item(
                Vec::new(),
                vec![Span::raw(task.command.clone())],
                task_facts(task),
            ));
        }
    }
    if !agents.is_empty() {
        lines.extend(heading("Agents", agents.len()));
        for agent in agents {
            let (lead, text, right) = agent_item(app, session, agent);
            lines.push(item(lead, text, right));
        }
    }
    lines
}

/// A sub-agent's line in the background view, in parts: the lead that
/// places it under the sub-agent that started it (`└ ● fork`), its type,
/// model and latest step, and the facts flush right.
fn agent_item(
    app: &App,
    session: &Session,
    agent: &SubAgent,
) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<String>) {
    let t = app.theme;
    let lead = match agent.level {
        0 => Vec::new(),
        level => vec![
            Span::raw("  ".repeat(level - 1)),
            Span::styled("└ ", t.muted()),
        ],
    };
    let mut text = vec![Span::styled(
        agent.kind.clone().unwrap_or_else(|| "agent".to_owned()),
        t.agent(session.agent),
    )];
    if let Some(model) = super::subagent_model(session, agent) {
        text.push(Span::styled(format!(" ({model})"), t.muted()));
    }
    text.push(Span::styled(" - ", t.muted()));
    text.push(Span::raw(
        agent
            .activity
            .as_ref()
            .unwrap_or(&agent.description)
            .clone(),
    ));
    let mut right = vec!["running".to_owned()];
    if let Some(tokens) = agent.tokens_used {
        right.push(format::tokens(tokens));
    }
    right.push(format::since(app.now, agent.last_active));
    (lead, text, right)
}

/// Facts about the session: its usage, then where it runs and how to
/// resume it.
fn dock_facts(app: &App, session: &Session, width: usize) -> Vec<Line<'static>> {
    let t = app.theme;
    let mut usage = Vec::new();
    if let Some(percent) = session.context_percent() {
        let window = session
            .context_window
            .map(format::tokens)
            .unwrap_or_default();
        usage.push(format!("context {percent:.0}% of {window}"));
    } else if let Some(tokens) = session.context_tokens {
        usage.push(format!("context {}", format::tokens(tokens)));
    }
    if let Some(tokens) = session.tokens_used {
        usage.push(format!("{} tokens", format::tokens(tokens)));
    }
    if let Some(started) = session.started_at {
        usage.push(format!("up {}", format::since(app.now, started)));
    }
    if let (Some(cpu), Some(memory)) = (session.cpu, session.memory) {
        usage.push(format!("CPU:{cpu:.0}% · MEM:{}", format::bytes(memory)));
    }
    if let Some(pid) = session.pid {
        usage.push(format!("pid {pid}"));
    }
    let place = format!(
        "{}  ·  {}",
        format::home_relative(&session.cwd),
        session.resume_command()
    );
    vec![
        Line::default(),
        justify(
            vec![
                Span::raw(INDENT),
                Span::styled(usage.join("  ·  "), t.muted()),
            ],
            Vec::new(),
            width,
        ),
        justify(
            vec![Span::raw(INDENT), Span::styled(place, t.muted())],
            Vec::new(),
            width,
        ),
    ]
}

/// Local wall-clock time of `at`, `14:05`.
fn clock(at: SystemTime) -> String {
    jiff::Timestamp::try_from(at)
        .map(|ts| {
            ts.to_zoned_datetime(jiff::tz::TimeZone::system())
                .strftime("%H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// Title rule of the dock: `── ✻ name  branch ──────── agent · model (effort) ──`.
fn dock_title(app: &App, session: &Session, width: usize) -> Line<'static> {
    let t = app.theme;
    let branch = session
        .worktree
        .as_ref()
        .and_then(|w| w.branch.clone())
        .or_else(|| session.git_branch.clone())
        .filter(|b| b != "HEAD");
    let mut title = vec![
        Span::styled("── ", t.border()),
        agent_glyph(app, session.agent),
        Span::raw(" "),
        Span::styled(session.name.clone(), t.bold()),
    ];
    if let Some(branch) = branch {
        title.push(Span::styled(format!("  {branch}"), t.branch()));
    }
    title.push(Span::raw(" "));
    let mut tail = vec![
        Span::raw(" "),
        Span::styled(session.agent.label(), t.agent(session.agent)),
    ];
    if let Some(model) = &session.model {
        let model = match &session.effort {
            Some(effort) => format!(" · {} ({effort})", format::model(session.agent, model)),
            None => format!(" · {}", format::model(session.agent, model)),
        };
        tail.push(Span::styled(model, t.muted()));
    }
    tail.push(Span::styled(" ──", t.border()));
    let used = |spans: &[Span<'_>]| spans.iter().map(|s| s.content.width()).sum::<usize>();
    let fill = width.saturating_sub(used(&title) + used(&tail));
    title.push(Span::styled("─".repeat(fill), t.border()));
    title.extend(tail);
    Line::from(title)
}

/// Changed files listed in the folder panel.
const DOCK_CHANGES: usize = 4;

/// The details of a folder that is a git repository: branch, pending
/// changes, and the branches its sessions work on.
pub(super) fn folder_lines(app: &App, path: &std::path::Path, width: usize) -> Vec<Line<'static>> {
    let t = app.theme;
    let Some(info) = app.git_info(path) else {
        return Vec::new();
    };

    // Title rule: ── api-server  main  ↑2 ↓1 ─────────── ~/code/api-server ──
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut title = vec![
        Span::styled("── ", t.border()),
        Span::styled(name, t.bold()),
    ];
    match &info.branch {
        Some(branch) => title.push(Span::styled(format!("  {branch}"), t.branch())),
        None => title.push(Span::styled("  detached", t.muted())),
    }
    if let Some((ahead, behind)) = info.ahead_behind {
        title.push(Span::styled(format!("  ↑{ahead} ↓{behind}"), t.muted()));
    }
    title.push(Span::raw(" "));
    let used: usize = title.iter().map(|s| s.content.width()).sum();
    let tail = format!(" {} ──", format::home_relative(path));
    title.push(Span::styled(
        "─".repeat(width.saturating_sub(used + tail.width())),
        t.border(),
    ));
    title.push(Span::styled(tail, t.muted()));
    let mut lines = vec![Line::default(), Line::from(title)];

    lines.extend(change_lines(app, &info, width));

    // The branches sessions work on, against the trunk.
    if let Some(overview) = app.overview(path) {
        lines.push(Line::default());
        let top = lines.len();
        lines.extend(overview_lines(app, path, &overview, width, top));
    }
    lines
}

/// The `changes` section of the folder panel.
fn change_lines(app: &App, info: &crate::git::GitInfo, width: usize) -> Vec<Line<'static>> {
    let t = app.theme;
    let mut lines = Vec::new();
    let summary = if info.changes.is_empty() {
        "clean".to_owned()
    } else {
        let untracked = info.changes.iter().filter(|(c, _)| c == "?").count();
        let changed = info.changes.len() - untracked;
        let mut parts = Vec::new();
        if changed > 0 {
            parts.push(format!("{changed} changed"));
        }
        if untracked > 0 {
            parts.push(format!("{untracked} untracked"));
        }
        parts.join(" · ")
    };
    lines.push(Line::from(vec![
        Span::styled(format!("   {:<10}", "changes"), t.muted()),
        Span::raw(summary),
    ]));
    for (code, file) in info.changes.iter().take(DOCK_CHANGES) {
        let style = match code.as_str() {
            "?" => t.muted(),
            "D" => t.error(),
            "A" => t.ready(),
            _ => t.waiting(),
        };
        lines.push(justify(
            vec![
                Span::raw("     "),
                Span::styled(format!("{code} "), style),
                Span::raw(file.clone()),
            ],
            Vec::new(),
            width,
        ));
    }
    if info.changes.len() > DOCK_CHANGES {
        lines.push(Line::styled(
            format!("     … {} more", info.changes.len() - DOCK_CHANGES),
            t.muted(),
        ));
    }

    lines
}

/// Indentation of the branch overview.
const OVERVIEW_INDENT: &str = "    ";
/// Colors of the branches, in turn; the trunk is blue.
const BRANCH_COLORS: [Color; 5] = [
    Color::Magenta,
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Red,
];

/// The trunk as a line down the left, and a block branching off it for each
/// branch a session works on: its name, sessions and folder, then its
/// latest commits, oldest first, the line ending at the newest.
///
/// ```text
/// ●  main   7d5903b  docs: the install section              2 days ago
/// │
/// ├─╮   feat/parser   fix-parser  api-server   29 ahead · 0 behind main
/// │ ┆  24 more
/// │ ●  b3c1d2e  feat: …                                  80 minutes ago
/// │ ●  1daf0d8  fix: …                                    3 minutes ago
/// ```
fn overview_lines(
    app: &App,
    path: &std::path::Path,
    overview: &Overview,
    width: usize,
    top: usize,
) -> Vec<Line<'static>> {
    let t = app.theme;
    let trunk = t.fg_bold(Color::Blue).remove_modifier(Modifier::BOLD);
    let live = app.live_branches(path);
    let commit_line = |spine: Vec<Span<'static>>, point: &crate::git::Point| {
        let mut left = vec![Span::raw(OVERVIEW_INDENT)];
        left.extend(spine);
        left.push(Span::styled(format!("{}  ", point.short), t.branch()));
        left.push(Span::raw(point.subject.clone()));
        justify(
            left,
            vec![Span::styled(
                format!("{}  {} ", point.age, point.author),
                t.muted(),
            )],
            width,
        )
    };

    // The trunk's latest commit, and the sessions on it if any.
    let mut head = vec![
        Span::styled("●  ", t.fg_bold(Color::Blue)),
        Span::styled(format!(" {} ", overview.trunk), t.pill(Color::Blue)),
    ];
    if let Some(on_trunk) = live.iter().find(|b| b.name == overview.trunk) {
        head.push(Span::styled(
            format!("  {}", on_trunk.sessions.join(", ")),
            t.bold(),
        ));
    }
    head.push(Span::raw("  "));
    let mut lines = vec![commit_line(head, &overview.tip)];
    // Lines holding a commit, to click on.
    let mut commits = vec![(0, overview.tip.hash.clone())];

    for (n, branch) in overview.branches.iter().enumerate() {
        let color = BRANCH_COLORS[n % BRANCH_COLORS.len()];
        let branch_line = t.fg_bold(color).remove_modifier(Modifier::BOLD);
        lines.push(Line::from(vec![
            Span::raw(OVERVIEW_INDENT),
            Span::styled("│", trunk),
        ]));

        // The branch leaving the trunk: name, sessions, folder.
        let mut left = vec![
            Span::raw(OVERVIEW_INDENT),
            Span::styled("├", trunk),
            Span::styled("─╮  ", branch_line),
            Span::styled(format!(" {} ", branch.name), t.pill(color)),
        ];
        if let Some(info) = live.iter().find(|b| b.name == branch.name) {
            left.push(Span::styled(
                format!("  {}", info.sessions.join(", ")),
                t.bold(),
            ));
            let folder = info.folder.file_name().map_or_else(
                || info.folder.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            left.push(Span::styled(format!("  {folder}"), t.muted()));
        }
        lines.push(justify(
            left,
            vec![Span::styled(
                format!(
                    "{} ahead · {} behind {} ",
                    branch.ahead, branch.behind, overview.trunk
                ),
                t.muted(),
            )],
            width,
        ));

        // Older commits folded, then the latest ones, the newest last.
        let spine = |glyph: &str| {
            vec![
                Span::styled("│ ", trunk),
                Span::styled(format!("{glyph}  "), branch_line),
            ]
        };
        let shown = u32::try_from(branch.commits.len()).unwrap_or(u32::MAX);
        if branch.ahead > shown {
            let mut more = vec![Span::raw(OVERVIEW_INDENT)];
            more.extend(spine("┆"));
            more.push(Span::styled(
                format!("{} more", branch.ahead - shown),
                t.muted(),
            ));
            lines.push(Line::from(more));
        }
        if branch.commits.is_empty() {
            let mut none = vec![Span::raw(OVERVIEW_INDENT)];
            none.extend(spine("╰"));
            none.push(Span::styled("no commits yet", t.muted()));
            lines.push(Line::from(none));
        }
        for point in branch.commits.iter().rev() {
            commits.push((lines.len(), point.hash.clone()));
            lines.push(commit_line(spine("●"), point));
        }
    }

    mark_commits(app, &mut lines, commits, top, width);
    lines
}

/// Makes the overview's commits clickable (`commits`: line and hash; `top`:
/// the panel line the overview starts on), and lays the open one on the
/// selection band.
fn mark_commits(
    app: &App,
    lines: &mut [Line<'static>],
    commits: Vec<(usize, String)>,
    top: usize,
    width: usize,
) {
    let open = app.shown_commit();
    let mut hits = app.commit_hits.borrow_mut();
    for (line, hash) in commits {
        if open.as_ref() == Some(&hash) {
            lines[line] = super::paint(
                std::mem::take(&mut lines[line]),
                app.theme.highlight(),
                width,
            );
        }
        hits.push((u16::try_from(top + line).unwrap_or(u16::MAX), hash));
    }
}

/// The open commit's changes, beside or under the folder panel: a fixed
/// header, then its files with their added and removed lines and its patch,
/// colored, scrolled by `scroll` lines.
pub(super) fn diff_lines(
    app: &App,
    diff: &crate::git::CommitDiff,
    hash: &str,
    width: usize,
    height: usize,
    scroll: usize,
) -> Vec<Line<'static>> {
    let t = app.theme;
    let short: String = hash.chars().take(7).collect();
    let mut lines = vec![
        Line::default(),
        justify(
            vec![
                Span::raw(" "),
                Span::styled(short, t.branch().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  Files ({})", diff.files.len()), t.bold()),
            ],
            vec![Span::styled("scroll  PgUp/PgDn  Esc close ", t.muted())],
            width,
        ),
    ];

    // The files, a blank line, then the patch: they scroll together under
    // the header, and only the lines in view are drawn.
    let total = diff.files.len() + 1 + diff.patch.len();
    let room = height.saturating_sub(lines.len());
    let most = total.saturating_sub(room);
    app.diff_most.set(most);
    let start = scroll.min(most);
    for n in start..total.min(start + room) {
        let line = if let Some(file) = diff.files.get(n) {
            file_line(app, file, width)
        } else if n == diff.files.len() {
            Line::default()
        } else {
            patch_line(app, &diff.patch[n - diff.files.len() - 1], width)
        };
        lines.push(line);
    }
    lines
}

/// A changed file: its status, path, and lines added and removed.
fn file_line(app: &App, file: &crate::git::ChangedFile, width: usize) -> Line<'static> {
    let t = app.theme;
    let status = match file.status.as_str() {
        "A" => t.working(),
        "D" => t.error(),
        _ => t.waiting(),
    };
    let counts = match (file.added, file.removed) {
        (Some(added), Some(removed)) => vec![
            Span::styled(format!("+{added}"), t.working()),
            Span::raw(" "),
            Span::styled(format!("-{removed} "), t.error()),
        ],
        _ => vec![Span::styled("binary ", t.muted())],
    };
    justify(
        vec![
            Span::raw(" "),
            Span::styled(format!("{} ", file.status), status),
            Span::raw(file.path.clone()),
        ],
        counts,
        width,
    )
}

/// A line of patch, colored as `git diff` does.
fn patch_line(app: &App, text: &str, width: usize) -> Line<'static> {
    let t = app.theme;
    let style = if text.starts_with("diff --git") {
        t.bold()
    } else if text.starts_with("+++") || text.starts_with("---") || text.starts_with("index ") {
        t.muted()
    } else if text.starts_with("@@") {
        t.branch()
    } else if text.starts_with('+') {
        t.working()
    } else if text.starts_with('-') {
        t.error()
    } else {
        Style::new()
    };
    let text = format::truncate(&format!(" {}", text.replace('\t', "    ")), width);
    Line::from(Span::styled(text, style))
}
