//! Background thread that collects sessions and pushes snapshots to the UI.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use notify::{RecursiveMode, Watcher as _};

use crate::collectors::{self, Collector, Context};
use crate::model::{Session, Usage};
use crate::procs::{Machine, ProcessTable};
use crate::view::{git_branch, repo_dir, repo_of, worktree_of};

/// Collection settings.
#[derive(Debug, Clone)]
pub struct Settings {
    pub root: PathBuf,
    pub exited_window: Duration,
    /// Upper bound between two collections when no file event arrives.
    pub interval: Duration,
}

/// The state of the world at one point in time.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    /// Current git branch of each repository with a session.
    pub branches: HashMap<String, String>,
    /// Quota usage of each signed-in agent account.
    pub usage: Vec<Usage>,
    /// Machine-wide load, and the share taken by agents.
    pub system: SystemLoad,
}

/// The "System" panel: the machine and the agents running on it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemLoad {
    pub machine: Machine,
    /// CPU of every live agent and its child processes, in percent of one core.
    pub agents_cpu: f32,
    pub agents_memory: u64,
}

/// Wait after a file event before collecting, so bursts of writes coalesce.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Starts the monitor thread. It stops when the returned receiver is dropped.
pub fn spawn(settings: Settings) -> Receiver<Snapshot> {
    let (snapshots, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("petsitter-monitor".into())
        .spawn(move || run(&settings, collectors::all(), &snapshots))
        .expect("failed to spawn the monitor thread");
    receiver
}

/// Collects once, synchronously.
pub fn collect_once(settings: &Settings) -> Snapshot {
    let mut procs = ProcessTable::new();
    let mut collectors = collectors::all();
    collect(settings, &mut procs, &mut collectors)
}

fn run(settings: &Settings, mut collectors: Vec<Box<dyn Collector>>, snapshots: &Sender<Snapshot>) {
    let (events, wakeups) = mpsc::channel();
    // File watching only speeds refreshes up; polling keeps working without it.
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|e| !e.kind.is_access()) {
            let _ = events.send(());
        }
    });
    let _watcher = watcher.ok().map(|mut watcher| {
        for path in collectors.iter().flat_map(|c| c.watch_paths()) {
            if path.exists() {
                let _ = watcher.watch(&path, RecursiveMode::Recursive);
            }
        }
        watcher
    });

    let mut procs = ProcessTable::new();
    loop {
        let snapshot = collect(settings, &mut procs, &mut collectors);
        if snapshots.send(snapshot).is_err() {
            return;
        }
        match wakeups.recv_timeout(settings.interval) {
            Ok(()) => {
                let deadline = Instant::now() + DEBOUNCE;
                while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                    if wakeups.recv_timeout(left).is_err() {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The watcher is gone; fall back to plain polling.
            Err(RecvTimeoutError::Disconnected) => thread::sleep(settings.interval),
        }
    }
}

fn collect(
    settings: &Settings,
    procs: &mut ProcessTable,
    collectors: &mut [Box<dyn Collector>],
) -> Snapshot {
    procs.refresh();
    let ctx = Context {
        root: &settings.root,
        procs,
        now: SystemTime::now(),
        exited_window: settings.exited_window,
    };
    let mut sessions: Vec<Session> = collectors
        .iter_mut()
        .flat_map(|c| c.collect(&ctx))
        .collect();
    let usage = collectors
        .iter_mut()
        .filter_map(|c| c.usage(&ctx))
        .collect();

    for session in &mut sessions {
        session.worktree = worktree_of(&session.cwd);
    }

    // Resource use of each live session's process tree.
    let mut counted = HashSet::new();
    let (mut agents_cpu, mut agents_memory) = (0.0, 0);
    for session in &mut sessions {
        let Some(pid) = session.pid else { continue };
        let Some((cpu, memory)) = procs.tree_usage(pid) else {
            continue;
        };
        session.cpu = Some(cpu);
        session.memory = Some(memory);
        // Two sessions can share a process; count each tree once.
        if counted.insert(pid) {
            agents_cpu += cpu;
            agents_memory += memory;
        }
    }
    let system = SystemLoad {
        machine: procs.machine(),
        agents_cpu,
        agents_memory,
    };

    let branches = branches(&settings.root, &sessions);
    Snapshot {
        sessions,
        branches,
        usage,
        system,
    }
}

fn branches(root: &Path, sessions: &[Session]) -> HashMap<String, String> {
    let mut branches = HashMap::new();
    for session in sessions {
        let (repo, _) = repo_of(root, &session.cwd);
        if !branches.contains_key(&repo)
            && let Some(branch) = git_branch(&repo_dir(root, &session.cwd))
        {
            branches.insert(repo, branch);
        }
    }
    branches
}
