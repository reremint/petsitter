//! A made-up folder of agent sessions, to try the interface without any
//! agent running, and to make screenshots that show nobody's files:
//! `petsitter --demo` runs it live, `petsitter --demo-frame 140x46` prints
//! one frame as ANSI text.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

use crate::app::{App, ViewMode};
use crate::model::{Activity, Agent, BackgroundTask, Limit, Session, SubAgent, ToolCall, Usage};
use crate::monitor::{Snapshot, SystemLoad};
use crate::procs::Machine;
use crate::pulse::Pulse;
use crate::ui::Theme;

/// The folder the demo pretends to watch: `~/code`.
fn root() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join("code")
}
/// Catppuccin Mocha's text and background, the demo's terminal colors.
const PALETTE: ((u8, u8, u8), (u8, u8, u8)) = ((0xCD, 0xD6, 0xF4), (0x1E, 0x1E, 0x2E));

/// The app, loaded with the demo sessions and ten minutes of history, the
/// session `release-notes` selected.
pub fn app() -> App {
    let now = SystemTime::now();
    let theme = Theme::with_palette(PALETTE.0, PALETTE.1);
    let mut app = App::new(root(), Duration::from_mins(10), theme, ViewMode::Tree);
    let snapshot = snapshot(now);

    // Ten minutes of activity, sampled every two seconds.
    for (n, session) in snapshot.sessions.iter().enumerate() {
        let mut pulse = Pulse::default();
        let mut past = session.clone();
        for step in 0..300_u32 {
            let at = now - Duration::from_secs(600) + Duration::from_secs(u64::from(step) * 2);
            let wave = wave(step, n);
            if wave > 0.35 && session.activity != Activity::Idle {
                past.last_active = at;
            }
            past.cpu = session.cpu.map(|cpu| cpu * wave);
            pulse.observe(&past, at);
        }
        app.pulses.insert(crate::view::key(session), pulse);
    }
    for step in 0..120_u32 {
        let load = 25.0 + 30.0 * wave(step, 7);
        app.cpu_history.push_back(load);
    }
    app.core_history = (0..11)
        .map(|core| {
            (0..120_u32)
                .map(|step| 10.0 + 70.0 * wave(step, core))
                .collect()
        })
        .collect();
    app.load_snapshot(snapshot);
    app.cursor = app
        .snapshot
        .sessions
        .iter()
        .find(|s| s.name == "release-notes")
        .map(|s| crate::tree::RowKey::Session(crate::view::key(s)));
    // Mid-way through the shimmer, so a name catches the light.
    app.elapsed = Duration::from_millis(900);
    app
}

/// One frame of the demo, `width` by `height`, as text with ANSI colors.
pub fn frame(width: u16, height: u16) -> color_eyre::Result<String> {
    let app = app();
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| crate::ui::render(&app, frame))?;
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        let mut last = None;
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let style = (cell.fg, cell.bg, cell.modifier);
            if last != Some(style) {
                out.push_str(&sgr(cell.fg, cell.bg, cell.modifier));
                last = Some(style);
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\x1b[0m\n");
    }
    Ok(out)
}

/// The escape sequence that sets a cell's colors and attributes.
fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
    let mut codes = vec!["0".to_owned()];
    if modifier.contains(Modifier::BOLD) {
        codes.push("1".to_owned());
    }
    if modifier.contains(Modifier::DIM) {
        codes.push("2".to_owned());
    }
    if modifier.contains(Modifier::ITALIC) {
        codes.push("3".to_owned());
    }
    if modifier.contains(Modifier::REVERSED) {
        codes.push("7".to_owned());
    }
    if let Some(code) = color_code(fg, 30) {
        codes.push(code);
    }
    if let Some(code) = color_code(bg, 40) {
        codes.push(code);
    }
    let mut out = String::from("\x1b[");
    let _ = write!(out, "{}m", codes.join(";"));
    out
}

/// The SGR parameters of a color, `base` being 30 for text, 40 for
/// background.
fn color_code(color: Color, base: u8) -> Option<String> {
    let named = |n: u8| Some((base + n).to_string());
    let bright = |n: u8| Some((base + 60 + n).to_string());
    match color {
        Color::Reset => None,
        Color::Black => named(0),
        Color::Red => named(1),
        Color::Green => named(2),
        Color::Yellow => named(3),
        Color::Blue => named(4),
        Color::Magenta => named(5),
        Color::Cyan => named(6),
        Color::Gray => named(7),
        Color::DarkGray => bright(0),
        Color::LightRed => bright(1),
        Color::LightGreen => bright(2),
        Color::LightYellow => bright(3),
        Color::LightBlue => bright(4),
        Color::LightMagenta => bright(5),
        Color::LightCyan => bright(6),
        Color::White => bright(7),
        Color::Indexed(n) => Some(format!("{};5;{n}", base + 8)),
        Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", base + 8)),
    }
}

/// A smooth, made-up activity level from 0 to 1, different for each `seed`.
#[allow(clippy::cast_precision_loss)]
fn wave(step: u32, seed: usize) -> f32 {
    let t = step as f32 / 9.0 + seed as f32 * 1.7;
    (0.5 + 0.3 * t.sin() + 0.2 * (t * 2.3).cos()).clamp(0.0, 1.0)
}

/// The demo sessions, quotas and machine.
#[allow(clippy::too_many_lines)] // one made-up session after another
pub fn snapshot(now: SystemTime) -> Snapshot {
    let ago = |secs: u64| now - Duration::from_secs(secs);
    let in_ = |secs: u64| now + Duration::from_secs(secs);
    let tool = |name: &str, detail: &str, secs: u64| ToolCall {
        name: name.into(),
        detail: detail.into(),
        started_at: Some(ago(secs)),
    };
    let session = |agent, id: &str, name: &str, folder: &str, branch: &str| {
        let mut s = Session::new(agent, id, root().join(folder), now);
        s.name = name.into();
        s.git_branch = Some(branch.into());
        s.started_at = Some(ago(3 * 3600));
        s.pid = Some(4000 + u32::try_from(id.len()).unwrap_or(0));
        s
    };

    let mut parser = session(
        Agent::Claude,
        "0f8c1d2e-4a5b-4c6d-8e9f-0a1b2c3d4e5f",
        "fix-parser",
        "api-server",
        "feat/parser",
    );
    parser.activity = Activity::Waiting {
        reason: "Bash cargo publish --dry-run".into(),
    };
    parser.model = Some("claude-opus-5-5".into());
    parser.effort = Some("high".into());
    parser.context_tokens = Some(412_000);
    parser.context_window = Some(1_000_000);
    parser.tokens_used = Some(1_240_000);
    parser.cpu = Some(4.0);
    parser.memory = Some(620 << 20);
    parser.last_active = ago(12);
    parser.last_prompt = Some("add a regression test for the parser".into());
    parser.last_prompt_at = Some(ago(25 * 60));

    let mut review = session(
        Agent::Codex,
        "019a4b2c-7d8e-7f90-a1b2-c3d4e5f60718",
        "review",
        "api-server",
        "main",
    );
    review.activity = Activity::Working;
    review.model = Some("gpt-5.5".into());
    review.effort = Some("medium".into());
    review.context_tokens = Some(124_000);
    review.context_window = Some(400_000);
    review.tokens_used = Some(310_000);
    review.cpu = Some(96.0);
    review.memory = Some(410 << 20);
    review.last_active = ago(3);
    review.last_prompt = Some("review the open pull requests".into());
    review.last_prompt_at = Some(ago(40 * 60));
    review.current_tool = Some(tool("shell", "cargo test --workspace", 3));
    review.tasks = vec![
        BackgroundTask {
            monitor: false,
            command: "cargo test --workspace".into(),
            started_at: Some(ago(40)),
            cpu: Some(92.0),
        },
        BackgroundTask {
            monitor: false,
            command: "cargo watch -x clippy".into(),
            started_at: Some(ago(52 * 60)),
            cpu: Some(3.0),
        },
    ];

    let mut notes = session(
        Agent::Claude,
        "5e6f7a8b-9c0d-4e1f-a2b3-c4d5e6f7a8b9",
        "release-notes",
        "website",
        "docs",
    );
    notes.activity = Activity::Working;
    notes.model = Some("claude-sonnet-5-5".into());
    notes.effort = Some("medium".into());
    notes.context_tokens = Some(188_000);
    notes.context_window = Some(1_000_000);
    notes.tokens_used = Some(642_000);
    notes.cpu = Some(38.0);
    notes.memory = Some(540 << 20);
    notes.last_active = ago(1);
    notes.last_prompt = Some("write the release notes for 0.4".into());
    notes.last_prompt_at = Some(ago(9 * 60));
    notes.current_tool = Some(tool("Edit", "docs/changelog.md", 1));
    notes.recent_tools = vec![
        tool("Read", "CHANGELOG.md", 140),
        tool("Bash", "git log --oneline v0.3.0..HEAD", 95),
        tool("Grep", "BREAKING", 60),
        tool("Edit", "docs/changelog.md", 1),
    ];
    notes.tasks = vec![BackgroundTask {
        monitor: false,
        command: "npm run dev".into(),
        started_at: Some(ago(2 * 3600)),
        cpu: Some(2.0),
    }];
    notes.subagents = vec![
        SubAgent {
            description: "Collect the merged pull requests".into(),
            kind: Some("general-purpose".into()),
            current_tool: Some(tool("Bash", "gh pr list --state merged", 4)),
            activity: Some("Listing merged pull requests".into()),
            tokens_used: Some(48_000),
            model: Some("claude-haiku-5-5".into()),
            effort: None,
            last_active: ago(4),
            level: 0,
        },
        // Started by the agent above.
        SubAgent {
            description: "Sort the changes by area".into(),
            kind: Some("fork".into()),
            current_tool: Some(tool("Read", "CHANGELOG.md", 2)),
            activity: Some("Reading CHANGELOG.md".into()),
            tokens_used: Some(21_000),
            model: Some("claude-haiku-5-5".into()),
            effort: None,
            last_active: ago(2),
            level: 1,
        },
    ];

    let mut docs = session(
        Agent::OpenCode,
        "ses_4f2a9c1d8e7b",
        "docs",
        "website",
        "docs",
    );
    docs.activity = Activity::Idle;
    docs.model = Some("gpt-5.5".into());
    docs.context_tokens = Some(61_000);
    docs.context_window = Some(1_000_000);
    docs.tokens_used = Some(92_000);
    docs.cpu = Some(0.5);
    docs.memory = Some(180 << 20);
    docs.last_active = ago(4 * 60);
    docs.last_prompt = Some("rewrite the install section".into());
    docs.last_prompt_at = Some(ago(6 * 60));

    let mut plan = session(
        Agent::Claude,
        "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d",
        "terraform-plan",
        "infra",
        "main",
    );
    plan.activity = Activity::Idle;
    plan.model = Some("claude-opus-5-5".into());
    plan.effort = Some("high".into());
    plan.context_tokens = Some(583_000);
    plan.context_window = Some(1_000_000);
    plan.tokens_used = Some(2_310_000);
    plan.cpu = Some(0.0);
    plan.memory = Some(300 << 20);
    plan.last_active = ago(48 * 60);
    plan.last_prompt = Some("plan the database migration".into());
    plan.last_prompt_at = Some(ago(60 * 60));

    let limit = |label: &str, percent: f64, resets: u64| Limit {
        label: label.into(),
        percent,
        resets_at: Some(in_(resets)),
        updated_at: Some(now),
    };
    let sessions = vec![parser, review, notes, docs, plan];
    let agents_cpu = sessions.iter().filter_map(|s| s.cpu).sum();
    let agents_memory = sessions.iter().filter_map(|s| s.memory).sum();
    Snapshot {
        sessions,
        branches: [
            ("api-server", "main"),
            ("website", "docs"),
            ("infra", "main"),
        ]
        .into_iter()
        .map(|(folder, branch)| (folder.to_owned(), branch.to_owned()))
        .collect(),
        usage: vec![
            Usage {
                agent: Agent::Claude,
                plan: Some("Max".into()),
                limits: vec![
                    limit("5h", 23.0, 3 * 3600),
                    limit("week", 41.0, 3 * 86_400),
                    limit("Opus", 58.0, 3 * 86_400),
                ],
                updated_at: Some(now),
            },
            Usage {
                agent: Agent::Codex,
                plan: Some("plus".into()),
                limits: vec![limit("5h", 12.0, 2 * 3600), limit("week", 9.0, 5 * 86_400)],
                updated_at: Some(now),
            },
        ],
        system: SystemLoad {
            machine: Machine {
                cpu: 34.0,
                cores: 11,
                per_core: (0..11_u32)
                    .map(|core| 15.0 + 60.0 * wave(3, core as usize))
                    .collect(),
                memory_used: 24 << 30,
                memory_total: 36 << 30,
                memory_available: 12 << 30,
                memory_free: 2 << 30,
                swap_used: 1 << 30,
                swap_total: 4 << 30,
                load: Some((3.4, 3.1, 2.8)),
                uptime: Some(4 * 86_400 + 7200),
                brand: Some("Apple M3 Pro".into()),
            },
            agents_cpu,
            agents_memory,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_a_frame_with_every_kind_of_session() {
        let ansi = frame(150, 64).unwrap();
        // The text without its escape sequences.
        let mut frame = String::new();
        let mut chars = ansi.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|&c| c == 'm');
            } else {
                frame.push(c);
            }
        }
        for name in [
            "fix-parser",
            "review",
            "release-notes",
            "docs",
            "terraform-plan",
        ] {
            assert!(frame.contains(name), "{name} shown");
        }
        assert!(
            frame.contains("Background"),
            "details of the selected session"
        );
    }
}
