//! The default layout: the machine and the accounts' quotas in panels at the
//! top, the sessions in a tree of their folders, and the details of the
//! selected session or git folder docked at the bottom.
//!
//! Panels are drawn on a surface derived from the terminal's own background.
//! The tree is light: one line per folder and per session; folders open and
//! close, and a closed folder folds its exited sessions into one line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr as _;

use super::{State, Theme, agent_glyph, format, gauge, justify, paint, render_empty, widgets};
use crate::app::App;
use crate::model::{Activity, Limit, Session, Usage};
use crate::tree::{Row, RowKey, Status};

/// Space between side-by-side panels: two columns, which look as wide as
/// the one-line gap between stacked panels (a cell is about twice as tall
/// as it is wide).
const GAP: u16 = 2;
/// Narrowest quota panel before panels merge into one.
const MIN_USAGE_WIDTH: u16 = 46;

pub fn render(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area().inner(Margin::new(1, 0));
    // On short terminals the list comes first: the machine's box goes,
    // then the quotas.
    let usage_height = if area.height < SHORT / 2 + 4 {
        0
    } else {
        usage_height(app, area.width)
    };
    let system_height = if area.height < SHORT {
        0
    } else {
        system_height(app)
    };
    // The selected session's details, or the selected folder's repository,
    // docked at the bottom.
    let width = usize::from(area.width);
    app.commit_hits.borrow_mut().clear();
    app.diff_area.set(Rect::default());
    // An open commit's changes sit beside the folder panel on wide screens,
    // under it on narrow ones.
    let commit = app
        .shown_commit()
        .filter(|_| app.show_detail && app.selected_session().is_none());
    let dock_width = if commit.is_some() && width >= SIDE_BY_SIDE {
        width * 45 / 100
    } else {
        width
    };
    let dock: Vec<Line<'static>> = if !app.show_detail || area.height < 30 {
        Vec::new()
    } else if let Some(session) = app.selected_session() {
        super::dock::dock_lines(app, session, width)
    } else if let Some(folder) = app.selected_folder() {
        super::dock::folder_lines(app, &folder, dock_width)
    } else {
        Vec::new()
    };
    // A repository's branches get a little more room than a session.
    let share = if app.selected_folder().is_some() {
        3
    } else {
        2
    };
    let dock_height = if commit.is_some() {
        area.height * 3 / 5
    } else {
        u16::try_from(dock.len())
            .unwrap_or(0)
            .min(area.height * share / 5)
    };
    let [header, _, system, _, usage, _, body, dock_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(system_height),
        Constraint::Length(u16::from(system_height > 0)),
        Constraint::Length(usage_height),
        Constraint::Length(u16::from(usage_height > 0)),
        Constraint::Fill(1),
        Constraint::Length(dock_height),
        Constraint::Length(1),
    ])
    .areas(area);

    widgets::render_header(app, frame, header);
    widgets::render_footer(app, frame, footer);
    render_system(app, frame, system);
    render_usage(app, frame, usage);
    render_dock(app, frame, dock_area, dock, commit.as_deref());

    if app.snapshot.sessions.is_empty() {
        render_empty(app, frame, body);
    } else {
        render_tree(app, frame, body);
    }
}

/// Draws `lines` on a raised surface filling `area`.
fn panel(frame: &mut Frame<'_>, area: Rect, surface: Style, lines: Vec<Line<'static>>) {
    let width = usize::from(area.width);
    let mut painted: Vec<Line<'static>> = lines
        .into_iter()
        .map(|l| paint(l, surface, width))
        .collect();
    painted.resize_with(usize::from(area.height), || {
        paint(Line::default(), surface, width)
    });
    frame.render_widget(Paragraph::new(painted), area);
}

/// Terminal height under which the machine's box is left out (and, under
/// about half of it, the quota panels too), so the sessions keep room.
const SHORT: u16 = 32;

/// Width from which an open commit's changes sit beside the folder panel
/// rather than under it.
const SIDE_BY_SIDE: usize = 100;
/// Rows an open commit's changes need, at least, under the folder panel.
const MIN_DIFF_ROWS: u16 = 10;

/// The details panel; with an open commit, the folder panel and the
/// commit's changes, side by side or stacked, or a note when the terminal
/// is too short for them.
fn render_dock(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    dock: Vec<Line<'static>>,
    commit: Option<&str>,
) {
    let t = app.theme;
    let folder = app.selected_folder();
    let (Some(hash), Some(folder)) = (commit, folder) else {
        panel(frame, area, t.surface(), dock);
        place_commits(app, area);
        return;
    };
    let (left, right) = if usize::from(area.width) >= SIDE_BY_SIDE {
        let [left, _, right] = Layout::horizontal([
            Constraint::Percentage(45),
            Constraint::Length(GAP),
            Constraint::Fill(1),
        ])
        .areas(area);
        (left, right)
    } else {
        let wanted = u16::try_from(dock.len()).unwrap_or(u16::MAX);
        let [top, _, bottom] = Layout::vertical([
            Constraint::Length(wanted.min(area.height / 2)),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(area);
        (top, bottom)
    };
    panel(frame, left, t.surface(), dock);
    place_commits(app, left);

    let changes = if right.height < MIN_DIFF_ROWS {
        too_small(app, right, frame.area(), MIN_DIFF_ROWS - right.height)
    } else {
        match app.commit_diff(&folder, hash) {
            Some(diff) => super::dock::diff_lines(
                app,
                &diff,
                hash,
                usize::from(right.width),
                usize::from(right.height),
                app.diff_scroll,
            ),
            None => vec![
                Line::default(),
                Line::from(Span::styled(" reading…", t.muted())),
            ],
        }
    };
    panel(frame, right, t.surface(), changes);
    app.diff_area.set(right);
}

/// Places the folder panel's clickable commits on screen: they were
/// numbered from the top of the panel drawn in `area`.
fn place_commits(app: &App, area: Rect) {
    app.commit_hits.borrow_mut().retain_mut(|(row, _)| {
        *row = row.saturating_add(area.y);
        *row < area.bottom()
    });
}

/// In place of what does not fit, the terminal's size and the rows missing,
/// as btop does: centered in `area`.
fn too_small(app: &App, area: Rect, screen: Rect, missing: u16) -> Vec<Line<'static>> {
    let t = app.theme;
    let (width, height) = (screen.width, screen.height);
    let text = [
        (
            String::from("Terminal size too small:"),
            t.waiting().add_modifier(Modifier::BOLD),
        ),
        (format!("Width = {width} Height = {height}"), t.muted()),
        (String::new(), t.muted()),
        (String::from("Needed for the diff:"), t.bold()),
        (
            format!("Width = {width} Height = {}", height + missing),
            t.muted(),
        ),
    ];
    let top = usize::from(area.height).saturating_sub(text.len()) / 2;
    let mut lines = vec![Line::default(); top];
    lines.extend(
        text.into_iter()
            .map(|(line, style)| Line::from(Span::styled(line, style)).centered()),
    );
    lines
}

/// Shifts a line right by the panels' inner margin.
fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(line.spans);
    Line::from(spans)
}

// Quotas ----------------------------------------------------------------

/// Quota panels share one row when they fit side by side, otherwise merge.
fn usage_merged(app: &App, width: u16) -> bool {
    let count = u16::try_from(app.snapshot.usage.len()).unwrap_or(u16::MAX);
    count > 1 && width < (MIN_USAGE_WIDTH + GAP).saturating_mul(count)
}

fn usage_height(app: &App, width: u16) -> u16 {
    let usages = &app.snapshot.usage;
    if usages.is_empty() {
        return 0;
    }
    let rows = if usage_merged(app, width) {
        usages.iter().map(|u| u.limits.len()).sum()
    } else {
        usages.iter().map(|u| u.limits.len()).max().unwrap_or(0)
    };
    // Padding, title, rows, padding.
    u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(3)
}

fn render_usage(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let t = app.theme;
    let usages = &app.snapshot.usage;
    if usages.is_empty() || area.is_empty() {
        return;
    }
    if usage_merged(app, area.width) {
        let width = usize::from(area.width).saturating_sub(6);
        let mut lines = vec![Line::default(), indent(Line::styled("usage", t.muted()))];
        for usage in usages {
            for limit in &usage.limits {
                let mut spans = vec![agent_glyph(app, usage.agent), Span::raw(" ")];
                spans.extend(limit_spans(app, limit, width, true));
                lines.push(indent(Line::from(spans)));
            }
        }
        panel(frame, area, t.surface(), lines);
        return;
    }
    let count = u16::try_from(usages.len()).unwrap_or(1).max(1);
    let each = area.width.saturating_sub(GAP * (count - 1)) / count;
    for (n, usage) in usages.iter().enumerate() {
        let n = u16::try_from(n).unwrap_or(0);
        let x = area.x + n * (each + GAP);
        // The last panel takes the rounding remainder.
        let width = if n + 1 == count {
            area.right() - x
        } else {
            each
        };
        render_usage_panel(app, usage, frame, Rect { x, width, ..area });
    }
}

fn render_usage_panel(app: &App, usage: &Usage, frame: &mut Frame<'_>, area: Rect) {
    let t = app.theme;
    let width = usize::from(area.width).saturating_sub(4);
    let mut right = Vec::new();
    if let Some(plan) = &usage.plan {
        right.push(Span::styled(plan.clone(), t.muted()));
    }
    let title = justify(
        vec![
            agent_glyph(app, usage.agent),
            Span::raw(" "),
            Span::styled(usage.agent.display_name(), t.bold()),
        ],
        right,
        width + 1,
    );

    let mut lines = vec![Line::default(), indent(title)];
    // Reserve the age column only in panels that have a stale number.
    let ages = usage
        .limits
        .iter()
        .any(|l| widgets::limit_age(app, l).is_some());
    for limit in &usage.limits {
        lines.push(indent(Line::from(limit_spans(app, limit, width, ages))));
    }
    panel(frame, area, t.surface(), lines);
}

/// `week  ▰▰▰▰▰▱▱▱▱▱▱▱   32%  ↻ 6d`, the gauge filling `width`.
fn limit_spans(app: &App, limit: &Limit, width: usize, age_column: bool) -> Vec<Span<'static>> {
    let reset = limit.resets_at.map_or_else(String::new, |r| {
        let left = r.duration_since(app.now).unwrap_or_default();
        format!("  ↻{:<4}", format::duration(left))
    });
    let reset = format!("{reset:<7}");
    // A fixed-width age column keeps every gauge of a panel the same length.
    let age = widgets::limit_age(app, limit).map_or_else(String::new, |age| {
        format!("⧗{}", age.trim_end_matches(" old"))
    });
    let mut tail = vec![Span::styled(reset, app.theme.muted())];
    if age_column {
        tail.push(Span::styled(
            format!("  {age:<5}"),
            app.theme.faint_italic(),
        ));
    }
    meter_spans(app, &limit.label, limit.percent, tail, width)
}

/// `label ▰▰▰▰▰▱▱▱▱▱▱▱   32%<tail>`, the gauge filling what `width` leaves.
fn meter_spans(
    app: &App,
    label: &str,
    percent: f64,
    tail: Vec<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let t = app.theme;
    let label = format::fit(label, 6);
    let value = format!(" {percent:>4.0}%");
    let tail_width: usize = tail.iter().map(|s| s.content.width()).sum();
    let cells = width.saturating_sub(label.width() + 1 + value.width() + tail_width);
    let mut spans = vec![Span::styled(label, t.muted()), Span::raw(" ")];
    spans.extend(gauge(t, percent, cells));
    spans.push(Span::styled(
        value,
        t.level(percent).add_modifier(Modifier::BOLD),
    ));
    spans.extend(tail);
    spans
}

// System ----------------------------------------------------------------
//
// Laid out like btop: one box across the screen with the CPU history graph,
// a small graph per core and the memory meters. Narrow screens get the CPU
// graph with memory and agent meters below.

/// Narrowest screen with the full btop-like layout.
const SYSTEM_WIDE: u16 = 96;
/// Minimum content rows of the CPU box.
const CPU_MIN_ROWS: usize = 5;
/// Per-core entries: `C10 ⣀⣤⣶⣿⣿⣶  34%`, in two columns.
const CORE_SPARK: usize = 6;
const CORE_ENTRY: usize = 4 + CORE_SPARK + 5;
/// Space between the columns of the system box.
const COLUMN_GAP: usize = 4;
const CORE_COLUMNS: usize = 2;
/// Longest CPU history graph, in characters.
const GRAPH_MAX_WIDTH: usize = 40;
/// Width of the labels of the memory box.
const MEMORY_LABEL: usize = 10;

fn cpu_rows(app: &App) -> usize {
    app.snapshot
        .system
        .machine
        .per_core
        .len()
        .div_ceil(CORE_COLUMNS)
        .max(CPU_MIN_ROWS)
}

/// Height of the whole System section.
fn system_height(app: &App) -> u16 {
    u16::try_from(cpu_rows(app) + 3).unwrap_or(u16::MAX)
}

/// Share of `part` in `whole`, in percent.
#[allow(clippy::cast_precision_loss)]
fn share(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

/// Agents' CPU as a share of the whole machine.
fn agents_share(app: &App) -> f64 {
    let load = &app.snapshot.system;
    let cores = u32::try_from(load.machine.cores).unwrap_or(0);
    if cores == 0 {
        0.0
    } else {
        f64::from(load.agents_cpu) / f64::from(cores)
    }
}

/// Memory and agent meters, for narrow screens.
fn memory_and_agents(app: &App, width: usize) -> [Vec<Span<'static>>; 2] {
    let t = app.theme;
    let load = &app.snapshot.system;
    let machine = &load.machine;
    let note = |text: String| vec![Span::styled(format!("  {text:<11}"), t.muted())];
    [
        meter_spans(
            app,
            "mem",
            share(machine.memory_used, machine.memory_total),
            note(format!(
                "{} / {}",
                format::bytes(machine.memory_used),
                format::bytes(machine.memory_total)
            )),
            width,
        ),
        meter_spans(
            app,
            "agents",
            agents_share(app),
            note(format::bytes(load.agents_memory)),
            width,
        ),
    ]
}

/// `3d 04h`, `5h12`, `42m`.
fn uptime(seconds: u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h{minutes:02}"),
        _ => format!("{days}d {hours:02}h"),
    }
}

fn render_system(app: &App, frame: &mut Frame<'_>, area: Rect) {
    if !area.is_empty() {
        render_cpu_box(app, frame, area, area.width >= SYSTEM_WIDE);
    }
}

/// The CPU box: the machine's history graph, and one small graph per core.
fn render_cpu_box(app: &App, frame: &mut Frame<'_>, area: Rect, wide: bool) {
    let t = app.theme;
    let machine = &app.snapshot.system.machine;
    let width = usize::from(area.width).saturating_sub(4);
    let rows = cpu_rows(app);
    let history: Vec<f32> = app.cpu_history.iter().copied().collect();

    let mut left = vec![
        Span::styled("system", t.bold()),
        Span::styled("   cpu", t.muted()),
        Span::styled(
            format!("  {:.0}%", machine.cpu),
            t.level(f64::from(machine.cpu)).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(brand) = &machine.brand {
        left.push(Span::styled(format!("   {brand}"), t.muted()));
    }
    let mut right = Vec::new();
    if let Some((one, five, fifteen)) = machine.load {
        right.push(Span::styled(
            format!("load {one:.2} {five:.2} {fifteen:.2}"),
            t.muted(),
        ));
    }
    if let Some(up) = machine.uptime {
        right.push(Span::styled(format!("   up {}", uptime(up)), t.muted()));
    }
    right.push(Span::styled(
        format!("   {} cores", machine.cores),
        t.muted(),
    ));
    let mut lines = vec![Line::default(), indent(justify(left, right, width + 1))];

    if !wide {
        let graph = super::graph::area(t, &history, width, rows - 2);
        lines.extend(graph.into_iter().map(indent));
        lines.extend(
            memory_and_agents(app, width)
                .into_iter()
                .map(|row| indent(Line::from(row))),
        );
        panel(frame, area, t.surface(), lines);
        return;
    }

    // Graph (capped), cores in two columns, then memory taking the rest.
    // Every column is set apart by the same gap.
    let cores_width = CORE_COLUMNS * (CORE_ENTRY + COLUMN_GAP) - COLUMN_GAP;
    let graph_width = (width * 3 / 10).clamp(16, GRAPH_MAX_WIDTH);
    let memory_width = width.saturating_sub(graph_width + cores_width + 2 * COLUMN_GAP);
    let graph = super::graph::area(t, &history, graph_width, rows);
    let mut cores: Vec<Vec<Span<'static>>> = vec![Vec::new(); rows];
    for (n, (usage, history)) in machine.per_core.iter().zip(&app.core_history).enumerate() {
        let samples: Vec<f32> = history.iter().copied().collect();
        let level = t.level(f64::from(*usage));
        let row = &mut cores[n % rows];
        let cell = vec![
            Span::styled(format!("C{n:<2} "), t.muted()),
            super::graph::spark(t, &samples, CORE_SPARK, Some(level)),
            Span::styled(format!(" {usage:>3.0}%"), level),
        ];
        let used: usize = cell.iter().map(|s| s.content.width()).sum();
        row.extend(cell);
        row.push(Span::raw(
            " ".repeat((CORE_ENTRY + COLUMN_GAP).saturating_sub(used)),
        ));
    }
    let mut memory = memory_rows(app, memory_width);
    memory.resize(rows, Vec::new());
    for ((graph_line, core_line), memory_line) in graph.into_iter().zip(cores).zip(memory) {
        let mut spans = graph_line.spans;
        spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
        let used: usize = core_line.iter().map(|s| s.content.width()).sum();
        spans.extend(core_line);
        spans.push(Span::raw(
            " ".repeat((cores_width + COLUMN_GAP).saturating_sub(used)),
        ));
        spans.extend(memory_line);
        lines.push(indent(Line::from(spans)));
    }
    panel(frame, area, t.surface(), lines);
}

/// The memory column of the system box: used, available, free and swap,
/// plus the agents' share, each with a gauge filling `width`.
fn memory_rows(app: &App, width: usize) -> Vec<Vec<Span<'static>>> {
    let t = app.theme;
    let load = &app.snapshot.system;
    let machine = &load.machine;
    let row = |label: &str, percent: f64, amount: u64| {
        let value = format!(" {percent:>4.0}%  {:>5}", format::bytes(amount));
        let cells = width.saturating_sub(MEMORY_LABEL + value.width());
        let mut spans = vec![Span::styled(format::fit(label, MEMORY_LABEL), t.muted())];
        spans.extend(gauge(t, percent, cells));
        spans.push(Span::styled(value, t.level(percent)));
        spans
    };
    let mut rows = vec![
        row(
            "mem used",
            share(machine.memory_used, machine.memory_total),
            machine.memory_used,
        ),
        row(
            "available",
            share(machine.memory_available, machine.memory_total),
            machine.memory_available,
        ),
        row(
            "free",
            share(machine.memory_free, machine.memory_total),
            machine.memory_free,
        ),
    ];
    if machine.swap_total > 0 {
        rows.push(row(
            "swap",
            share(machine.swap_used, machine.swap_total),
            machine.swap_used,
        ));
    }
    rows.push(row(
        "agents",
        share(load.agents_memory, machine.memory_total),
        load.agents_memory,
    ));
    rows
}

// Tree ------------------------------------------------------------------

/// Width of the activity trace at the end of a session line.
const TRACE_WIDTH: usize = 24;
/// Columns of a session's name kept before its age gives way on a short
/// line of the tree.
const NAME_FLOOR: usize = 8;
/// Width of the context percentage after it (`100%`).
const CONTEXT_WIDTH: usize = 4;
/// Width of the tokens column at the end of session and sub-agent lines.
const TOKENS_WIDTH: usize = 5;

/// The accent color of a status.
fn status_color(t: Theme, status: Status) -> Color {
    t.state_color(match status {
        Status::Blocked => State::Blocked,
        Status::Working => State::Working,
        Status::Ready => State::Ready,
        Status::Quiet | Status::Exited => State::Faded,
    })
}

/// The status mark ahead of a session: a spinner while it works.
fn status_mark(app: &App, status: Status) -> Span<'static> {
    let t = app.theme;
    let symbol = match status {
        Status::Blocked => "⚑",
        Status::Working => return super::working_mark(app),
        Status::Ready => "✓",
        Status::Quiet => "○",
        Status::Exited => "✕",
    };
    super::mark(symbol, t.fg_bold(status_color(t, status)))
}

/// Indentation guides: one faint bar per level.
fn guides(app: &App, depth: usize) -> Span<'static> {
    Span::styled("│ ".repeat(depth), app.theme.track())
}

fn render_tree(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let rows = app.tree_rows();
    let cursor = app.tree_cursor(&rows);
    let sessions = &app.snapshot.sessions;
    let width = usize::from(area.width);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut targets: Vec<(usize, usize, RowKey)> = Vec::new();
    let mut cursor_line = 0;
    let mut cursor_end = 0;
    for (n, row) in rows.iter().enumerate() {
        if n > 0 && matches!(row, Row::Folder { depth: 0, .. }) {
            lines.push(Line::default());
        }
        // A little air above each session, and above the hidden ones, so
        // that line reads as one of the folder's rows.
        if let Row::Session { depth, .. } | Row::More { depth, .. } = row {
            lines.push(Line::from(vec![Span::raw("  "), guides(app, *depth)]));
        }
        let selected = cursor == Some(n);
        if selected {
            cursor_line = lines.len();
        }
        let start = lines.len();
        match row {
            Row::Folder { .. } => lines.push(folder_line(app, row, selected)),
            Row::More { depth, count, .. } => {
                lines.push(more_line(app, *depth, *count, selected));
            }
            Row::Session {
                index,
                depth,
                status,
            } => lines.extend(session_line(
                app,
                &sessions[*index],
                *depth,
                *status,
                selected,
                width,
            )),
        }
        if selected {
            cursor_end = lines.len();
        }
        targets.push((start, lines.len() - start, row.key(sessions)));
    }

    // Scroll just enough to show the whole selected block (a session with its
    // gauge, stats and sub-agents), keeping its first line in view.
    let visible = usize::from(area.height);
    let follow = (cursor_end + 1)
        .saturating_sub(visible)
        .min(cursor_line.saturating_sub(1));
    // The mouse wheel scrolls freely; keys bring the selection back in view.
    let most = lines.len().saturating_sub(visible);
    let scroll = app.tree_scroll.map_or(follow, |wheel| wheel.min(most));
    app.tree_offset.set((scroll, most));
    app.set_hits(targets.into_iter().filter_map(|(line, height, key)| {
        let top = line.checked_sub(scroll)?;
        let height = u16::try_from(height).unwrap_or(1);
        (top < visible).then(|| (area.y + u16::try_from(top).unwrap_or(0), height, key))
    }));
    // The selected row (with its sub-agents) on a gray band.
    let band = app.theme.selected_row();
    for line in lines.iter_mut().take(cursor_end).skip(cursor_line) {
        *line = paint(std::mem::take(line), band, usize::from(area.width));
    }
    let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

/// The selection arrow in the left margin, or a blank of the same width.
fn cursor_mark(app: &App, selected: bool) -> Span<'static> {
    if selected {
        Span::styled("❯ ", app.theme.selection())
    } else {
        Span::raw("  ")
    }
}

/// `▸ api-server  main`: a folder, open or closed, in orange when a session
/// in it is blocked, with its branch at the top level.
fn folder_line(app: &App, row: &Row, selected: bool) -> Line<'static> {
    let t = app.theme;
    let Row::Folder {
        name,
        depth,
        open,
        blocked,
        ..
    } = row
    else {
        return Line::default();
    };

    let name_style = if selected {
        t.selection()
    } else if *blocked {
        t.waiting()
    } else {
        t.bold()
    };
    let mut spans = vec![
        cursor_mark(app, selected),
        guides(app, *depth),
        Span::styled(if *open { "▾ " } else { "▸ " }, t.muted()),
        Span::styled(name.clone(), name_style),
    ];
    if *depth == 0
        && let Some(branch) = app.snapshot.branches.get(name)
    {
        spans.push(Span::styled(format!("  {branch}"), t.muted()));
    }
    Line::from(spans)
}

/// `✕ 3 hidden`: exited sessions hidden by a collapsed folder.
fn more_line(app: &App, depth: usize, count: usize, selected: bool) -> Line<'static> {
    let t = app.theme;
    let style = if selected { t.selection() } else { t.muted() };
    // A row of its own in the folder, like the exited sessions it stands for:
    // their mark in the status column, then how many are hidden.
    Line::from(vec![
        cursor_mark(app, selected),
        guides(app, depth),
        super::mark("✕", t.muted()),
        Span::raw(" "),
        Span::styled(format!("{count} hidden"), style),
    ])
}

/// What a session is doing at this moment.
fn action(app: &App, session: &Session, status: Status) -> Vec<Span<'static>> {
    let t = app.theme;
    if status == Status::Exited {
        return vec![Span::styled(
            session.agent.resume_args(&session.id),
            t.muted(),
        )];
    }
    // The model and how hard it thinks; tools and commands are in the
    // details panel.
    let mut spans = match &session.model {
        Some(model) => {
            let style = if status == Status::Quiet {
                t.muted()
            } else {
                t.agent(session.agent)
            };
            let mut spans = vec![Span::styled(format::model(session.agent, model), style)];
            if let Some(effort) = &session.effort {
                spans.push(Span::styled(format!(" ({effort})"), t.muted()));
            }
            spans
        }
        None => vec![Span::styled(session.agent.label(), t.muted())],
    };
    if let Some(tasks) = super::background_tasks(session) {
        spans.push(Span::styled(format!(" · {tasks}"), t.branch()));
    }
    if let Activity::Waiting { reason } = &session.activity {
        spans.push(Span::styled(format!("  {reason}"), t.waiting()));
    }
    spans
}

/// Cuts `spans` to `width` columns, ending with `…` when something is cut.
fn truncate_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = span.content.width();
        if used + w <= width {
            used += w;
            out.push(span);
        } else {
            let room = width.saturating_sub(used);
            if room > 0 {
                out.push(Span::styled(
                    format::truncate(&span.content, room),
                    span.style,
                ));
            }
            break;
        }
    }
    out
}

/// The branch a session works on: its worktree's, the one its agent
/// recorded, or its repository's.
fn session_branch(app: &App, session: &Session) -> Option<String> {
    worktree_branch(session)
        .or_else(|| session.git_branch.clone().filter(|b| b != "HEAD"))
        .or_else(|| {
            let (repo, _) = crate::view::repo_of(&app.root, &session.cwd);
            app.snapshot.branches.get(&repo).cloned()
        })
}

/// The branch of a session running in a linked worktree.
fn worktree_branch(session: &Session) -> Option<String> {
    let worktree = session.worktree.as_ref()?;
    worktree
        .branch
        .clone()
        .or_else(|| session.git_branch.clone())
}

/// Sub-agents listed under a session.
const SHOWN_SUBAGENTS: usize = 3;

/// Tokens processed, flush right at the very end of a line: `1.9M`.
fn tokens_span(app: &App, tokens: Option<u64>) -> Span<'static> {
    let text = tokens.map(format::tokens).unwrap_or_default();
    Span::styled(format!("  {text:>TOKENS_WIDTH$} "), app.theme.muted())
}

/// A sub-agent's line: the text on the left, and the tokens it used on the
/// right.
type SubAgentLine = (Vec<Span<'static>>, Vec<Span<'static>>);

/// Running sub-agents of a session, one per line, one level deeper than it.
fn subagent_lines(app: &App, session: &Session, depth: usize) -> Vec<SubAgentLine> {
    let t = app.theme;
    // The parent's guides, then plain indentation: no bar runs down the
    // sub-agents.
    let lead = || {
        vec![
            Span::raw("  "),
            guides(app, depth),
            Span::raw(" ".repeat(super::MARK_WIDTH + 1)),
        ]
    };
    let mut lines = Vec::new();
    for agent in session.subagents.iter().take(SHOWN_SUBAGENTS) {
        // `└─ Explore - Reading lexer.rs`: the agent type as Claude Code
        // records it, then what it is doing now.
        let mut spans = lead();
        // One started by another sub-agent sits under that one's type.
        spans.push(Span::raw("   ".repeat(agent.level)));
        spans.push(Span::styled("└─ ", t.muted()));
        spans.push(Span::styled(
            agent.kind.clone().unwrap_or_else(|| "agent".to_owned()),
            t.agent(session.agent),
        ));
        if let Some(model) = super::subagent_model(session, agent) {
            spans.push(Span::styled(format!(" ({model})"), t.muted()));
        }
        spans.push(Span::styled(" - ", t.muted()));
        // Its latest step, as Claude Code shows it; the mission otherwise.
        let label = agent.activity.as_ref().unwrap_or(&agent.description);
        spans.push(Span::styled(label.clone(), t.text_dim()));
        lines.push((spans, vec![tokens_span(app, agent.tokens_used)]));
    }
    if session.subagents.len() > SHOWN_SUBAGENTS {
        let mut spans = lead();
        spans.push(Span::styled(
            format!(
                "└─ {} more sub-agents",
                session.subagents.len() - SHOWN_SUBAGENTS
            ),
            t.muted(),
        ));
        lines.push((spans, Vec::new()));
    }
    lines
}

/// One session's line (and its sub-agents' below): status, agent, name,
/// branch, model and background shells, how long ago it last moved; flush
/// right, its activity trace, the context used and the tokens spent.
fn session_line(
    app: &App,
    session: &Session,
    depth: usize,
    status: Status,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let t = app.theme;
    let faded = matches!(status, Status::Quiet | Status::Exited);
    let name_style = match (selected, faded) {
        (true, _) => t.selection(),
        (false, true) => t.muted(),
        (false, false) => t.bold(),
    };
    let mut left = vec![
        cursor_mark(app, selected),
        guides(app, depth),
        status_mark(app, status),
        Span::raw(" "),
        agent_glyph(app, session.agent),
        Span::raw(" "),
    ];

    // End of the line: the activity trace (none once the session exited),
    // the share of the context window in use and the tokens used. CPU and
    // memory are in the details panel.
    let mut right = Vec::new();
    if status == Status::Exited {
        right.push(Span::raw(" ".repeat(TRACE_WIDTH)));
    } else {
        right.extend(widgets::pulse_spans(app, session, TRACE_WIDTH * 2, faded));
    }
    right.push(Span::raw("   "));
    right.extend(context_spans(app, session, faded));
    right.push(tokens_span(app, session.tokens_used));

    // Name, its tag and worktree branch, what it is doing, then how long ago:
    // one run of text in what `justify` leaves of the line. The age is set
    // aside first so it shows whole, unless that would leave less than a few
    // letters of the name: then it gives way, whole. The name is shortened
    // if needed, the tag shows whole or not at all, the branch only if a few
    // letters of it fit, and the action takes what is left.
    let width_of = |spans: &[Span<'_>]| spans.iter().map(|s| s.content.width()).sum::<usize>();
    let prefix = width_of(&left);
    let gauges = width_of(&right) + 1;
    let name_floor = session.name.width().min(NAME_FLOOR);
    let age = (status != Status::Exited)
        .then(|| format!(" - {}", format::since(app.now, session.last_active)))
        .filter(|age| prefix + name_floor + age.width() + gauges <= width);
    // An exited session's resume id must stay whole to be copied: it is set
    // aside instead of the age, and the gauges give way to it.
    let resume = (status == Status::Exited).then(|| action(app, session, status));
    let room = match &resume {
        Some(resume) => width.saturating_sub(width_of(resume) + 2),
        None => width.saturating_sub(gauges + age.as_ref().map_or(0, |age| age.width())),
    };
    let name = format::truncate(&session.name, room.saturating_sub(prefix));
    if status == Status::Working {
        left.extend(super::shimmer(app, &name, selected));
    } else {
        left.push(Span::styled(name, name_style));
    }
    if session.detached() && width_of(&left) + super::BG_TAG.len() <= room {
        // Running in the background, with no terminal to jump to.
        left.push(Span::styled(super::BG_TAG, t.muted()));
    }
    if let Some(branch) = session_branch(app, session) {
        let branch = format!(" {branch}");
        let free = room.saturating_sub(width_of(&left));
        if branch.width() <= free || free >= 6 {
            // Discreet: the branch is context, not the headline.
            left.push(Span::styled(format::truncate(&branch, free), t.muted()));
        }
    }

    if let Some(resume) = resume {
        left.push(Span::raw(" "));
        left.extend(resume);
        let needed: usize = left.iter().chain(&right).map(|s| s.content.width()).sum();
        if needed + 1 > width {
            right.clear();
        }
        let mut lines = vec![justify(left, right, width)];
        for (agent, tokens) in subagent_lines(app, session, depth) {
            lines.push(justify(agent, tokens, width));
        }
        return lines;
    }

    // The action, after a space, in what the age leaves.
    let age_width = age.as_ref().map_or(0, |age| age.width());
    let free = width.saturating_sub(width_of(&left) + gauges + age_width + 1);
    let doing = truncate_spans(action(app, session, status), free);
    // Not when nothing but its `…` would be left.
    if width_of(&doing) > 1 {
        left.push(Span::raw(" "));
        left.extend(doing);
    }
    if let Some(age) = age {
        left.push(Span::styled(age, t.muted()));
    }
    let mut lines = vec![justify(left, right, width)];

    // Below: running sub-agents, right under their session.
    for (agent, tokens) in subagent_lines(app, session, depth) {
        lines.push(justify(agent, tokens, width));
    }
    lines
}

/// ` 42%`: the share of the context window in use, blank when the window
/// size is unknown.
fn context_spans(app: &App, session: &Session, faded: bool) -> Vec<Span<'static>> {
    let t = app.theme;
    let Some(percent) = session.context_percent() else {
        return vec![Span::raw(" ".repeat(CONTEXT_WIDTH))];
    };
    let style = if faded {
        t.muted()
    } else {
        t.level(percent).add_modifier(Modifier::BOLD)
    };
    vec![Span::styled(
        format!("{percent:>w$.0}%", w = CONTEXT_WIDTH - 1),
        style,
    )]
}
