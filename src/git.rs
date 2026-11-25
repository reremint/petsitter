//! Repository details for the folder panel, read with the `git` command:
//! status, an overview of the branches sessions work on, and commits' diffs.
//!
//! Branch names come from agents' transcripts: they are only ever passed to
//! `git` after `--end-of-options`, once checked to name a local branch.

use std::path::Path;
use std::process::{Command, Stdio};

/// What the details panel shows about a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    /// Checked-out branch, or `None` when detached.
    pub branch: Option<String>,
    /// Commits ahead of and behind the upstream, when there is one.
    pub ahead_behind: Option<(u32, u32)>,
    /// Changed files: a short status (`M`, `A`, `D`, `R`, `?`) and the path.
    pub changes: Vec<(String, String)>,
}

/// Reads a repository's details; `None` when `dir` is not in a git repository.
pub fn info(dir: &Path) -> Option<GitInfo> {
    let status = git(dir, &["status", "--porcelain=v2", "--branch"])?;
    Some(parse_status(&status))
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        // Never block on locks, prompt for anything, or start a file monitor.
        .args([
            "--no-optional-locks",
            "-c",
            "core.quotepath=false",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_status(text: &str) -> GitInfo {
    let mut info = GitInfo::default();
    for line in text.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            info.branch = (head != "(detached)").then(|| head.to_owned());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut parts = ab.split_whitespace();
            let ahead = parts
                .next()
                .and_then(|a| a.trim_start_matches('+').parse().ok());
            let behind = parts
                .next()
                .and_then(|b| b.trim_start_matches('-').parse().ok());
            if let (Some(a), Some(b)) = (ahead, behind) {
                info.ahead_behind = Some((a, b));
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            info.changes.push(("?".to_owned(), path.to_owned()));
        } else if let Some((kind, rest)) = line
            .strip_prefix("1 ")
            .map(|r| (1, r))
            .or_else(|| line.strip_prefix("2 ").map(|r| (2, r)))
        {
            // `XY sub mH mI mW hH hI path`; renames (`2`) add a score field
            // before the path and `\torig` after it.
            let fields = if kind == 1 { 8 } else { 9 };
            let parts: Vec<&str> = rest.splitn(fields, ' ').collect();
            if let (Some(xy), Some(path)) = (parts.first(), parts.get(fields - 1)) {
                let path = path.split('\t').next().unwrap_or(path);
                let code = xy.chars().find(|c| *c != '.').unwrap_or('M');
                info.changes.push((code.to_string(), path.to_owned()));
            }
        } else if let Some(rest) = line.strip_prefix("u ") {
            let path = rest.rsplit(' ').next().unwrap_or(rest);
            info.changes.push(("U".to_owned(), path.to_owned()));
        }
    }
    info
}

/// A commit as the branch overview shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Point {
    /// Full hash, to be copied.
    pub hash: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    /// Relative date, e.g. `2 hours ago`.
    pub age: String,
}

/// A branch in the overview: how far it is from the trunk, and its latest
/// commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchLine {
    pub name: String,
    /// Commits on the branch since the fork, and on the trunk since then.
    pub ahead: u32,
    pub behind: u32,
    /// Its latest commits since the fork, newest first.
    pub commits: Vec<Point>,
}

/// Commits listed per branch in the overview.
pub const BRANCH_COMMITS: usize = 5;

/// The trunk of a repository and a few branches measured against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    /// The trunk's name (`main`, `origin/main`).
    pub trunk: String,
    pub tip: Point,
    pub branches: Vec<BranchLine>,
}

/// Measures `branches` against the repository's trunk (the branch the
/// remote's `HEAD` names, else `main` or `master`). Branches that do not
/// exist, or the trunk itself, are left out.
pub fn overview(dir: &Path, branches: &[String]) -> Option<Overview> {
    let trunk = trunk(dir)?;
    let tip = point(dir, &trunk)?;
    let mut lines = Vec::new();
    for name in branches {
        if *name == trunk
            || lines.iter().any(|l: &BranchLine| l.name == *name)
            || !is_branch(dir, name)
        {
            continue;
        }
        let range = format!("{trunk}...{name}");
        let counts = git(
            dir,
            &[
                "rev-list",
                "--count",
                "--left-right",
                "--end-of-options",
                &range,
            ],
        )
        .unwrap_or_default();
        let mut counts = counts.split_whitespace().map(|n| n.parse().unwrap_or(0));
        let behind = counts.next().unwrap_or(0);
        let ahead = counts.next().unwrap_or(0);
        let count = format!("-n{BRANCH_COMMITS}");
        let range = format!("{trunk}..{name}");
        let commits = git(
            dir,
            &[
                "log",
                &count,
                POINT_FORMAT,
                "--end-of-options",
                &range,
                "--",
            ],
        )
        .map(|text| text.lines().filter_map(parse_point).collect())
        .unwrap_or_default();
        lines.push(BranchLine {
            name: name.clone(),
            ahead,
            behind,
            commits,
        });
    }
    Some(Overview {
        trunk,
        tip,
        branches: lines,
    })
}

/// Whether `name` is a local branch (and not an option in disguise).
fn is_branch(dir: &Path, name: &str) -> bool {
    !name.starts_with('-')
        && git(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .is_some()
}

/// The repository's main line: the local branch the remote's `HEAD` points
/// to (or its remote copy), else `main` or `master`.
fn trunk(dir: &Path) -> Option<String> {
    let exists = |rev: &str| git(dir, &["rev-parse", "--verify", "--quiet", rev]).is_some();
    if let Some(remote) = git(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        let remote = remote.trim();
        let local = remote.strip_prefix("origin/").unwrap_or(remote);
        if exists(&format!("refs/heads/{local}")) {
            return Some(local.to_owned());
        }
        return Some(remote.to_owned());
    }
    ["main", "master"]
        .into_iter()
        .find(|b| exists(&format!("refs/heads/{b}")))
        .map(str::to_owned)
}

/// The fields of a [`Point`], for `git log`.
const POINT_FORMAT: &str = "--format=%H%x1f%h%x1f%s%x1f%an%x1f%cr";

fn point(dir: &Path, rev: &str) -> Option<Point> {
    let text = git(
        dir,
        &["log", "-1", POINT_FORMAT, "--end-of-options", rev, "--"],
    )?;
    parse_point(text.trim_end())
}

fn parse_point(line: &str) -> Option<Point> {
    let mut parts = line.split('\u{1f}');
    Some(Point {
        hash: parts.next()?.to_owned(),
        short: parts.next()?.to_owned(),
        subject: parts.next()?.to_owned(),
        author: parts.next()?.to_owned(),
        age: parts.next()?.to_owned(),
    })
}

/// A file a commit changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// `M`, `A`, `D`, `R`…
    pub status: String,
    pub path: String,
    /// Lines added and removed (`None` for binary files).
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

/// What a commit changed: its files and its patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDiff {
    pub files: Vec<ChangedFile>,
    /// The patch, line by line (cut after [`PATCH_LINES`]).
    pub patch: Vec<String>,
}

/// Lines of patch kept per commit.
pub const PATCH_LINES: usize = 4000;

/// Reads the files and patch of commit `hash`.
pub fn commit_diff(dir: &Path, hash: &str) -> Option<CommitDiff> {
    if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let show = |what: &str| {
        git(
            dir,
            &[
                "show",
                "--no-color",
                "--format=",
                what,
                "--end-of-options",
                hash,
            ],
        )
    };
    let statuses = show("--name-status")?;
    let numstat = show("--numstat").unwrap_or_default();
    let patch = show("-U2").unwrap_or_default();
    Some(CommitDiff {
        files: parse_changes(&statuses, &numstat),
        patch: patch.lines().take(PATCH_LINES).map(str::to_owned).collect(),
    })
}

/// Joins `--name-status` and `--numstat` output, which list the same files
/// in the same order.
fn parse_changes(statuses: &str, numstat: &str) -> Vec<ChangedFile> {
    let counts: Vec<(Option<u32>, Option<u32>)> = numstat
        .lines()
        .map(|line| {
            let mut parts = line.split('\t');
            (
                parts.next().and_then(|n| n.parse().ok()),
                parts.next().and_then(|n| n.parse().ok()),
            )
        })
        .collect();
    statuses
        .lines()
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(n, line)| {
            let mut parts = line.split('\t');
            let status = parts.next().unwrap_or_default();
            // Renames and copies list the old path, then the new one.
            let path = parts.next_back().unwrap_or_default().to_owned();
            let (added, removed) = counts.get(n).copied().unwrap_or_default();
            ChangedFile {
                status: status.chars().next().unwrap_or('M').to_string(),
                path,
                added,
                removed,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_status() {
        let info = parse_status(
            "# branch.oid 0123\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n\
             1 .M N... 100644 100644 100644 aaa bbb src/main.rs\n\
             2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs\told.rs\n\
             ? notes.md\n",
        );
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.ahead_behind, Some((2, 1)));
        assert_eq!(
            info.changes,
            [
                ("M".to_owned(), "src/main.rs".to_owned()),
                ("R".to_owned(), "new name.rs".to_owned()),
                ("?".to_owned(), "notes.md".to_owned()),
            ]
        );
    }

    #[test]
    fn joins_statuses_and_counts() {
        let files = parse_changes(
            "M\tsrc/a.rs\nR087\told.rs\tnew.rs\nA\tlogo.png\n",
            "3\t1\tsrc/a.rs\n0\t2\tnew.rs\n-\t-\tlogo.png\n",
        );
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!((files[0].added, files[0].removed), (Some(3), Some(1)));
        assert_eq!(
            (files[1].status.as_str(), files[1].path.as_str()),
            ("R", "new.rs")
        );
        assert_eq!(files[2].added, None, "binary");
    }

    #[test]
    fn reads_a_real_repository() {
        let dir = tempfile::tempdir().unwrap();
        // Isolated from the developer's own git settings (signing, hooks).
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("git is installed")
        };
        assert!(run(&["init", "-q", "-b", "main"]).status.success());
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        run(&["add", "a.txt"]);
        run(&[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "first",
        ]);
        std::fs::write(dir.path().join("b.txt"), "b").unwrap();

        let info = info(dir.path()).expect("a repository");
        assert_eq!(info.branch.as_deref(), Some("main"));

        run(&["checkout", "-qb", "feature"]);
        for n in 0..2 {
            run(&[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("feature {n}"),
            ]);
        }
        let overview = overview(
            dir.path(),
            &["feature".into(), "gone".into(), "--output=x".into()],
        )
        .unwrap();
        assert_eq!(overview.trunk, "main");
        assert_eq!(overview.branches.len(), 1, "unknown branches are left out");
        assert!(!dir.path().join("x").exists(), "never read as an option");
        let feature = &overview.branches[0];
        assert_eq!((feature.ahead, feature.behind), (2, 0));
        let subjects: Vec<&str> = feature.commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["feature 1", "feature 0"], "newest first");

        let main = String::from_utf8(run(&["rev-parse", "main"]).stdout).unwrap();
        let first = commit_diff(dir.path(), main.trim()).unwrap();
        assert_eq!(first.files.len(), 1);
        assert_eq!(first.files[0].path, "a.txt");
        assert!(first.patch.iter().any(|l| l == "+a"));
        assert_eq!(commit_diff(dir.path(), "--stat"), None, "hashes only");
        assert_eq!(info.changes, [("?".to_owned(), "b.txt".to_owned())]);
    }
}
