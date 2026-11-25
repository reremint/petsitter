//! Pieces shared by both layouts: header, footer, activity trace.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use super::{format, justify};
use crate::app::{App, ViewMode};
use crate::model::{Activity, Limit, Session};
use crate::pulse::braille;
use crate::view::{self, Counts};

/// Usage older than this is shown with its age.
const STALE_USAGE: Duration = Duration::from_mins(30);

/// `2h old` when a quota number is getting old.
pub(super) fn limit_age(app: &App, limit: &Limit) -> Option<String> {
    let updated = limit.updated_at?;
    app.now
        .duration_since(updated)
        .is_ok_and(|age| age > STALE_USAGE)
        .then(|| format!("{} old", format::since(app.now, updated)))
}

pub(super) fn render_header(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let t = app.theme;
    let counts = Counts::of(&app.snapshot.sessions, &app.view);
    let left = vec![
        Span::styled("petsitter", t.bold()),
        Span::styled(format!("  {}", format::home_relative(&app.root)), t.muted()),
    ];

    let mut right = Vec::new();
    let mut chip = |n: usize, text: String, style| {
        if n > 0 {
            right.push(Span::styled(text, style));
            right.push(Span::raw("   "));
        }
    };
    chip(
        counts.waiting,
        format!("⚑ {} blocked", counts.waiting),
        t.waiting(),
    );
    chip(
        counts.working,
        format!("{} working", counts.working),
        t.working(),
    );
    chip(counts.ready, format!("✓ {} ready", counts.ready), t.ready());
    right.push(Span::styled(
        jiff::ZonedDateTime::now().strftime("%H:%M").to_string(),
        t.muted(),
    ));
    frame.render_widget(
        Paragraph::new(justify(left, right, usize::from(area.width))),
        area,
    );
}

pub(super) fn render_footer(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let t = app.theme;
    let width = usize::from(area.width);

    let left = if let Some((message, _)) = &app.message {
        vec![Span::styled(message.clone(), t.waiting())]
    } else if !app.view.exited.is_empty() {
        let exited = app.view.exited.len();
        let text = if width >= 70 {
            format!("✕ {exited} exited")
        } else {
            format!("✕ {exited}")
        };
        let shown = match app.mode {
            ViewMode::Flat => app.show_exited,
            ViewMode::Tree => !app.hide_exited,
        };
        let action = if shown { "  h hide" } else { "  h show" };
        vec![
            Span::styled(text, t.muted()),
            Span::styled(action, t.muted()),
        ]
    } else {
        Vec::new()
    };

    let mut right = Vec::new();
    let mut hints: Vec<(&str, &str)> = Vec::new();
    if width >= 100 {
        hints.push(match app.mode {
            ViewMode::Flat => ("↑↓", "select"),
            ViewMode::Tree => ("←→", "fold"),
        });
    }
    if !app.selected_session().is_some_and(Session::detached) {
        hints.push(("⏎", "jump"));
    }
    if width >= 70 {
        hints.push(("y", "resume"));
        if app
            .selected_session()
            .is_some_and(|s| s.activity == Activity::Exited)
        {
            hints.push(("^x", "delete"));
        }
        if width >= 100 {
            hints.push(("d", "details"));
        }
    }
    hints.push(("q", "quit"));
    for (key, label) in hints {
        right.push(Span::styled(key, t.bold()));
        right.push(Span::styled(format!(" {label}  "), t.muted()));
    }
    frame.render_widget(Paragraph::new(justify(left, right, width)), area);
}

/// The last ten minutes of activity as a braille trace, `buckets / 2` wide.
pub(super) fn pulse_spans(
    app: &App,
    session: &Session,
    buckets: usize,
    history: bool,
) -> Vec<Span<'static>> {
    let t = app.theme;
    let buckets = app
        .pulses
        .get(&view::key(session))
        .map_or_else(|| vec![0.0; buckets], |p| p.buckets(app.now, buckets));
    let active = match session.activity {
        _ if history => t.muted(),
        Activity::Working => t.working(),
        Activity::Waiting { .. } => t.waiting(),
        Activity::Idle | Activity::Exited => t.muted(),
    };
    braille(&buckets)
        .into_iter()
        .map(|(c, busy)| Span::styled(c.to_string(), if busy { active } else { t.track() }))
        .collect()
}

/// What the session is doing, as spans.
pub(super) fn activity_spans(app: &App, session: &Session, history: bool) -> Vec<Span<'static>> {
    let t = app.theme;
    let prompt = || {
        session
            .last_prompt
            .clone()
            .map(|p| Span::styled(p, t.faint_italic()))
    };
    if history {
        return prompt().into_iter().collect();
    }
    match &session.activity {
        Activity::Waiting { reason } => vec![Span::styled(reason.clone(), t.waiting())],
        Activity::Working => match &session.current_tool {
            Some(tool) => vec![
                Span::styled(tool.name.clone(), t.tool()),
                Span::raw(" "),
                Span::raw(tool.detail.clone()),
            ],
            None => vec![Span::styled("thinking…", t.faint_italic())],
        },
        Activity::Idle | Activity::Exited => {
            prompt().map_or_else(|| vec![Span::styled("ready", t.muted())], |p| vec![p])
        }
    }
}
