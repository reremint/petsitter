//! Rendering. Two layouts are available: [`dashboard`] (the default, a tree
//! of folders) and [`flat`] (a plain list), chosen with `--view`.

mod dashboard;
mod dock;
mod flat;
mod format;
mod graph;
mod theme;
mod widgets;

pub use theme::{State, Theme, query_palette};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr as _;

use crate::app::{App, ViewMode};
use crate::model::{Activity, Agent};

/// Cells of every status mark.
pub(super) const MARK_WIDTH: usize = 1;

/// Time the gap stays on each corner: a full turn every half second.
const SPINNER_STEP_MS: u128 = 125;
/// A tight square of four braille dots with one missing, the gap going
/// round clockwise: top left, top right, bottom right, bottom left. The
/// square sits on the cell's two middle dot rows (the lowest two put it
/// clearly below the provider's glyph next to it).
const SPINNER: [&str; 4] = ["⠴", "⠦", "⠖", "⠲"];

/// The working indicator: the turning square of dots (the shimmer over the
/// session's name goes with it).
pub(super) fn working_mark(app: &App) -> Span<'static> {
    #[allow(clippy::cast_possible_truncation)] // wraps around anyway
    let step = (app.elapsed.as_millis() / SPINNER_STEP_MS) as usize % SPINNER.len();
    Span::styled(SPINNER[step], app.theme.working())
}

/// Time for the highlight to cross a name.
const SHIMMER_PERIOD_MS: u128 = 2400;
/// Half-width of the highlight, in characters.
const SHIMMER_BAND: f32 = 3.0;

/// `text` with a soft highlight sweeping across it from left to right, as
/// coding agents show their working state (Codex, Claude Code): each
/// character's brightness falls off along a cosine around the crest.
#[allow(clippy::cast_precision_loss)]
pub(super) fn shimmer(app: &App, text: &str, selected: bool) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    // The crest starts and ends off the text, so it fades in and out.
    let travel = chars.len() as f32 + 2.0 * SHIMMER_BAND;
    let progress = (app.elapsed.as_millis() % SHIMMER_PERIOD_MS) as f32 / SHIMMER_PERIOD_MS as f32;
    let crest = progress * travel - SHIMMER_BAND;
    chars
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let distance = (i as f32 - crest).abs();
            let intensity = if distance < SHIMMER_BAND {
                f32::midpoint(1.0, (std::f32::consts::PI * distance / SHIMMER_BAND).cos())
            } else {
                0.0
            };
            Span::styled(c.to_string(), app.theme.shimmer(intensity, selected))
        })
        .collect()
}

/// The background commands a session keeps running, as Claude Code words
/// it: `3 shells`, `1 shell · 2 monitors`; `None` when there are none.
pub(super) fn background_tasks(session: &crate::model::Session) -> Option<String> {
    let count = |n: usize, what: &str| match n {
        0 => None,
        1 => Some(format!("1 {what}")),
        n => Some(format!("{n} {what}s")),
    };
    let parts: Vec<String> = [
        count(session.shells(), "shell"),
        count(session.monitors(), "monitor"),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What sets a sub-agent's model apart from its session's: its model
/// and/or effort when they differ (`fable-5-2`, `low`, `fable-5-2 · low`).
pub(super) fn subagent_model(
    session: &crate::model::Session,
    agent: &crate::model::SubAgent,
) -> Option<String> {
    let model = agent
        .model
        .as_ref()
        .filter(|m| session.model.as_ref() != Some(*m))
        .map(|m| format::model(session.agent, m));
    let effort = agent
        .effort
        .clone()
        .filter(|e| session.effort.as_ref() != Some(e));
    let parts: Vec<String> = model.into_iter().chain(effort).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A one-cell status symbol in the indicator's width, against its right
/// edge so every mark sits one space from the provider's glyph.
pub(super) fn mark(symbol: &str, style: ratatui::style::Style) -> Span<'static> {
    Span::styled(format!("{symbol:>MARK_WIDTH$}"), style)
}

pub fn render(app: &App, frame: &mut Frame<'_>) {
    match app.mode {
        ViewMode::Flat => flat::render(app, frame),
        ViewMode::Tree => dashboard::render(app, frame),
    }
}

/// A horizontal gauge: `▰▰▰▱▱▱▱▱▱▱`, filled in the color of its level.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub(super) fn gauge(t: Theme, percent: f64, cells: usize) -> Vec<Span<'static>> {
    let percent = percent.clamp(0.0, 100.0);
    let mut filled = ((percent / 100.0) * cells as f64).round() as usize;
    if percent > 0.0 {
        filled = filled.max(1);
    }
    let filled = filled.min(cells);
    vec![
        Span::styled("▰".repeat(filled), t.level(percent)),
        Span::styled("▱".repeat(cells - filled), t.track()),
    ]
}

/// Paints a line on a background: pads it to `width` and puts `surface`
/// under every span, so the fill covers the whole row.
pub(super) fn paint(line: Line<'static>, surface: Style, width: usize) -> Line<'static> {
    let used = line.width();
    let mut spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| {
            let style = surface.patch(span.style);
            span.style(style)
        })
        .collect();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), surface));
    }
    Line::from(spans)
}

/// A one-character mark identifying the agent.
pub(super) fn agent_glyph(app: &App, agent: Agent) -> Span<'static> {
    let glyph = match agent {
        Agent::Claude => "✻",
        Agent::Codex => "◈",
        Agent::OpenCode => "▣",
    };
    Span::styled(glyph, app.theme.agent(agent))
}

pub(super) fn render_empty(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let t = app.theme;
    let (title, hint) = if app.loaded {
        (
            format!(
                "No agent sessions under {}",
                format::home_relative(&app.root)
            ),
            "Watching Claude Code, Codex and opencode sessions",
        )
    } else {
        ("Looking for agents…".to_owned(), "")
    };
    let text = vec![Line::styled(title, t.bold()), Line::styled(hint, t.muted())];
    let [middle] = Layout::vertical([Constraint::Length(2)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), middle);
}

pub(super) fn status_icon(app: &App, activity: &Activity) -> Span<'static> {
    let t = app.theme;
    match activity {
        Activity::Working => working_mark(app),
        Activity::Waiting { .. } => mark("⚑", t.waiting()),
        Activity::Idle => mark("✓", t.ready()),
        Activity::Exited => mark("✕", t.muted()),
    }
}

/// Places `right` against the right edge, truncating `left` if space runs out.
pub(super) fn justify(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let right_width: usize = right.iter().map(|s| s.content.width()).sum();
    let budget = width.saturating_sub(right_width + 1);
    let mut spans = Vec::with_capacity(left.len() + right.len() + 1);
    let mut used = 0;
    for span in left {
        let w = span.content.width();
        if used + w <= budget {
            used += w;
            spans.push(span);
        } else {
            let room = budget.saturating_sub(used);
            if room > 0 {
                spans.push(Span::styled(
                    format::truncate(&span.content, room),
                    span.style,
                ));
                used += room;
            }
            break;
        }
    }
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(used + right_width)),
    ));
    spans.extend(right);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width(spans: &[Span<'_>]) -> usize {
        spans.iter().map(|s| s.content.width()).sum()
    }

    #[test]
    fn the_spinner_keeps_its_width() {
        assert!(SPINNER.iter().all(|f| f.chars().count() == MARK_WIDTH));
    }

    #[test]
    fn gauges_keep_their_width() {
        let t = Theme::plain();
        for percent in [0.0, 0.4, 12.5, 50.0, 100.0, 140.0] {
            assert_eq!(width(&gauge(t, percent, 10)), 10, "{percent}%");
        }
        assert_eq!(
            gauge(t, 0.4, 10)[0].content,
            "▰",
            "tiny usage stays visible"
        );
        assert_eq!(gauge(t, 50.0, 10)[0].content, "▰▰▰▰▰");
    }

    /// A working session with a sub-agent, on a busy 11-core machine.
    fn sample_snapshot() -> crate::monitor::Snapshot {
        use std::path::PathBuf;
        use std::time::SystemTime;

        use crate::model::{Agent, Session, ToolCall};
        use crate::monitor::{Snapshot, SystemLoad};
        use crate::procs::Machine;

        let now = SystemTime::now();
        let mut session = Session::new(Agent::Claude, "s1", PathBuf::from("/lab/app"), now);
        session.name = "fix-parser".into();
        session.activity = crate::model::Activity::Working;
        session.pid = Some(42);
        session.cpu = Some(120.0);
        session.memory = Some(800 << 20);
        session.context_tokens = Some(420_000);
        session.context_window = Some(1_000_000);
        session.current_tool = Some(ToolCall {
            name: "Bash".into(),
            detail: "cargo test".into(),
            started_at: Some(now),
        });
        session.subagents = vec![crate::model::SubAgent {
            description: "Review the parser".into(),
            kind: Some("Explore".into()),
            current_tool: Some(ToolCall {
                name: "Read".into(),
                detail: "src/parser.rs".into(),
                started_at: Some(now),
            }),
            activity: Some("Reading parser.rs".into()),
            tokens_used: Some(48_000),
            model: Some("claude-haiku-5-5".into()),
            effort: None,
            last_active: now,
            level: 0,
        }];
        Snapshot {
            sessions: vec![session],
            system: SystemLoad {
                machine: Machine {
                    cpu: 35.0,
                    cores: 11,
                    per_core: vec![30.0; 11],
                    memory_used: 25 << 30,
                    memory_total: 36 << 30,
                    memory_available: 11 << 30,
                    memory_free: 1 << 30,
                    swap_used: 1 << 30,
                    swap_total: 4 << 30,
                    load: Some((3.1, 2.9, 2.5)),
                    uptime: Some(86_400 * 3),
                    brand: Some("Test CPU".into()),
                },
                agents_cpu: 120.0,
                agents_memory: 800 << 20,
            },
            ..Snapshot::default()
        }
    }

    /// The app with `snapshot` loaded and its first session selected, so the
    /// details panel shows.
    fn app_on(snapshot: crate::monitor::Snapshot, mode: crate::app::ViewMode) -> crate::app::App {
        let mut app = crate::app::App::new(
            std::path::PathBuf::from("/lab"),
            std::time::Duration::from_secs(600),
            Theme::plain(),
            mode,
        );
        app.load_snapshot(snapshot);
        app.cursor = Some(crate::tree::RowKey::Session(crate::view::key(
            &app.snapshot.sessions[0],
        )));
        app
    }

    /// The screen as text, one line per row.
    fn draw(app: &crate::app::App, width: u16, height: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(app, frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
                    + "\n"
            })
            .collect()
    }

    /// Renders both layouts on a wide and a narrow screen with synthetic data:
    /// nothing may panic, and the System section must show its three boxes.
    /// Run with `--nocapture` to look at the result.
    #[test]
    fn renders_both_layouts_at_several_sizes() {
        use crate::app::ViewMode;

        let snapshot = sample_snapshot();

        for mode in [ViewMode::Tree, ViewMode::Flat] {
            for (width, height) in [(180, 60), (70, 40)] {
                let app = app_on(snapshot.clone(), mode);
                let screen = draw(&app, width, height);
                println!("--- {mode:?} {width}x{height}\n{screen}");
                assert!(screen.contains("fix-parser"));
                if mode == ViewMode::Tree {
                    assert!(
                        screen.contains("└─ Explore (haiku-5-5) - Reading parser.rs"),
                        "sub-agent listed"
                    );
                }
                if height >= 60 {
                    assert!(screen.contains("Agents (1)"), "sub-agents in the details");
                }
                if mode == ViewMode::Tree && width >= 120 {
                    assert!(screen.contains("available"), "memory shown");
                    assert!(screen.contains("C10"), "every core listed");
                }
            }
        }
    }

    /// A sub-agent started by another one is indented under it, in the tree
    /// and in the details panel.
    #[test]
    fn nests_subagents_under_their_parent() {
        let mut snapshot = sample_snapshot();
        let parent = snapshot.sessions[0].subagents[0].clone();
        let child = |kind: &str, level| crate::model::SubAgent {
            kind: Some(kind.into()),
            model: None,
            level,
            ..parent.clone()
        };
        snapshot.sessions[0]
            .subagents
            .extend([child("fork", 1), child("claude", 2)]);
        let app = app_on(snapshot, crate::app::ViewMode::Tree);
        let screen = draw(&app, 180, 80);
        println!("{screen}");
        // The column where `text` starts, on the line holding it.
        let column = |text: &str| {
            screen
                .lines()
                .find_map(|line| Some(line[..line.find(text)?].width()))
                .unwrap_or_else(|| panic!("{text:?} not on screen"))
        };

        let tree = column("└─ Explore");
        assert_eq!(column("└─ fork"), tree + 3);
        assert_eq!(column("└─ claude"), tree + 6);

        let details = column("● Explore");
        assert_eq!(column("└ ● fork"), details);
        assert_eq!(column("└ ● claude"), details + 2);
    }

    #[test]
    fn paint_fills_the_whole_row() {
        let line = paint(Line::from("abc"), Style::new(), 8);
        assert_eq!(line.width(), 8);
    }
}
