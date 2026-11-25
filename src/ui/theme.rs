//! Colors and text styles.
//!
//! Foreground colors are the 16 named ANSI colors, so petsitter follows the
//! terminal's color scheme instead of imposing its own. Filled shapes (pills,
//! gauges) use reverse video on those same colors. Panel surfaces need real
//! colors: they are derived from the terminal's own background, queried at
//! startup, and simply left out when the terminal does not answer.
//! `NO_COLOR` disables colors and keeps text attributes.

use ratatui::style::{Color, Modifier, Style};

use crate::model::Agent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    colors: bool,
    /// Whether the terminal has a light background (dark is assumed otherwise).
    light: bool,
    /// Panel backgrounds derived from the terminal's colors, when known.
    surfaces: Option<Surfaces>,
}

/// An RGB triple.
pub type Rgb = (u8, u8, u8);

/// Each provider's own color, for dark and light backgrounds.
///
/// - Claude: Anthropic's primary brand orange (`#d97757`, anthropics/skills
///   `brand-guidelines`), on both backgrounds.
/// - Codex: `OpenAI`'s brand is monochrome; this is the accent of the Codex TUI
///   (`codex-rs/tui/src/style.rs`: `#63A8F8`, `#1C64C8` on light).
/// - opencode: its brand is monochrome; this is the primary color of its
///   default TUI theme (`opencode.json`: `#FAB283`, `#3B7DD8` on light).
const BRAND_COLORS: [(Agent, Rgb, Rgb); 3] = [
    (Agent::Claude, (0xD9, 0x77, 0x57), (0xD9, 0x77, 0x57)),
    (Agent::Codex, (0x63, 0xA8, 0xF8), (0x1C, 0x64, 0xC8)),
    (Agent::OpenCode, (0xFA, 0xB2, 0x83), (0x3B, 0x7D, 0xD8)),
];

/// Background shades slightly offset from the terminal's own background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Surfaces {
    /// The panels' background (system, quotas, details).
    raised: Color,
    /// A row picked inside a panel: a neutral gray, set apart from the
    /// panel's background.
    highlight: Color,
    /// The terminal's own text and background colors.
    foreground: Rgb,
    background: Rgb,
}

impl Surfaces {
    /// Panels are darker than the terminal: on a dark background, a shade
    /// deeper (or, on a black one, a hint toward the text); on a light one,
    /// a few percent toward the text. The selected row leans a little more
    /// toward the text, in gray.
    fn from_palette(fg: Rgb, bg: Rgb) -> Self {
        let mix = |toward: Rgb, amount: f32| {
            let channel = |b: u8, f: u8| {
                let value = f32::from(b) + (f32::from(f) - f32::from(b)) * amount;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let value = value.round().clamp(0.0, 255.0) as u8;
                value
            };
            (
                channel(bg.0, toward.0),
                channel(bg.1, toward.1),
                channel(bg.2, toward.2),
            )
        };
        let raised = if is_light(bg) {
            mix(fg, 0.06)
        } else {
            let deeper = mix((0, 0, 0), 0.35);
            if deeper == bg { mix(fg, 0.04) } else { deeper }
        };
        let rgb = |(r, g, b): Rgb| Color::Rgb(r, g, b);
        Self {
            raised: rgb(raised),
            highlight: rgb(mix(fg, 0.10)),
            foreground: fg,
            background: bg,
        }
    }
}

/// Asks the terminal for its foreground and background colors.
///
/// Terminals answer through the same input stream as the keyboard, and a busy
/// terminal sometimes answers out of order, which reads as "unsupported"; the
/// query is retried a few times.
pub fn query_palette() -> Result<terminal_colorsaurus::ColorPalette, terminal_colorsaurus::Error> {
    let options = terminal_colorsaurus::QueryOptions::default();
    let mut result = terminal_colorsaurus::color_palette(options.clone());
    for _ in 0..2 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        result = terminal_colorsaurus::color_palette(options.clone());
    }
    result
}

/// Last colors the terminal reported, so a failed query still gets surfaces.
fn palette_cache() -> std::path::PathBuf {
    crate::hooks::state_dir().join("terminal-colors.json")
}

/// Queries the terminal, falling back to (and refreshing) the cached answer.
fn palette() -> Option<(Rgb, Rgb)> {
    let cache = palette_cache();
    if let Ok(palette) = query_palette() {
        let colors = (
            palette.foreground.scale_to_8bit(),
            palette.background.scale_to_8bit(),
        );
        if let Some(dir) = cache.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&cache, serde_json::json!([colors.0, colors.1]).to_string());
        return Some(colors);
    }
    let cached: (Rgb, Rgb) = serde_json::from_slice(&std::fs::read(cache).ok()?).ok()?;
    Some(cached)
}

/// Whether a background is light, by perceived luminance.
fn is_light(bg: Rgb) -> bool {
    let (r, g, b) = (f32::from(bg.0), f32::from(bg.1), f32::from(bg.2));
    0.2126 * r + 0.7152 * g + 0.0722 * b > 127.5
}

/// Orange for blocked sessions: Catppuccin's peach, Mocha on dark
/// backgrounds and Latte on light ones (the 16 ANSI colors have no orange).
const ORANGE: (Rgb, Rgb) = ((0xFA, 0xB3, 0x87), (0xFE, 0x64, 0x0B));

/// The selection's magenta as RGB, for dark and light backgrounds
/// (Catppuccin Mocha and Latte's terminal magenta), to fade it smoothly.
const SELECTION: (Rgb, Rgb) = ((0xF5, 0xC2, 0xE7), (0xEA, 0x76, 0xCB));

/// Session states, each with its accent color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting for the user (permission prompt, question): orange.
    Blocked,
    /// Generating or running tools: green.
    Working,
    /// Finished its turn recently: blue.
    Ready,
    /// Alive but idle for a while, or exited: gray.
    Faded,
}

// Every style is a method, even those colors do not affect, so call sites stay uniform.
#[allow(clippy::unused_self)]
impl Theme {
    /// Honors the `NO_COLOR` convention (<https://no-color.org>) and asks the
    /// terminal for its colors. Call before entering the alternate screen.
    pub fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        if no_color {
            return Self {
                colors: false,
                light: false,
                surfaces: None,
            };
        }
        let palette = palette();
        let light = palette.is_some_and(|(_, bg)| is_light(bg));
        let surfaces = palette.map(|(fg, bg)| Surfaces::from_palette(fg, bg));
        Self {
            colors: true,
            light,
            surfaces,
        }
    }

    /// Colors for a terminal whose text and background are known, without
    /// asking it (the demo's screenshots).
    pub fn with_palette(fg: Rgb, bg: Rgb) -> Self {
        Self {
            colors: true,
            light: is_light(bg),
            surfaces: Some(Surfaces::from_palette(fg, bg)),
        }
    }

    /// Plain ANSI colors, without surfaces.
    #[cfg(test)]
    pub fn plain() -> Self {
        Self {
            colors: true,
            light: false,
            surfaces: None,
        }
    }

    /// The background of the selected row of the list: the panels' color,
    /// the darkest of the screen (none without known colors).
    pub fn selected_row(self) -> Style {
        self.surface()
    }

    /// The background of a row picked inside a panel, which already has the
    /// panels' color: a gray (none without known colors).
    pub fn highlight(self) -> Style {
        self.surfaces
            .filter(|_| self.colors)
            .map_or_else(Style::new, |s| Style::new().bg(s.highlight))
    }

    /// Background of the panels; plain when surfaces are unknown.
    pub fn surface(self) -> Style {
        self.surfaces
            .map_or_else(Style::new, |s| Style::new().bg(s.raised))
    }

    fn fg(self, color: Color) -> Style {
        if self.colors {
            Style::new().fg(color)
        } else {
            Style::new()
        }
    }

    /// Bold text in `color`.
    pub fn fg_bold(self, color: Color) -> Style {
        self.fg(color).add_modifier(Modifier::BOLD)
    }

    /// Regular text, slightly quieter than the default (used for sub-agents).
    pub fn text_dim(self) -> Style {
        Style::new().add_modifier(Modifier::ITALIC)
    }

    /// A git branch name (cyan: magenta is the selection).
    pub fn branch(self) -> Style {
        self.fg(Color::Cyan)
    }

    pub fn bold(self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    /// Secondary information. `DIM` adapts to both light and dark themes.
    pub fn muted(self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    pub fn faint_italic(self) -> Style {
        self.muted().add_modifier(Modifier::ITALIC)
    }

    pub fn border(self) -> Style {
        self.fg(Color::DarkGray)
    }

    /// The accent color of a session state.
    pub fn state_color(self, state: State) -> Color {
        match state {
            State::Blocked => {
                let (r, g, b) = if self.light { ORANGE.1 } else { ORANGE.0 };
                Color::Rgb(r, g, b)
            }
            State::Working => Color::Green,
            State::Ready => Color::Blue,
            State::Faded => Color::DarkGray,
        }
    }

    /// Text under a passing highlight, `intensity` from 0 (resting) to 1
    /// (the crest): the text color (the selection's when `selected`) faded
    /// toward the background at rest, full at the crest. Without known
    /// colors, dim then bold.
    pub fn shimmer(self, intensity: f32, selected: bool) -> Style {
        let intensity = intensity.clamp(0.0, 1.0);
        let Some(surfaces) = self.surfaces.filter(|_| self.colors) else {
            let base = if selected {
                self.selection()
            } else {
                self.bold()
            };
            return match intensity {
                i if i > 0.5 => base,
                i if i > 0.15 => base.remove_modifier(Modifier::BOLD),
                _ => base
                    .remove_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::DIM),
            };
        };
        let bg = surfaces.background;
        let fg = match (selected, self.light) {
            (false, _) => surfaces.foreground,
            (true, false) => SELECTION.0,
            (true, true) => SELECTION.1,
        };
        let amount = 0.5 + 0.5 * intensity;
        let channel = |b: u8, f: u8| {
            let value = f32::from(b) + (f32::from(f) - f32::from(b)) * amount;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let value = value.round().clamp(0.0, 255.0) as u8;
            value
        };
        Style::new()
            .fg(Color::Rgb(
                channel(bg.0, fg.0),
                channel(bg.1, fg.1),
                channel(bg.2, fg.2),
            ))
            .add_modifier(Modifier::BOLD)
    }

    /// A label drawn as a colored pill: dark text on `color`.
    pub fn pill(self, color: Color) -> Style {
        if self.colors {
            Style::new()
                .bg(color)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        }
    }

    pub fn working(self) -> Style {
        self.fg(self.state_color(State::Working))
    }

    /// Blocked on the user.
    pub fn waiting(self) -> Style {
        self.fg(self.state_color(State::Blocked))
            .add_modifier(Modifier::BOLD)
    }

    pub fn ready(self) -> Style {
        self.fg(self.state_color(State::Ready))
    }

    /// Something removed or failing.
    pub fn error(self) -> Style {
        self.fg(Color::Red)
    }

    /// Empty part of a gauge or chart.
    pub fn track(self) -> Style {
        self.fg(Color::DarkGray)
    }

    /// Gauge fill, from comfortable to nearly exhausted.
    pub fn level(self, percent: f64) -> Style {
        match percent {
            p if p >= 90.0 => self.fg(Color::Red),
            p if p >= 70.0 => self.fg(Color::Yellow),
            _ => self.fg(Color::Green),
        }
    }

    pub fn tool(self) -> Style {
        self.fg(Color::Blue).add_modifier(Modifier::BOLD)
    }

    pub fn selection(self) -> Style {
        self.fg(Color::Magenta).add_modifier(Modifier::BOLD)
    }

    /// The provider's own color (see [`BRAND_COLORS`]).
    pub fn agent(self, agent: Agent) -> Style {
        let (_, dark, light) = BRAND_COLORS
            .iter()
            .find(|(a, ..)| *a == agent)
            .copied()
            .unwrap_or((agent, (255, 255, 255), (0, 0, 0)));
        let (r, g, b) = if self.light { light } else { dark };
        self.fg(Color::Rgb(r, g, b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_has_a_brand_color() {
        for agent in [Agent::Claude, Agent::Codex, Agent::OpenCode] {
            assert!(BRAND_COLORS.iter().any(|(a, ..)| *a == agent), "{agent}");
        }
        assert_eq!(
            Theme::plain().agent(Agent::Claude).fg,
            Some(Color::Rgb(0xD9, 0x77, 0x57))
        );
    }

    #[test]
    fn state_colors() {
        let t = Theme::plain();
        assert_eq!(t.state_color(State::Blocked), Color::Rgb(0xFA, 0xB3, 0x87));
        assert_eq!(t.state_color(State::Working), Color::Green);
        assert_eq!(t.state_color(State::Faded), Color::DarkGray);
    }

    #[test]
    fn tells_light_from_dark_backgrounds() {
        assert!(is_light((0xEF, 0xF1, 0xF5)), "Catppuccin Latte");
        assert!(!is_light((0x1E, 0x1E, 0x2E)), "Catppuccin Mocha");
    }

    #[test]
    fn panels_are_darker_than_the_terminal() {
        // Catppuccin Mocha: a deeper shade of its background.
        let mocha = Surfaces::from_palette((0xCD, 0xD6, 0xF4), (0x1E, 0x1E, 0x2E));
        assert_eq!(mocha.raised, Color::Rgb(20, 20, 30));
        // Pure black cannot go deeper: a hint toward the text instead.
        let black = Surfaces::from_palette((255, 255, 255), (0, 0, 0));
        assert_eq!(black.raised, Color::Rgb(10, 10, 10));
        let light = Surfaces::from_palette((0, 0, 0), (255, 255, 255));
        assert_eq!(light.raised, Color::Rgb(240, 240, 240));
        // A highlight inside a panel is a neutral gray, not a tint.
        assert_eq!(black.highlight, Color::Rgb(26, 26, 26));
    }
}
