//! petsitter — a terminal dashboard for the AI coding agents running under a
//! directory.

mod actions;
mod app;
mod background;
mod collectors;
mod demo;
mod git;
mod hooks;
mod model;
mod monitor;
mod procs;
mod pulse;
mod statusline;
mod tail;
mod tree;
mod ui;
mod view;

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre::{Context as _, Result, eyre};

use crate::model::Activity;
use crate::monitor::Settings;
use crate::view::{Section, View, repo_of, section};

/// Keep an eye on every AI coding agent session running under a directory.
///
/// Sessions of Claude Code, Codex and opencode whose working directory is ROOT
/// or one of its descendants are shown live, in a tree of their folders.
#[derive(Debug, Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Directory to monitor, usually the parent of your repositories [default: current directory].
    root: Option<PathBuf>,

    /// Mark live sessions as quiet after this long without activity.
    #[arg(long, value_name = "DURATION", default_value = "10m", value_parser = parse_duration)]
    idle: Duration,

    /// Keep exited sessions listed for this long.
    #[arg(long, value_name = "DURATION", default_value = "12h", value_parser = parse_duration)]
    exited: Duration,

    /// Maximum delay between refreshes when no file change is detected.
    #[arg(long, value_name = "DURATION", default_value = "2s", value_parser = parse_duration)]
    interval: Duration,

    /// Screen layout.
    #[arg(long, value_enum, default_value_t = app::ViewMode::Tree)]
    view: app::ViewMode,

    /// Print the sessions once as plain text and exit.
    #[arg(long)]
    once: bool,

    /// Show made-up sessions instead of the real ones.
    #[arg(long, hide = true)]
    demo: bool,

    /// Print one frame of the demo, `COLSxROWS`, as ANSI text, and exit.
    #[arg(long, hide = true, value_name = "COLSxROWS", value_parser = parse_size)]
    demo_frame: Option<(u16, u16)>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage optional agent hooks that report approval prompts.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Check terminal color detection and the optional integrations.
    Doctor,
    /// Entry point invoked by an agent hook (internal).
    #[command(hide = true)]
    Hook { agent: HookAgent },
    /// Claude Code status line relay (internal).
    #[command(hide = true)]
    Statusline {
        /// Status line command to run with the same input.
        #[arg(long)]
        then: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum HooksAction {
    /// Add petsitter's hook to the agent's configuration.
    Install { agent: HookAgent },
    /// Remove petsitter's hook from the agent's configuration.
    Uninstall { agent: HookAgent },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum HookAgent {
    /// Wrap Claude Code's status line to record fresh account usage.
    Claude,
    /// Report Codex approval prompts.
    Codex,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Hook {
            agent: HookAgent::Codex,
        }) => {
            // A hook must never get in Codex's way: whatever happens, it
            // exits quietly with success.
            let _ = hooks::run_codex_hook();
            return Ok(());
        }
        Some(Command::Hook {
            agent: HookAgent::Claude,
        }) => {
            return Err(eyre!("Claude Code uses `petsitter statusline`, not a hook"));
        }
        Some(Command::Statusline { then }) => {
            std::process::exit(statusline::run(then.as_deref())?);
        }
        Some(Command::Doctor) => {
            doctor();
            return Ok(());
        }
        Some(Command::Hooks { action }) => {
            match action {
                HooksAction::Install {
                    agent: HookAgent::Codex,
                } => {
                    let path = hooks::install_codex()?;
                    println!("Installed the petsitter hook in {}", path.display());
                    println!("Restart running Codex sessions to pick it up.");
                }
                HooksAction::Install {
                    agent: HookAgent::Claude,
                } => {
                    let path = statusline::install()?;
                    println!("Wrapped the Claude Code status line in {}", path.display());
                    println!(
                        "Your status line looks the same; petsitter now records account usage."
                    );
                }
                HooksAction::Uninstall {
                    agent: HookAgent::Codex,
                } => {
                    let path = hooks::uninstall_codex()?;
                    println!("Removed the petsitter hook from {}", path.display());
                }
                HooksAction::Uninstall {
                    agent: HookAgent::Claude,
                } => {
                    let path = statusline::uninstall()?;
                    println!("Restored the Claude Code status line in {}", path.display());
                }
            }
            return Ok(());
        }
        None => {}
    }

    if let Some((width, height)) = cli.demo_frame {
        print!("{}", demo::frame(width, height)?);
        return Ok(());
    }
    if cli.demo {
        // The demo snapshot once; the sender stays alive so the app runs on.
        let (sender, snapshots) = std::sync::mpsc::channel();
        sender.send(demo::snapshot(std::time::SystemTime::now()))?;
        let app = demo::app();
        let result = ratatui::run(|terminal| {
            let _mouse = MouseCapture::enable()?;
            app.run(terminal, &snapshots)
        });
        drop(sender);
        return result;
    }

    let root = match cli.root {
        Some(root) => root,
        None => std::env::current_dir().wrap_err("cannot read the current directory")?,
    };
    let root = root
        .canonicalize()
        .wrap_err_with(|| format!("cannot open {}", root.display()))?;
    if !root.is_dir() {
        return Err(eyre!("{} is not a directory", root.display()));
    }
    let settings = Settings {
        root: root.clone(),
        exited_window: cli.exited,
        // Below this, refreshing would only burn CPU.
        interval: cli.interval.max(Duration::from_millis(250)),
    };

    if cli.once {
        print_once(&settings, cli.idle);
        return Ok(());
    }

    let snapshots = monitor::spawn(settings);
    let app = app::App::new(root, cli.idle, ui::Theme::detect(), cli.view);
    ratatui::run(|terminal| {
        let _mouse = MouseCapture::enable()?;
        app.run(terminal, &snapshots)
    })
}

/// Prints what petsitter can see of the terminal and the integrations.
fn doctor() {
    let hex = |c: &terminal_colorsaurus::Color| {
        let (r, g, b) = c.scale_to_8bit();
        format!("#{r:02x}{g:02x}{b:02x}")
    };
    match ui::query_palette() {
        Ok(p) => println!(
            "terminal colors   ok: foreground {}, background {} ({:?})",
            hex(&p.foreground),
            hex(&p.background),
            p.theme_mode()
        ),
        Err(e) => {
            println!("terminal colors   unavailable ({e}); panels are drawn without surfaces");
        }
    }

    let relay = statusline::usage_file();
    match std::fs::metadata(&relay).and_then(|m| m.modified()) {
        Ok(at) => println!(
            "claude usage      relay record updated {}s ago ({})",
            SystemTime::now()
                .duration_since(at)
                .unwrap_or_default()
                .as_secs(),
            relay.display()
        ),
        Err(_) => {
            println!("claude usage      no relay record; run `petsitter hooks install claude`");
        }
    }

    let codex = hooks::codex_status_dir();
    if codex.is_dir() {
        println!("codex approvals   hook status files in {}", codex.display());
    } else {
        println!("codex approvals   no hook data; run `petsitter hooks install codex`");
    }
}

/// Reports mouse events while alive; restores normal mouse behavior on drop,
/// including when unwinding from a panic.
struct MouseCapture;

impl MouseCapture {
    fn enable() -> std::io::Result<Self> {
        ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::EnableMouseCapture
        )?;
        Ok(Self)
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::DisableMouseCapture
        );
    }
}

/// Plain-text output for scripts and debugging.
fn print_once(settings: &Settings, idle_after: Duration) {
    let snapshot = monitor::collect_once(settings);
    let now = SystemTime::now();
    let sessions = &snapshot.sessions;
    let view = View::build(sessions, &settings.root, now, idle_after);
    for index in view.order() {
        let s = &sessions[index];
        let place = match section(s, now, idle_after) {
            Section::Active => "active",
            Section::Inactive => "inactive",
            Section::Exited => "exited",
        };
        let state = match &s.activity {
            Activity::Working => "working".to_owned(),
            Activity::Waiting { reason } => format!("waiting: {reason}"),
            Activity::Idle => "idle".to_owned(),
            Activity::Exited => "exited".to_owned(),
        };
        let tool = s
            .current_tool
            .as_ref()
            .filter(|_| s.activity == Activity::Working)
            .map(|t| format!("  [{} {}]", t.name, t.detail))
            .unwrap_or_default();
        let subagents = if s.subagents.is_empty() {
            String::new()
        } else {
            format!("  (+{} sub-agents)", s.subagents.len())
        };
        // Names, prompts and commands come from agents' files: no control
        // characters reach the terminal.
        let line = format!(
            "{place:<9} {:<20} {:<9} {:<24} {:<10} {state}{tool}{subagents}",
            repo_of(&settings.root, &s.cwd).0,
            s.agent.label(),
            s.name,
            model::short_id(&s.id),
        );
        println!("{}", line.replace(char::is_control, " "));
    }
}

/// Parses a terminal size such as `140x46`.
fn parse_size(value: &str) -> Result<(u16, u16), String> {
    let (cols, rows) = value
        .split_once('x')
        .ok_or_else(|| format!("expected COLSxROWS, got {value:?}"))?;
    let parse = |n: &str| n.parse::<u16>().map_err(|e| format!("{n:?}: {e}"));
    Ok((parse(cols)?, parse(rows)?))
}

/// Parses durations such as `90s`, `10m`, `12h` or `1d`.
fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration `{value}`"))?;
    let seconds = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        other => return Err(format!("unknown unit `{other}` (use s, m, h or d)")),
    };
    Ok(Duration::from_secs(number.saturating_mul(seconds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_duration("12h"), Ok(Duration::from_hours(12)));
        assert!(parse_duration("ten").is_err());
        assert!(parse_duration("5w").is_err());
    }

    #[test]
    fn cli_is_well_formed() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }
}
