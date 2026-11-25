//! The folder tree of the default layout.
//!
//! Sessions are placed in the folder they run in, below the monitored root.
//! Inside a folder, sessions come first, by status (blocked, ready, working,
//! quiet, exited) and then by when the user last prompted them, most recent
//! first; sub-folders follow, in the same order (the most pressing status
//! inside them, then their latest prompt).
//!
//! Sessions running in a linked git worktree are filed under the repository
//! the worktree belongs to.
//!
//! Folders start collapsed: a collapsed folder lists its live sessions and
//! folds the exited ones into a single "N hidden" line; expanding it lists them
//! too. The user's choices override that default.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::model::{Activity, Session};
use crate::view::{self, Section, SessionKey};

/// The state of a session, in display priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Blocked,
    Ready,
    Working,
    Quiet,
    Exited,
}

impl Status {
    pub fn of(session: &Session, now: SystemTime, idle_after: Duration) -> Self {
        match (view::section(session, now, idle_after), &session.activity) {
            (Section::Exited, _) => Self::Exited,
            (Section::Inactive, _) => Self::Quiet,
            (_, Activity::Waiting { .. }) => Self::Blocked,
            (_, Activity::Working) => Self::Working,
            (_, Activity::Idle | Activity::Exited) => Self::Ready,
        }
    }
}

/// Identity of a row, stable across refreshes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowKey {
    Folder(PathBuf),
    Session(SessionKey),
    /// The "N hidden" line of a collapsed folder.
    More(PathBuf),
}

/// One line of the tree.
#[derive(Debug, Clone)]
pub enum Row {
    Folder {
        path: PathBuf,
        /// Display name; chains of single sub-folders are joined (`a/b/c`).
        name: String,
        depth: usize,
        open: bool,
        /// Whether a session inside (sub-folders included) is blocked.
        blocked: bool,
    },
    Session {
        index: usize,
        depth: usize,
        status: Status,
    },
    /// Exited sessions hidden by a collapsed folder.
    More {
        path: PathBuf,
        depth: usize,
        count: usize,
    },
}

impl Row {
    pub fn key(&self, sessions: &[Session]) -> RowKey {
        match self {
            Self::Folder { path, .. } => RowKey::Folder(path.clone()),
            Self::Session { index, .. } => RowKey::Session(view::key(&sessions[*index])),
            Self::More { path, .. } => RowKey::More(path.clone()),
        }
    }

    pub fn depth(&self) -> usize {
        match self {
            Self::Folder { depth, .. } | Self::Session { depth, .. } | Self::More { depth, .. } => {
                *depth
            }
        }
    }
}

#[derive(Debug, Default)]
struct Node {
    sessions: Vec<usize>,
    children: BTreeMap<String, Node>,
}

/// Builds the visible rows of the tree.
pub fn rows(
    sessions: &[Session],
    root: &Path,
    now: SystemTime,
    idle_after: Duration,
    folds: &HashMap<PathBuf, bool>,
    hide_exited: bool,
) -> Vec<Row> {
    let mut top = Node::default();
    for (index, session) in sessions.iter().enumerate() {
        if hide_exited && Status::of(session, now, idle_after) == Status::Exited {
            continue;
        }
        // Sessions in a worktree live under the repository it belongs to.
        let place = session
            .worktree
            .as_ref()
            .map(|w| &w.main)
            .filter(|main| main.starts_with(root))
            .unwrap_or(&session.cwd);
        let Ok(relative) = place.strip_prefix(root) else {
            continue;
        };
        let mut node = &mut top;
        for part in relative.components() {
            if let Component::Normal(name) = part {
                node = node
                    .children
                    .entry(name.to_string_lossy().into_owned())
                    .or_default();
            }
        }
        node.sessions.push(index);
    }

    let ctx = Ctx {
        sessions,
        now,
        idle_after,
        folds,
    };
    let mut rows = Vec::new();
    // Sessions started in the root itself get a folder named after the root.
    let lone = Node {
        sessions: std::mem::take(&mut top.sessions),
        children: BTreeMap::new(),
    };
    let mut folders: Vec<(&str, &Node, PathBuf)> = top
        .children
        .iter()
        .map(|(name, child)| (name.as_str(), child, root.join(name)))
        .collect();
    let root_name = root.file_name().map_or_else(
        || root.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    if !lone.sessions.is_empty() {
        folders.push((root_name.as_str(), &lone, root.to_path_buf()));
    }
    ctx.sort_folders(&mut folders);
    for (name, node, path) in folders {
        if path == root {
            ctx.push(&mut rows, node, name.to_owned(), &path, 0);
        } else {
            ctx.push_compacted(&mut rows, node, name, &path, 0);
        }
    }
    rows
}

struct Ctx<'a> {
    sessions: &'a [Session],
    now: SystemTime,
    idle_after: Duration,
    /// Folders the user expanded (`true`) or collapsed (`false`).
    folds: &'a HashMap<PathBuf, bool>,
}

impl Ctx<'_> {
    fn status(&self, index: usize) -> Status {
        Status::of(&self.sessions[index], self.now, self.idle_after)
    }

    /// Joins folders that hold nothing but a single sub-folder.
    fn push_compacted(
        &self,
        rows: &mut Vec<Row>,
        node: &Node,
        name: &str,
        path: &Path,
        depth: usize,
    ) {
        let (mut node, mut name, mut path) = (node, name.to_owned(), path.to_path_buf());
        while node.sessions.is_empty() && node.children.len() == 1 {
            let (child_name, child) = node.children.iter().next().expect("one child");
            name = format!("{name}/{child_name}");
            path = path.join(child_name);
            node = child;
        }
        self.push(rows, node, name, &path, depth);
    }

    fn push(&self, rows: &mut Vec<Row>, node: &Node, name: String, path: &Path, depth: usize) {
        let open = self.folds.get(path).copied().unwrap_or(false);
        rows.push(Row::Folder {
            path: path.to_path_buf(),
            name,
            depth,
            open,
            blocked: self.has_blocked(node),
        });

        let mut own: Vec<(Status, usize)> =
            node.sessions.iter().map(|&i| (self.status(i), i)).collect();
        // By status, then the session the user prompted last first. Prompts
        // are rare compared to tool calls, so rows stay put while agents work.
        own.sort_by(|a, b| {
            let (sa, sb) = (&self.sessions[a.1], &self.sessions[b.1]);
            let prompted = |s: &Session| s.last_prompt_at.or(s.started_at);
            a.0.cmp(&b.0)
                .then_with(|| prompted(sb).cmp(&prompted(sa)))
                .then_with(|| sa.id.cmp(&sb.id))
        });
        let hidden = if open {
            0
        } else {
            own.iter().filter(|(s, _)| *s == Status::Exited).count()
        };
        for (status, index) in own {
            if open || status != Status::Exited {
                rows.push(Row::Session {
                    index,
                    depth: depth + 1,
                    status,
                });
            }
        }
        if hidden > 0 {
            rows.push(Row::More {
                path: path.to_path_buf(),
                depth: depth + 1,
                count: hidden,
            });
        }
        let mut folders: Vec<(&str, &Node, PathBuf)> = node
            .children
            .iter()
            .map(|(name, child)| (name.as_str(), child, path.join(name)))
            .collect();
        self.sort_folders(&mut folders);
        for (name, child, child_path) in folders {
            self.push_compacted(rows, child, name, &child_path, depth + 1);
        }
    }

    /// Orders folders like the sessions in them: by the most pressing status
    /// inside (sub-folders included), then by the latest prompt inside, most
    /// recent first, then by name.
    fn sort_folders(&self, folders: &mut [(&str, &Node, PathBuf)]) {
        folders.sort_by(|a, b| {
            let (sa, pa) = self.rank(a.1);
            let (sb, pb) = self.rank(b.1);
            sa.cmp(&sb)
                .then_with(|| pb.cmp(&pa))
                .then_with(|| a.0.cmp(b.0))
        });
    }

    /// The most pressing status in a folder and the latest prompt in it.
    fn rank(&self, node: &Node) -> (Status, Option<SystemTime>) {
        let own = node.sessions.iter().map(|&i| {
            let session = &self.sessions[i];
            (
                self.status(i),
                session.last_prompt_at.or(session.started_at),
            )
        });
        own.chain(node.children.values().map(|child| self.rank(child)))
            .fold((Status::Exited, None), |(status, prompted), (s, p)| {
                (status.min(s), prompted.max(p))
            })
    }

    fn has_blocked(&self, node: &Node) -> bool {
        node.sessions
            .iter()
            .any(|&i| self.status(i) == Status::Blocked)
            || node.children.values().any(|c| self.has_blocked(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Agent;

    fn session(id: &str, cwd: &str, activity: Activity, age: u64, now: SystemTime) -> Session {
        let mut s = Session::new(
            Agent::Claude,
            id,
            PathBuf::from(cwd),
            now - Duration::from_secs(age),
        );
        s.activity = activity;
        s
    }

    fn names(rows: &[Row], sessions: &[Session]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let indent = "  ".repeat(row.depth());
                match row {
                    Row::Folder { name, open, .. } => {
                        format!("{indent}{}{name}", if *open { "v " } else { "> " })
                    }
                    Row::Session { index, .. } => format!("{indent}{}", sessions[*index].id),
                    Row::More { count, .. } => format!("{indent}+{count}"),
                }
            })
            .collect()
    }

    fn sample(now: SystemTime) -> Vec<Session> {
        vec![
            session("quiet", "/lab/app", Activity::Idle, 3600, now),
            session("busy", "/lab/app", Activity::Working, 5, now),
            session(
                "ask",
                "/lab/app",
                Activity::Waiting {
                    reason: "permission".into(),
                },
                30,
                now,
            ),
            session("gone", "/lab/app", Activity::Exited, 60, now),
            session("gone2", "/lab/app", Activity::Exited, 90, now),
            session("deep", "/lab/app/web/ui", Activity::Working, 5, now),
            session("nested", "/lab/mono/pkg/core", Activity::Exited, 10, now),
            session("top", "/lab", Activity::Idle, 10, now),
        ]
    }

    #[test]
    fn collapsed_folders_hide_exited_sessions() {
        let now = SystemTime::now();
        let sessions = sample(now);
        let rows = rows(
            &sessions,
            Path::new("/lab"),
            now,
            Duration::from_secs(600),
            &HashMap::new(),
            false,
        );
        assert_eq!(
            names(&rows, &sessions),
            [
                "> app",
                "  ask",
                "  busy",
                "  quiet",
                "  +2",
                "  > web/ui",
                "    deep",
                "> lab",
                "  top",
                "> mono/pkg/core",
                "  +1",
            ]
        );
        assert!(matches!(&rows[0], Row::Folder { blocked: true, .. }));
    }

    #[test]
    fn orders_by_status_then_latest_prompt() {
        let now = SystemTime::now();
        let prompted = |id: &str, activity: Activity, minutes_ago: u64| {
            let mut s = session(id, "/lab/app", activity, 1, now);
            s.last_prompt_at = Some(now - Duration::from_secs(minutes_ago * 60));
            s
        };
        let sessions = vec![
            prompted("busy-old", Activity::Working, 30),
            prompted("busy-new", Activity::Working, 2),
            prompted("ready", Activity::Idle, 60),
            prompted(
                "ask",
                Activity::Waiting {
                    reason: "permission".into(),
                },
                90,
            ),
        ];
        let rows = rows(
            &sessions,
            Path::new("/lab"),
            now,
            Duration::from_secs(600),
            &HashMap::new(),
            false,
        );
        assert_eq!(
            names(&rows, &sessions),
            ["> app", "  ask", "  ready", "  busy-new", "  busy-old"]
        );
    }

    #[test]
    fn orders_folders_like_sessions() {
        let now = SystemTime::now();
        let prompted = |id: &str, cwd: &str, activity: Activity, minutes_ago: u64| {
            let mut s = session(id, cwd, activity, 1, now);
            s.last_prompt_at = Some(now - Duration::from_secs(minutes_ago * 60));
            s
        };
        let sessions = vec![
            prompted("a", "/lab/alpha", Activity::Working, 5),
            prompted("b", "/lab/beta", Activity::Working, 1),
            prompted("c", "/lab/gamma", Activity::Idle, 60),
        ];
        let rows = rows(
            &sessions,
            Path::new("/lab"),
            now,
            Duration::from_secs(600),
            &HashMap::new(),
            false,
        );
        let folders: Vec<String> = names(&rows, &sessions)
            .into_iter()
            .filter(|n| n.starts_with('>'))
            .collect();
        // Ready first, then the working folder prompted last.
        assert_eq!(folders, ["> gamma", "> beta", "> alpha"]);
    }

    #[test]
    fn exited_sessions_can_be_left_out() {
        let now = SystemTime::now();
        let sessions = sample(now);
        let rows = rows(
            &sessions,
            Path::new("/lab"),
            now,
            Duration::from_secs(600),
            &HashMap::new(),
            true,
        );
        let names = names(&rows, &sessions);
        assert!(!names.iter().any(|n| n.contains('+')), "no hidden line");
        // A folder holding only exited sessions goes too.
        assert!(!names.iter().any(|n| n.contains("mono")));
    }

    #[test]
    fn worktree_sessions_join_their_repository() {
        let now = SystemTime::now();
        let mut feature = session("feat", "/lab/app-feature", Activity::Working, 5, now);
        feature.worktree = Some(crate::model::Worktree {
            main: PathBuf::from("/lab/app"),
            branch: Some("feature".into()),
        });
        let sessions = vec![
            session("main", "/lab/app", Activity::Working, 5, now),
            feature,
        ];
        let rows = rows(
            &sessions,
            Path::new("/lab"),
            now,
            Duration::from_secs(600),
            &HashMap::new(),
            false,
        );
        assert_eq!(names(&rows, &sessions), ["> app", "  feat", "  main"]);
    }

    #[test]
    fn expanded_folders_list_everything() {
        let now = SystemTime::now();
        let sessions = sample(now);
        let folds = HashMap::from([(PathBuf::from("/lab/app"), true)]);
        let rows = rows(
            &sessions,
            Path::new("/lab"),
            now,
            Duration::from_secs(600),
            &folds,
            false,
        );
        let names = names(&rows, &sessions);
        let app: Vec<&str> = names[0..7].iter().map(String::as_str).collect();
        assert_eq!(
            app,
            [
                "v app",
                "  ask",
                "  busy",
                "  quiet",
                "  gone",
                "  gone2",
                "  > web/ui"
            ]
        );
    }
}
