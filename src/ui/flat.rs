//! The "flat" layout: a borderless, agent-first list.
//!
//! Sessions that need the user are pinned on top, the others follow by
//! repository, one line each. The selected session's details are docked at
//! the bottom of the screen. Inactive sessions are always listed; exited
//! ones are folded into the footer until `h` expands them.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr as _;

use super::{BG_TAG, agent_glyph, format, gauge, justify, render_empty, status_icon, widgets};
use crate::app::App;
use crate::model::Usage;
use crate::model::{Activity, Session};
use crate::tree::RowKey;
use crate::view::{self, SessionKey, repo_of};

/// Buckets of the activity trace (two per character).
const PULSE_BUCKETS: usize = 16;
/// Narrowest screen that still shows the activity trace.
const PULSE_MIN_WIDTH: usize = 96;
/// Narrowest screen that shows the context gauge (a bare percentage below).
const GAUGE_MIN_WIDTH: usize = 72;
/// Cells of the per-session context gauge.
const CONTEXT_CELLS: usize = 5;

pub fn render(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area().inner(Margin::new(1, 0));
    let selected = app.selected_session();
    let dock = selected
        .filter(|_| app.show_detail && area.height >= 16)
        .map(|session| super::dock::dock_lines(app, session, usize::from(area.width)))
        .unwrap_or_default();
    let dock_height = u16::try_from(dock.len()).unwrap_or(0);
    let mut usage = usage_lines(app, usize::from(area.width));
    if !usage.is_empty() {
        usage.push(Line::default());
    }
    let usage_height = u16::try_from(usage.len()).unwrap_or(0);

    let [header, _, usage_area, body, dock_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(usage_height),
        Constraint::Fill(1),
        Constraint::Length(dock_height),
        Constraint::Length(1),
    ])
    .areas(area);

    widgets::render_header(app, frame, header);
    widgets::render_footer(app, frame, footer);
    frame.render_widget(Paragraph::new(usage), usage_area);
    if !dock.is_empty() {
        frame.render_widget(Paragraph::new(dock), dock_area);
    }

    if app.order().is_empty() && (app.view.inactive.is_empty() && app.view.exited.is_empty()) {
        render_empty(app, frame, body);
        return;
    }
    let (lines, selected_line, positions) = list_lines(app, usize::from(body.width));
    // Keep the selection visible, with one line of context below it.
    let visible = usize::from(body.height);
    let scroll = selected_line.map_or(0, |line| (line + 2).saturating_sub(visible));
    app.set_hits(positions.into_iter().filter_map(|(line, key)| {
        let top = line.checked_sub(scroll)?;
        (top < visible).then(|| {
            let y = body.y + u16::try_from(top).unwrap_or(0);
            (y, 1, RowKey::Session(key))
        })
    }));
    let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
}

/// Where a row is listed, which changes what it shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// In the "blocked" block: the repository name is shown.
    Pinned,
    /// Under its repository.
    Repo,
    /// In the quiet or exited sections.
    History,
}

/// Shared measurements of the list rows.
struct Rows {
    width: usize,
    name: usize,
    pulse: bool,
    gauge: bool,
}

/// Builds the list; returns its lines, the line of the selected session and
/// the line of every session (for mouse clicks).
fn list_lines(
    app: &App,
    width: usize,
) -> (Vec<Line<'static>>, Option<usize>, Vec<(usize, SessionKey)>) {
    let t = app.theme;
    let sessions = &app.snapshot.sessions;
    let selected = app.selected_session().map(view::key);
    let order = app.order();
    // Names up to 22 columns, plus the tag of detached sessions.
    let name = order
        .iter()
        .map(|&i| sessions[i].name.width().min(22) + tag_width(&sessions[i]))
        .max()
        .unwrap_or(0)
        .max(8)
        .min(width / 3);
    let rows = Rows {
        width,
        name,
        pulse: width >= PULSE_MIN_WIDTH,
        gauge: width >= GAUGE_MIN_WIDTH,
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_line = None;
    let mut positions = Vec::new();
    let mut push_row = |lines: &mut Vec<Line<'static>>, index: usize, place: Place| {
        let session = &sessions[index];
        let is_selected = selected.as_ref() == Some(&view::key(session));
        if is_selected {
            selected_line = Some(lines.len());
        }
        positions.push((lines.len(), view::key(session)));
        lines.push(row(app, session, is_selected, place, &rows));
    };

    let waiting: Vec<usize> = app.view.waiting(sessions).collect();
    if !waiting.is_empty() {
        lines.push(Line::styled("BLOCKED", t.waiting()));
        for &index in &waiting {
            push_row(&mut lines, index, Place::Pinned);
        }
        lines.push(Line::default());
    }

    for group in &app.view.repos {
        let rest: Vec<usize> = group
            .sessions
            .iter()
            .copied()
            .filter(|i| !waiting.contains(i))
            .collect();
        if rest.is_empty() {
            continue;
        }
        let mut label = vec![Span::styled(group.name.clone(), t.bold())];
        if let Some(branch) = app.snapshot.branches.get(&group.name) {
            label.push(Span::styled(format!("  {branch}"), t.muted()));
        }
        lines.push(Line::from(label));
        for index in rest {
            push_row(&mut lines, index, Place::Repo);
        }
        lines.push(Line::default());
    }

    let exited: &[usize] = if app.show_exited {
        &app.view.exited
    } else {
        &[]
    };
    for (title, list) in [("QUIET", app.view.inactive.as_slice()), ("EXITED", exited)] {
        if list.is_empty() {
            continue;
        }
        lines.push(Line::styled(title, t.muted()));
        for &index in list {
            push_row(&mut lines, index, Place::History);
        }
        lines.push(Line::default());
    }
    if lines.last().is_some_and(|l| l.width() == 0) {
        lines.pop();
    }
    (lines, selected_line, positions)
}

/// Name columns a detached session keeps before its tag is left out.
const TAGGED_NAME_MIN: usize = 8;

/// Columns the `[bg]` tag takes after a session's name.
fn tag_width(session: &Session) -> usize {
    if session.detached() { BG_TAG.len() } else { 0 }
}

/// One session, one line: state, agent, name, what it is doing, heartbeat, age.
fn row(app: &App, session: &Session, selected: bool, place: Place, rows: &Rows) -> Line<'static> {
    let t = app.theme;
    let history = place == Place::History;

    let name_style = match (selected, history) {
        (true, _) => t.selection(),
        (false, true) => t.muted(),
        (false, false) => t.bold(),
    };
    let mut left: Vec<Span<'static>> = vec![
        if selected {
            Span::styled("❯ ", t.selection())
        } else {
            Span::raw("  ")
        },
        if history && session.activity != Activity::Exited {
            super::mark("○", t.muted())
        } else {
            status_icon(app, &session.activity)
        },
        Span::raw(" "),
        agent_glyph(app, session.agent),
        Span::raw(" "),
    ];
    // A detached session's tag sits right after its name, inside the name
    // column, unless the column is too narrow to keep enough of the name.
    let tag = session.detached()
        && (rows.name >= TAGGED_NAME_MIN + BG_TAG.len()
            || session.name.width() + BG_TAG.len() <= rows.name);
    let name = if tag {
        format::truncate(&session.name, rows.name - BG_TAG.len())
    } else {
        format::fit(&session.name, rows.name)
    };
    let pad = rows.name.saturating_sub(name.width() + BG_TAG.len());
    if session.activity == Activity::Working && !history {
        left.extend(super::shimmer(app, &name, selected));
    } else {
        left.push(Span::styled(name, name_style));
    }
    if tag {
        left.push(Span::styled(BG_TAG, t.muted()));
        left.push(Span::raw(" ".repeat(pad)));
    }
    left.push(Span::raw("  "));
    if place == Place::Pinned {
        left.push(Span::styled(
            format!("{}  ", repo_of(&app.root, &session.cwd).0),
            t.muted(),
        ));
    }
    left.extend(widgets::activity_spans(app, session, history));

    let mut right = Vec::new();
    if rows.pulse {
        right.extend(widgets::pulse_spans(app, session, PULSE_BUCKETS, history));
        right.push(Span::raw("   "));
    }
    right.extend(context_spans(app, session, rows.gauge));
    right.push(Span::raw("  "));
    let since = match (&session.activity, &session.current_tool) {
        (Activity::Working, Some(tool)) => tool.started_at.unwrap_or(session.last_active),
        _ => session.last_active,
    };
    right.push(Span::styled(
        format!("{:>5}", format::since(app.now, since)),
        t.muted(),
    ));
    justify(left, right, rows.width)
}

/// How full the context window is: a small gauge and a percentage, or the
/// raw token count when the window size is unknown.
fn context_spans(app: &App, session: &Session, with_gauge: bool) -> Vec<Span<'static>> {
    let t = app.theme;
    let width = if with_gauge { CONTEXT_CELLS + 5 } else { 4 };
    match (session.context_percent(), session.context_tokens) {
        (Some(percent), _) => {
            let mut spans = if with_gauge {
                let mut gauge = gauge(t, percent, CONTEXT_CELLS);
                gauge.push(Span::raw(" "));
                gauge
            } else {
                Vec::new()
            };
            spans.push(Span::styled(format!("{percent:>3.0}%"), t.level(percent)));
            spans
        }
        (None, Some(tokens)) => vec![Span::styled(
            format!("{:>width$}", format::tokens(tokens)),
            t.muted(),
        )],
        (None, None) => vec![Span::raw(" ".repeat(width))],
    }
}

/// Account quotas, one line per signed-in agent (wrapped when narrow).
fn usage_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = app.theme;
    let cells = if width >= 100 { 10 } else { 6 };
    let mut lines = vec![system_line(app, cells)];
    for usage in &app.snapshot.usage {
        let prefix = vec![
            Span::raw("  "),
            agent_glyph(app, usage.agent),
            Span::raw(" "),
            Span::styled(format::fit(usage.agent.display_name(), 9), t.bold()),
            Span::styled(
                format::fit(usage.plan.as_deref().unwrap_or(""), 8),
                t.muted(),
            ),
        ];
        let indent: usize = prefix.iter().map(|s| s.content.width()).sum();
        let mut line = prefix;
        let mut used = indent;
        for group in limit_groups(app, usage, cells) {
            let group_width: usize = group.iter().map(|s| s.content.width()).sum();
            if used > indent && used + group_width > width {
                lines.push(Line::from(std::mem::take(&mut line)));
                line.push(Span::raw(" ".repeat(indent)));
                used = indent;
            }
            used += group_width;
            line.extend(group);
        }
        lines.push(Line::from(line));
    }
    lines
}

/// `▤ System   cpu ▰▰▱▱▱  23%   mem ▰▰▰▱▱  61%   agents ▰▱▱▱▱  12% 2.1G`
#[allow(clippy::cast_precision_loss)]
fn system_line(app: &App, cells: usize) -> Line<'static> {
    let t = app.theme;
    let load = &app.snapshot.system;
    let machine = &load.machine;
    let memory = if machine.memory_total > 0 {
        machine.memory_used as f64 / machine.memory_total as f64 * 100.0
    } else {
        0.0
    };
    let agents = if machine.cores > 0 {
        f64::from(load.agents_cpu) / machine.cores as f64
    } else {
        0.0
    };
    let mut spans = vec![
        Span::styled("  ▤ ", t.muted()),
        Span::styled(format::fit("System", 17), t.bold()),
    ];
    for (label, percent, note) in [
        ("cpu", f64::from(machine.cpu), String::new()),
        ("mem", memory, String::new()),
        (
            "agents",
            agents,
            format!(" {}", format::bytes(load.agents_memory)),
        ),
    ] {
        spans.push(Span::styled(format!("{label} "), t.muted()));
        spans.extend(gauge(t, percent, cells));
        spans.push(Span::styled(format!(" {percent:>3.0}%"), t.level(percent)));
        spans.push(Span::styled(format!("{note}     "), t.muted()));
    }
    Line::from(spans)
}

/// One span group per limit: `week ▰▰▰▱▱▱▱▱▱▱  23% ↻ 6d`, plus the data's age
/// when it is stale.
fn limit_groups(app: &App, usage: &Usage, cells: usize) -> Vec<Vec<Span<'static>>> {
    let t = app.theme;
    let groups: Vec<Vec<Span<'static>>> = usage
        .limits
        .iter()
        .map(|limit| {
            let mut group = vec![Span::styled(
                format!("{} ", format::truncate(&limit.label, 8)),
                t.muted(),
            )];
            group.extend(gauge(t, limit.percent, cells));
            group.push(Span::styled(
                format!(" {:>3.0}%", limit.percent),
                t.level(limit.percent),
            ));
            if let Some(reset) = limit.resets_at {
                let left = reset.duration_since(app.now).unwrap_or_default();
                group.push(Span::styled(
                    format!(" ↻ {:<4}", format::duration(left)),
                    t.muted(),
                ));
            }
            if let Some(age) = widgets::limit_age(app, limit) {
                group.push(Span::styled(format!(" {age}"), t.faint_italic()));
            }
            group.push(Span::raw("   "));
            group
        })
        .collect();
    groups
}
