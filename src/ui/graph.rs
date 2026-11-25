//! Braille history graphs, as in btop: each character holds two samples
//! side by side and four dots of height.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::Theme;

/// Dot rows of a braille cell, bottom to top, for its left and right columns.
const LEFT: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
const RIGHT: [u32; 4] = [0x80, 0x20, 0x10, 0x08];

/// Draws `samples` (percentages, oldest first) as a filled area graph
/// `width` characters wide and `height` lines tall. The newest samples sit on
/// the right; missing history is left blank. Each line is colored by the
/// level it represents, so the top of a busy graph turns yellow, then red.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn area(t: Theme, samples: &[f32], width: usize, height: usize) -> Vec<Line<'static>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let slots = width * 2;
    let recent = &samples[samples.len().saturating_sub(slots)..];
    let missing = slots - recent.len();
    let dots = height * 4;
    // Height of each slot in dots, `None` where there is no sample yet.
    let levels: Vec<Option<usize>> = (0..slots)
        .map(|i| {
            i.checked_sub(missing).map(|j| {
                let v = f64::from(recent[j].clamp(0.0, 100.0));
                // Any activity shows at least one dot.
                let h = (v / 100.0 * dots as f64).round() as usize;
                if v > 0.0 { h.max(1) } else { h }
            })
        })
        .collect();

    (0..height)
        .rev()
        .map(|row| {
            let floor = row * 4;
            // The level this line stands for, for its color.
            let band = (row + 1) as f64 / height as f64 * 100.0;
            let style = t.level(band);
            let text: String = levels
                .chunks(2)
                .map(|pair| {
                    let fill =
                        |level: Option<usize>| level.map_or(0, |h| h.saturating_sub(floor).min(4));
                    let (l, r) = (fill(pair[0]), fill(pair.get(1).copied().flatten()));
                    let bits: u32 = LEFT[..l].iter().chain(&RIGHT[..r]).sum();
                    char::from_u32(0x2800 + bits).unwrap_or(' ')
                })
                .map(|c| if c == '\u{2800}' { ' ' } else { c })
                .collect();
            Line::from(Span::styled(text, style))
        })
        .collect()
}

/// A one-line graph, for per-core history.
pub fn spark(t: Theme, samples: &[f32], width: usize, style: Option<Style>) -> Span<'static> {
    let line = area(t, samples, width, 1).pop().unwrap_or_default();
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let text = text.replace(' ', "⣀");
    Span::styled(text, style.unwrap_or_else(|| t.track()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn fills_from_the_bottom_with_newest_on_the_right() {
        let t = Theme::plain();
        // Two lines, eight dots tall: 100% fills both, 50% only the bottom one.
        let lines = area(t, &[100.0, 100.0, 50.0, 50.0], 3, 2);
        assert_eq!(text(&lines), [" ⣿ ", " ⣿⣿"]);
    }

    #[test]
    fn small_values_stay_visible() {
        let t = Theme::plain();
        let lines = area(t, &[1.0, 0.0], 1, 1);
        assert_eq!(text(&lines), ["⡀"]);
    }
}
