//! A snapshot of the processes that may belong to coding agents.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::model::RexTarget;

/// Information about one running process.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Process start time, in seconds since the Unix epoch.
    pub start_time: u64,
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub rex: Option<RexTarget>,
    /// CPU use in percent of one core.
    pub cpu: f32,
    /// Resident memory in bytes.
    pub memory: u64,
}

impl ProcInfo {
    /// Basename of the executable (`argv[0]`).
    pub fn program(&self) -> &str {
        self.cmd
            .first()
            .and_then(|arg| Path::new(arg).file_name())
            .and_then(OsStr::to_str)
            .unwrap_or_default()
    }

    /// Whether any of the arguments after the program equals `arg`.
    pub fn has_arg(&self, arg: &str) -> bool {
        self.cmd.iter().skip(1).any(|a| a == arg)
    }
}

/// Machine-wide load.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Machine {
    /// Average CPU use across all cores, in percent.
    pub cpu: f32,
    pub cores: usize,
    /// CPU use of each core, in percent.
    pub per_core: Vec<f32>,
    pub memory_used: u64,
    pub memory_total: u64,
    pub memory_available: u64,
    pub memory_free: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// Load averages over 1, 5 and 15 minutes, when the platform reports them.
    pub load: Option<(f64, f64, f64)>,
    /// Seconds since boot.
    pub uptime: Option<u64>,
    /// CPU model, e.g. "Apple M3 Pro".
    pub brand: Option<String>,
}

/// Process table, refreshed once per collection cycle.
#[derive(Debug)]
pub struct ProcessTable {
    /// `None` when process information is unavailable on this platform.
    system: Option<System>,
    procs: HashMap<u32, ProcInfo>,
    children: HashMap<u32, Vec<u32>>,
}

impl Default for ProcessTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessTable {
    pub fn new() -> Self {
        Self {
            system: System::new().ok(),
            procs: HashMap::new(),
            children: HashMap::new(),
        }
    }

    /// Re-reads the process list.
    pub fn refresh(&mut self) {
        let kind = ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet)
            .with_environ(UpdateKind::OnlyIfNotSet)
            .with_cpu()
            .with_memory();
        let Some(system) = &mut self.system else {
            return;
        };
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);

        self.procs = system
            .processes()
            .iter()
            .map(|(pid, p)| {
                let cmd: Vec<String> = p
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                let info = ProcInfo {
                    pid: pid.as_u32(),
                    parent: p.parent().map(Pid::as_u32),
                    start_time: p.start_time(),
                    cwd: p.cwd().map(Path::to_path_buf),
                    rex: rex_target(p.environ()),
                    cpu: p.cpu_usage(),
                    memory: p.memory(),
                    cmd,
                };
                (info.pid, info)
            })
            .collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in self.procs.values() {
            if let Some(parent) = p.parent {
                children.entry(parent).or_default().push(p.pid);
            }
        }
        self.children = children;
    }

    /// CPU and memory of a process and all its descendants.
    pub fn tree_usage(&self, pid: u32) -> Option<(f32, u64)> {
        self.procs.get(&pid)?;
        let (mut cpu, mut memory) = (0.0, 0);
        let mut stack = vec![pid];
        let mut seen = std::collections::HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(p) = self.procs.get(&pid) {
                cpu += p.cpu;
                memory += p.memory;
            }
            stack.extend(self.children.get(&pid).into_iter().flatten());
        }
        Some((cpu, memory))
    }

    /// Machine-wide CPU, memory and load.
    pub fn machine(&mut self) -> Machine {
        let Some(system) = &mut self.system else {
            return Machine::default();
        };
        system.refresh_cpu_usage();
        system.refresh_memory();
        Machine {
            cpu: system.global_cpu_usage(),
            cores: system.cpus().len(),
            per_core: system.cpus().iter().map(sysinfo::Cpu::usage).collect(),
            memory_used: system.used_memory(),
            memory_total: system.total_memory(),
            memory_available: system.available_memory(),
            memory_free: system.free_memory(),
            swap_used: system.used_swap(),
            swap_total: system.total_swap(),
            load: System::load_average()
                .ok()
                .map(|l| (l.one, l.five, l.fifteen)),
            uptime: System::uptime().ok(),
            brand: system
                .cpus()
                .first()
                .map(|c| c.brand().trim().to_owned())
                .filter(|b| !b.is_empty()),
        }
    }

    /// The direct children of `pid`.
    pub fn children(&self, pid: u32) -> impl Iterator<Item = &ProcInfo> {
        self.children
            .get(&pid)
            .into_iter()
            .flatten()
            .filter_map(|child| self.procs.get(child))
    }

    pub fn get(&self, pid: u32) -> Option<&ProcInfo> {
        self.procs.get(&pid)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ProcInfo> {
        self.procs.values()
    }

    /// Finds the closest ancestor (or the process itself) attached to a Rex pane.
    pub fn rex_target(&self, pid: u32) -> Option<RexTarget> {
        let mut current = Some(pid);
        for _ in 0..16 {
            let proc = self.procs.get(&current?)?;
            if let Some(target) = &proc.rex {
                return Some(target.clone());
            }
            current = proc.parent;
        }
        None
    }
}

fn rex_target(environ: &[std::ffi::OsString]) -> Option<RexTarget> {
    let mut block = None;
    let mut session = None;
    for var in environ {
        let var = var.to_string_lossy();
        if let Some(v) = var.strip_prefix("REX_BLOCK=") {
            block = Some(v.to_owned());
        } else if let Some(v) = var.strip_prefix("REX_SESSION=") {
            session = Some(v.to_owned());
        }
    }
    block
        .filter(|b| !b.is_empty())
        .map(|block| RexTarget { session, block })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rex_environment() {
        let env = ["PATH=/bin", "REX_SESSION=session:1", "REX_BLOCK=block:2"].map(Into::into);
        assert_eq!(
            rex_target(&env),
            Some(RexTarget {
                session: Some("session:1".into()),
                block: "block:2".into()
            })
        );
        assert_eq!(rex_target(&["PATH=/bin".into()]), None);
    }
}
