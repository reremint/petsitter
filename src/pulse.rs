//! Per-session activity history, drawn as a small braille trace.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use crate::model::Session;

/// How far back the heartbeat looks.
pub const WINDOW: Duration = Duration::from_mins(10);

/// CPU below this share of one core counts as idle (a waiting process).
const IDLE_CPU: f32 = 3.0;

/// What a session did lately: the moments its transcripts moved (its own
/// and its sub-agents'), and the CPU its whole process tree used (shells,
/// monitors and other background commands, sub-agents' included).
#[derive(Debug, Default)]
pub struct Pulse {
    last_seen: Option<SystemTime>,
    last_subagent: Option<SystemTime>,
    events: VecDeque<SystemTime>,
    cpu: VecDeque<(SystemTime, f32)>,
}

impl Pulse {
    /// Records the session's latest activity, if it moved since last time,
    /// and a sample of its CPU.
    ///
    /// The first observation is seeded with the start times of its recent tool
    /// calls, so the heartbeat is not empty right after launch.
    pub fn observe(&mut self, session: &Session, now: SystemTime) {
        if self.last_seen.is_none() {
            self.events
                .extend(session.recent_tools.iter().filter_map(|t| t.started_at));
        }
        if self.last_seen != Some(session.last_active) {
            self.last_seen = Some(session.last_active);
            self.events.push_back(session.last_active);
        }
        let subagent = session.subagents.iter().map(|a| a.last_active).max();
        if subagent.is_some() && subagent != self.last_subagent {
            self.last_subagent = subagent;
            self.events.extend(subagent);
        }
        if let Some(cpu) = session.cpu {
            self.cpu.push_back((now, cpu));
        }
        if let Some(cutoff) = now.checked_sub(WINDOW) {
            self.events.retain(|&t| t >= cutoff);
            self.cpu.retain(|&(t, _)| t >= cutoff);
        }
    }

    /// Activity levels from 0 to 1 over [`WINDOW`], oldest bucket first: the
    /// higher of the transcript activity (relative to the busiest bucket) and
    /// the CPU (a full core is the top).
    pub fn buckets(&self, now: SystemTime, count: usize) -> Vec<f32> {
        if count == 0 {
            return Vec::new();
        }
        #[allow(clippy::cast_precision_loss)] // `count` is a handful of cells
        let span = WINDOW.as_secs_f64() / count as f64;
        let bucket = |at: SystemTime| -> Option<usize> {
            // Clock skew: count it as happening now.
            let age = now.duration_since(at).unwrap_or_default();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let from_end = (age.as_secs_f64() / span) as usize;
            (age < WINDOW).then(|| count - 1 - from_end.min(count - 1))
        };

        let mut events = vec![0_u32; count];
        for &at in &self.events {
            if let Some(i) = bucket(at) {
                events[i] += 1;
            }
        }
        let mut cpu = vec![0.0_f32; count];
        for &(at, value) in &self.cpu {
            if let Some(i) = bucket(at) {
                cpu[i] = cpu[i].max(value);
            }
        }
        // A few events per bucket already means "busy"; avoid single events
        // looking like peaks on quiet sessions.
        let busiest = events.iter().copied().max().unwrap_or(0).max(3);
        events
            .iter()
            .zip(&cpu)
            .map(|(&n, &cpu)| {
                #[allow(clippy::cast_precision_loss)]
                let transcript = n as f32 / busiest as f32;
                let cpu = if cpu < IDLE_CPU {
                    0.0
                } else {
                    (cpu / 100.0).min(1.0)
                };
                transcript.max(cpu)
            })
            .collect()
    }
}

/// Dot rows of a braille cell, bottom to top, for its left and right columns.
const LEFT_DOTS: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
const RIGHT_DOTS: [u32; 4] = [0x80, 0x20, 0x10, 0x08];

/// Renders activity levels as a braille bar chart, two buckets per character
/// and up to four dots high, like a small oscilloscope trace.
///
/// Returns each character with whether it shows any activity, so callers can
/// draw idle stretches as a faint baseline.
pub fn braille(levels: &[f32]) -> Vec<(char, bool)> {
    // Any activity shows at least one dot.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let height = |level: f32| ((level.clamp(0.0, 1.0) * 4.0).ceil() as usize).min(4);
    levels
        .chunks(2)
        .map(|pair| {
            let left = height(pair[0]);
            let right = pair.get(1).map_or(0, |&n| height(n));
            if left == 0 && right == 0 {
                // Baseline: the two bottom dots.
                return ('⣀', false);
            }
            let bits: u32 = LEFT_DOTS[..left].iter().chain(&RIGHT_DOTS[..right]).sum();
            (char::from_u32(0x2800 + bits).unwrap_or('⣀'), true)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, Session};
    use std::path::PathBuf;

    #[test]
    fn records_activity_once_per_change() {
        let now = SystemTime::now();
        let mut session = Session::new(Agent::Claude, "s", PathBuf::new(), now);
        let mut pulse = Pulse::default();
        pulse.observe(&session, now);
        pulse.observe(&session, now);
        session.last_active = now - Duration::from_secs(90);
        pulse.observe(&session, now);

        let buckets = pulse.buckets(now, 10);
        let busy: Vec<usize> = (0..10).filter(|&i| buckets[i] > 0.0).collect();
        assert_eq!(
            busy,
            [8, 9],
            "now, and 90 seconds ago in one-minute buckets"
        );
    }

    #[test]
    fn old_events_are_dropped() {
        let now = SystemTime::now();
        let session = Session::new(Agent::Codex, "s", PathBuf::new(), now - WINDOW * 2);
        let mut pulse = Pulse::default();
        pulse.observe(&session, now);
        assert_eq!(pulse.buckets(now, 5), [0.0; 5]);
    }

    #[test]
    fn counts_the_process_tree_and_subagents() {
        let now = SystemTime::now();
        let old = now - WINDOW * 2;
        let mut session = Session::new(Agent::Claude, "s", PathBuf::new(), old);
        let mut pulse = Pulse::default();
        // A background shell keeps a core half busy: no transcript activity.
        session.cpu = Some(50.0);
        pulse.observe(&session, now);
        assert_eq!(pulse.buckets(now, 5)[4], 0.5);
        // A waiting process stays flat.
        let mut quiet = Pulse::default();
        session.cpu = Some(1.0);
        quiet.observe(&session, now);
        assert_eq!(quiet.buckets(now, 5), [0.0; 5]);
        // A sub-agent at work shows even when the session's own transcript
        // is still.
        session.subagents = vec![crate::model::SubAgent {
            description: "Review".into(),
            kind: None,
            current_tool: None,
            activity: None,
            tokens_used: None,
            model: None,
            effort: None,
            last_active: now,
            level: 0,
        }];
        quiet.observe(&session, now);
        assert!(quiet.buckets(now, 5)[4] > 0.0);
    }

    #[test]
    fn draws_braille_traces() {
        let text = |buckets: &[f32]| {
            braille(buckets)
                .into_iter()
                .map(|(c, _)| c)
                .collect::<String>()
        };
        assert_eq!(text(&[0.0, 0.0, 0.0, 0.0]), "⣀⣀");
        assert_eq!(text(&[1.0, 1.0]), "⣿", "full on both sides");
        assert_eq!(text(&[1.0, 0.0]), "⡇", "full left column only");
        assert_eq!(text(&[0.0, 0.3]), "⢠", "a low bar on the right");
        assert_eq!(
            braille(&[0.0, 0.0, 0.3, 0.0])
                .iter()
                .map(|&(_, a)| a)
                .collect::<Vec<_>>(),
            [false, true]
        );
    }
}
