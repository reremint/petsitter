//! Text formatting helpers.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::model::Agent;

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// Compact duration: `12s`, `4m`, `2h05`, `3d`.
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h{:02}", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Time elapsed since `then`, clamped at zero for clocks that drift.
pub fn since(now: SystemTime, then: SystemTime) -> String {
    duration(now.duration_since(then).unwrap_or_default())
}

/// Token counts: `950`, `84k`, `1.2M`.
#[allow(clippy::cast_precision_loss)]
pub fn tokens(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{}k", n / 1000),
        _ if n.is_multiple_of(1_000_000) => format!("{}M", n / 1_000_000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Byte sizes: `512M`, `2.1G`, `32G`.
#[allow(clippy::cast_precision_loss)]
pub fn bytes(n: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let gib = n as f64 / GIB;
    if gib >= 10.0 {
        format!("{gib:.0}G")
    } else if gib >= 1.0 {
        format!("{gib:.1}G")
    } else {
        format!("{:.0}M", n as f64 / (1024.0 * 1024.0))
    }
}

/// Truncates `text` to `width` terminal columns, ending with `…` when cut.
pub fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Pads `text` with spaces to exactly `width` columns, truncating if needed.
pub fn fit(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    let pad = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(pad))
}

/// Shortens a model identifier for display: the agent already tells the
/// vendor, so a redundant `claude-` prefix is dropped.
pub fn model(agent: Agent, model: &str) -> String {
    let model = match agent {
        Agent::Claude => model.strip_prefix("claude-").unwrap_or(model),
        Agent::Codex | Agent::OpenCode => model,
    };
    truncate(model, 24)
}

/// Displays a path with the home directory shortened to `~`.
pub fn home_relative(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(duration(Duration::from_secs(9)), "9s");
        assert_eq!(duration(Duration::from_secs(125)), "2m");
        assert_eq!(duration(Duration::from_mins(65)), "1h05");
        assert_eq!(duration(Duration::from_hours(72)), "3d");
    }

    #[test]
    fn formats_tokens() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(84_321), "84k");
        assert_eq!(tokens(1_250_000), "1.2M");
        assert_eq!(tokens(1_000_000), "1M");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(512 * 1024 * 1024), "512M");
        assert_eq!(bytes(2_254_857_830), "2.1G");
        assert_eq!(bytes(32 * 1024 * 1024 * 1024), "32G");
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("日本語テキスト", 5), "日本…");
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(model(Agent::Claude, "claude-opus-5-5"), "opus-5-5");
        assert_eq!(model(Agent::Codex, "gpt-5.5-codex"), "gpt-5.5-codex");
    }
}
