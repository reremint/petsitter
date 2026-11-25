//! Turns the flat session list into the grouped layout shown on screen.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::model::{Activity, Agent, Session};

/// Where a session is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Inside its repository card.
    Active,
    /// Alive, but nothing happened for a while.
    Inactive,
    /// The process is gone.
    Exited,
}

/// Stable identity of a session across refreshes.
pub type SessionKey = (Agent, String);

pub fn key(session: &Session) -> SessionKey {
    (session.agent, session.id.clone())
}

/// Decides the section of a session.
///
/// A session waiting for the user always stays active: it needs attention no
/// matter how long it has been waiting.
pub fn section(session: &Session, now: SystemTime, idle_after: Duration) -> Section {
    match session.activity {
        Activity::Exited => Section::Exited,
        Activity::Waiting { .. } => Section::Active,
        // Background commands and sub-agents keep a session awake even when
        // its transcript is still.
        Activity::Working if !session.tasks.is_empty() || !session.subagents.is_empty() => {
            Section::Active
        }
        Activity::Working | Activity::Idle => {
            let quiet = now.duration_since(session.last_active).unwrap_or_default();
            if quiet > idle_after {
                Section::Inactive
            } else {
                Section::Active
            }
        }
    }
}

/// Sort rank inside a repository card: most urgent first.
fn urgency(activity: &Activity) -> u8 {
    match activity {
        Activity::Waiting { .. } => 0,
        Activity::Working => 1,
        Activity::Idle => 2,
        Activity::Exited => 3,
    }
}

/// The repository a working directory belongs to: its first path component
/// below `root` (or the root's own name for sessions started in the root),
/// plus the remaining sub-path if any.
pub fn repo_of(root: &Path, cwd: &Path) -> (String, Option<PathBuf>) {
    let Ok(relative) = cwd.strip_prefix(root) else {
        return (cwd.display().to_string(), None);
    };
    let mut components = relative
        .components()
        .filter(|c| matches!(c, Component::Normal(_)));
    let Some(repo) = components.next() else {
        let name = root.file_name().map_or_else(
            || root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        return (name, None);
    };
    let rest: PathBuf = components.collect();
    (
        repo.as_os_str().to_string_lossy().into_owned(),
        (!rest.as_os_str().is_empty()).then_some(rest),
    )
}

/// Directory of the repository a working directory belongs to.
pub fn repo_dir(root: &Path, cwd: &Path) -> PathBuf {
    match cwd
        .strip_prefix(root)
        .ok()
        .and_then(|r| r.components().next())
    {
        Some(Component::Normal(first)) => root.join(first),
        _ => root.to_path_buf(),
    }
}

/// A repository card and the indices of its active sessions.
#[derive(Debug, Clone)]
pub struct RepoGroup {
    pub name: String,
    pub sessions: Vec<usize>,
}

/// Grouped, sorted indices into the session list.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub repos: Vec<RepoGroup>,
    pub inactive: Vec<usize>,
    pub exited: Vec<usize>,
}

impl View {
    pub fn build(sessions: &[Session], root: &Path, now: SystemTime, idle_after: Duration) -> Self {
        let mut repos: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut view = Self::default();

        for (index, session) in sessions.iter().enumerate() {
            match section(session, now, idle_after) {
                Section::Active => {
                    repos
                        .entry(repo_of(root, &session.cwd).0)
                        .or_default()
                        .push(index);
                }
                Section::Inactive => view.inactive.push(index),
                Section::Exited => view.exited.push(index),
            }
        }

        for (name, mut indices) in repos {
            indices.sort_by(|&a, &b| {
                let (a, b) = (&sessions[a], &sessions[b]);
                urgency(&a.activity)
                    .cmp(&urgency(&b.activity))
                    .then_with(|| a.name.cmp(&b.name))
                    .then_with(|| a.id.cmp(&b.id))
            });
            view.repos.push(RepoGroup {
                name,
                sessions: indices,
            });
        }
        let newest_first = |list: &mut Vec<usize>| {
            list.sort_by(|&a, &b| {
                sessions[b]
                    .last_active
                    .cmp(&sessions[a].last_active)
                    .then_with(|| sessions[a].id.cmp(&sessions[b].id))
            });
        };
        newest_first(&mut view.inactive);
        newest_first(&mut view.exited);
        view
    }

    /// Session indices in on-screen order, for keyboard navigation.
    pub fn order(&self) -> impl Iterator<Item = usize> + '_ {
        self.repos
            .iter()
            .flat_map(|r| r.sessions.iter().copied())
            .chain(self.inactive.iter().copied())
            .chain(self.exited.iter().copied())
    }

    /// Sessions waiting for the user, in repository order.
    pub fn waiting<'a>(&'a self, sessions: &'a [Session]) -> impl Iterator<Item = usize> + 'a {
        self.repos
            .iter()
            .flat_map(|r| r.sessions.iter().copied())
            .filter(|&i| matches!(sessions[i].activity, Activity::Waiting { .. }))
    }

    /// On-screen order: sessions that need the user first, then
    /// the other active sessions by repository, then the inactive ones, then
    /// (when shown) the exited ones.
    pub fn display_order(&self, sessions: &[Session], exited: bool) -> Vec<usize> {
        let mut order: Vec<usize> = self.waiting(sessions).collect();
        order.extend(
            self.repos
                .iter()
                .flat_map(|r| r.sessions.iter().copied())
                .filter(|&i| !matches!(sessions[i].activity, Activity::Waiting { .. })),
        );
        order.extend(&self.inactive);
        if exited {
            order.extend(&self.exited);
        }
        order
    }
}

/// Counts shown in the header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub waiting: usize,
    pub working: usize,
    pub ready: usize,
    pub inactive: usize,
    pub exited: usize,
}

impl Counts {
    pub fn of(sessions: &[Session], view: &View) -> Self {
        let mut counts = Self {
            inactive: view.inactive.len(),
            exited: view.exited.len(),
            ..Self::default()
        };
        for &index in view.repos.iter().flat_map(|r| &r.sessions) {
            match sessions[index].activity {
                Activity::Waiting { .. } => counts.waiting += 1,
                Activity::Working => counts.working += 1,
                Activity::Idle => counts.ready += 1,
                Activity::Exited => {}
            }
        }
        counts
    }
}

/// The linked worktree containing `dir`, if any.
///
/// A worktree has a `.git` file (not a directory) pointing to
/// `<main>/.git/worktrees/<name>`; that directory's `commondir` leads back to
/// the main repository's `.git`.
pub fn worktree_of(dir: &Path) -> Option<crate::model::Worktree> {
    let top = dir.ancestors().find(|d| d.join(".git").exists())?;
    let dot_git = top.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        top.join(git_dir)
    };
    let common = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common = git_dir.join(common.trim());
    let common = common.canonicalize().unwrap_or(common);
    // Submodules also use a `.git` file, but their common dir is not `.git`.
    if common.file_name()? != ".git" {
        return None;
    }
    Some(crate::model::Worktree {
        main: common.parent()?.to_path_buf(),
        branch: git_branch(top),
    })
}

/// Current branch of the repository at `dir`, read from `.git/HEAD`.
///
/// Supports worktrees and submodules, where `.git` is a file pointing to the
/// real git directory. Detached heads are shown as a short commit hash.
pub fn git_branch(dir: &Path) -> Option<String> {
    let dot_git = dir.join(".git");
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            dir.join(target)
        }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(reference) => {
            let reference = reference.trim();
            Some(
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(reference)
                    .to_owned(),
            )
        }
        None => Some(head.chars().take(7).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, cwd: &str, activity: Activity, age_secs: u64, now: SystemTime) -> Session {
        let mut s = Session::new(
            Agent::Claude,
            id,
            PathBuf::from(cwd),
            now - Duration::from_secs(age_secs),
        );
        s.activity = activity;
        s
    }

    #[test]
    fn repo_is_first_component_below_root() {
        let root = Path::new("/lab");
        assert_eq!(repo_of(root, Path::new("/lab/app")), ("app".into(), None));
        assert_eq!(repo_of(root, Path::new("/lab")), ("lab".into(), None));
        assert_eq!(
            repo_dir(root, Path::new("/lab/app/web")),
            Path::new("/lab/app")
        );
        assert_eq!(repo_dir(root, Path::new("/lab")), Path::new("/lab"));
        assert_eq!(
            repo_of(root, Path::new("/lab/app/web/src")),
            ("app".into(), Some("web/src".into()))
        );
    }

    #[test]
    fn groups_and_classifies_sessions() {
        let now = SystemTime::now();
        let idle = Duration::from_secs(600);
        let sessions = vec![
            session("ready", "/lab/b", Activity::Idle, 30, now),
            session("quiet", "/lab/b", Activity::Idle, 3600, now),
            session("stuck", "/lab/a", Activity::Working, 3600, now),
            session(
                "ask",
                "/lab/b/sub",
                Activity::Waiting {
                    reason: "permission".into(),
                },
                3600,
                now,
            ),
            session("busy", "/lab/b", Activity::Working, 5, now),
            session("gone", "/lab/a", Activity::Exited, 100, now),
        ];
        let view = View::build(&sessions, Path::new("/lab"), now, idle);

        assert_eq!(view.repos.len(), 1);
        assert_eq!(view.repos[0].name, "b");
        let ids: Vec<&str> = view.repos[0]
            .sessions
            .iter()
            .map(|&i| sessions[i].id.as_str())
            .collect();
        assert_eq!(ids, ["ask", "busy", "ready"]);
        assert_eq!(view.inactive, [1, 2], "same age: ordered by id");
        assert_eq!(view.exited, [5]);
        assert_eq!(
            Counts::of(&sessions, &view),
            Counts {
                waiting: 1,
                working: 1,
                ready: 1,
                inactive: 2,
                exited: 1
            }
        );
        assert_eq!(view.order().count(), sessions.len());
        let shown: Vec<&str> = view
            .display_order(&sessions, false)
            .iter()
            .map(|&i| sessions[i].id.as_str())
            .collect();
        assert_eq!(shown, ["ask", "busy", "ready", "quiet", "stuck"]);
        assert_eq!(view.display_order(&sessions, true).len(), sessions.len());
    }

    #[test]
    fn finds_the_main_repository_of_a_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("app");
        let git_dir = main.join(".git/worktrees/app-feature");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        let tree = root.join("app-feature");
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();

        let found = worktree_of(&tree.join("src")).expect("a worktree");
        assert_eq!(found.main, main);
        assert_eq!(found.branch.as_deref(), Some("feature"));
        assert_eq!(
            worktree_of(&main),
            None,
            "the main checkout is not a worktree"
        );
    }

    #[test]
    fn reads_branch_from_head_and_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(git_branch(&repo).as_deref(), Some("feat/x"));

        let worktree = dir.path().join("wt");
        let gitdir = dir.path().join("gitdir");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "0123456789abcdef\n").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        assert_eq!(git_branch(&worktree).as_deref(), Some("0123456"));
    }
}
