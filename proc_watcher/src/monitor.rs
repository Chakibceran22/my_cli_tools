//! Samples a process tree and keeps a short history, with spikes flagged, for
//! the charts. What is charted (the whole tree, or one process in it) is the
//! focus, chosen in the picker.

use std::time::Instant;

use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::platform::{self, TaskInfo};
use crate::render::sanitize;

/// Samples kept per series; more than any chart is wide.
const HISTORY: usize = 1024;
const MIB: u64 = 1024 * 1024;

/// Append-only series that keeps the last `HISTORY` points plus lifetime stats.
#[derive(Default)]
pub struct History {
    points: Vec<u64>,
    spikes: Vec<bool>,
    peak: u64,
    sum: u128,
    count: u64,
}

impl History {
    fn push(&mut self, value: u64, spike: bool) {
        // Trim in batches so pushes stay amortised O(1).
        if self.points.len() >= HISTORY * 2 {
            self.points.drain(..HISTORY);
            self.spikes.drain(..HISTORY);
        }
        self.points.push(value);
        self.spikes.push(spike);
        self.peak = self.peak.max(value);
        self.sum += u128::from(value);
        self.count += 1;
    }

    /// The most recent `n` points and their spike flags, oldest first.
    pub fn tail(&self, n: usize) -> (&[u64], &[bool]) {
        let start = self.points.len().saturating_sub(n);
        (&self.points[start..], &self.spikes[start..])
    }

    fn last(&self) -> Option<u64> {
        self.points.last().copied()
    }

    pub fn peak(&self) -> u64 {
        self.peak
    }

    pub fn average(&self) -> u64 {
        if self.count == 0 { 0 } else { (self.sum / u128::from(self.count)) as u64 }
    }
}

/// CPU (in tenths of a percent) rose at least 20 points, and by half, since the previous sample.
fn is_cpu_spike(previous: Option<u64>, current: u64) -> bool {
    previous.is_some_and(|p| current >= p + (p / 2).max(200))
}

/// Memory moved by at least 5% or 8 MiB since the previous sample.
fn is_memory_jump(previous: Option<u64>, current: u64) -> bool {
    previous.is_some_and(|p| p.abs_diff(current) >= (p / 20).max(8 * MIB))
}

/// What the charts and details follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The root and every process below it, as totals.
    All,
    /// One process. Its command line finds it again when a watcher or dev
    /// server restarts it under a new PID: nothing app-specific is assumed.
    One { pid: u32, command: String },
}

/// One process of the tree, for the picker.
pub struct TreeEntry {
    pub pid: u32,
    pub depth: usize,
    pub name: String,
    pub command: String,
    pub memory: u64,
    pub cpu: f32,
}

pub enum Sampled {
    Updated,
    /// The focused process ended and nothing has replaced it yet.
    Waiting,
    /// The root process is gone (or its PID now belongs to another process).
    Gone,
}

/// Everything shown about the focus at one point in time. For `Focus::All` the
/// figures are totals for the tree; names and paths are the root's.
pub struct Snapshot {
    pub pid: u32,
    pub name: String,
    pub status: String,
    pub parent: Option<u32>,
    pub user: Option<String>,
    pub start_time: u64,
    pub run_time: u64,
    /// Percent of one core; exceeds 100 on multi-threaded work.
    pub cpu: f32,
    pub cpu_spike: bool,
    pub memory: u64,
    pub memory_delta: i64,
    pub memory_jump: bool,
    pub virtual_memory: u64,
    pub read_rate: f64,
    pub write_rate: f64,
    pub total_read: u64,
    pub total_written: u64,
    pub open_files: Option<usize>,
    pub exe: Option<String>,
    pub cwd: Option<String>,
    pub command: String,
    pub task: Option<TaskInfo>,
    pub context_switch_rate: Option<f64>,
    pub syscall_rate: Option<f64>,
    /// Processes counted: 1, or the size of the tree.
    pub processes: usize,
    /// The member using the most memory, when there are several: PID, name, bytes.
    pub heaviest: Option<(u32, String, u64)>,
}

pub struct Monitor {
    sys: System,
    root: Pid,
    root_start: Option<u64>,
    pub focus: Focus,
    /// The root's tree at the last sample, depth first.
    pub tree: Vec<TreeEntry>,
    /// Bumped whenever the focus moves to another process.
    pub generation: u64,
    /// The focused process ended and nothing has replaced it yet.
    pub waiting: bool,
    /// Things worth logging since the last call to `take_notes`.
    notes: Vec<String>,
    pub cores: usize,
    pub total_memory: u64,
    pub snapshot: Option<Snapshot>,
    /// CPU percent in tenths, so the chart keeps one decimal of precision.
    pub cpu: History,
    pub memory: History,
    last_sample: Option<Instant>,
    sampled_generation: u64,
    user_cache: Option<(u32, Option<String>)>,
    /// Processes refreshed at the last sample.
    tracked: Vec<Pid>,
}

impl Monitor {
    /// Watches the tree under `root`, following `focus`.
    pub fn new(root: u32, focus: Focus) -> Result<Self, String> {
        let mut sys = System::new().map_err(|err| format!("cannot read system info: {err}"))?;
        sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        Ok(Self {
            total_memory: sys.total_memory(),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
            sys,
            root: Pid::from_u32(root),
            root_start: None,
            focus,
            tree: Vec::new(),
            generation: 0,
            waiting: false,
            notes: Vec::new(),
            snapshot: None,
            cpu: History::default(),
            memory: History::default(),
            last_sample: None,
            sampled_generation: 0,
            user_cache: None,
            tracked: Vec::new(),
        })
    }

    pub fn root(&self) -> u32 {
        self.root.as_u32()
    }

    /// The PID shown: the focused process, or the root when watching all.
    pub fn pid(&self) -> u32 {
        match &self.focus {
            Focus::All => self.root.as_u32(),
            Focus::One { pid, .. } => *pid,
        }
    }

    /// Switches what the charts follow, starting them afresh.
    pub fn set_focus(&mut self, focus: Focus) {
        if focus == self.focus {
            return;
        }
        self.focus = focus;
        self.generation += 1;
        self.waiting = false;
        self.cpu = History::default();
        self.memory = History::default();
        self.snapshot = None;
    }

    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    pub fn sample(&mut self) -> Sampled {
        // Always for the command line: a just-launched process may not have exec'd yet.
        let refresh = ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_disk_usage()
            .with_user(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always);
        let tree = platform::process_tree(self.root.as_u32());
        let pids: Vec<Pid> = tree.iter().map(|&(pid, _)| Pid::from_u32(pid)).collect();
        // Refreshing last time's processes too drops the ones that died from sysinfo's table.
        let mut to_refresh = pids.clone();
        to_refresh.extend(self.tracked.iter().filter(|pid| !pids.contains(pid)));
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::Some(&to_refresh), true, refresh);
        self.tracked = pids;

        let now = Instant::now();
        let elapsed = self.last_sample.map(|t| now.duration_since(t).as_secs_f64());
        self.last_sample = Some(now);

        let Some(root) = self.sys.process(self.root) else {
            return Sampled::Gone;
        };
        if self.root_start.is_some_and(|start| start != root.start_time()) {
            return Sampled::Gone;
        }
        self.root_start = Some(root.start_time());

        self.tree = tree
            .iter()
            .filter_map(|&(pid, depth)| {
                let p = self.sys.process(Pid::from_u32(pid))?;
                Some(TreeEntry {
                    pid,
                    depth,
                    name: p.name().to_string_lossy().into_owned(),
                    command: command_line(p),
                    memory: p.memory(),
                    cpu: p.cpu_usage(),
                })
            })
            .collect();

        let Some(members) = self.focus_members() else {
            return Sampled::Waiting;
        };
        self.record(&members, elapsed);
        Sampled::Updated
    }

    /// The PIDs the focus covers now, following a restarted process; `None` while waiting.
    fn focus_members(&mut self) -> Option<Vec<u32>> {
        let (pid, command) = match &self.focus {
            Focus::All => return Some(self.tree.iter().map(|e| e.pid).collect()),
            Focus::One { pid, command } => (*pid, command.clone()),
        };
        if let Some(entry) = self.tree.iter().find(|e| e.pid == pid) {
            self.focus = Focus::One { pid, command: entry.command.clone() };
            self.waiting = false;
            return Some(vec![pid]);
        }
        // The newest process in the tree running the same command line took its place.
        let replacement = self
            .tree
            .iter()
            .filter(|e| !command.is_empty() && e.command == command)
            .max_by_key(|e| self.sys.process(Pid::from_u32(e.pid)).map_or(0, |p| p.start_time()))
            .map(|e| e.pid);
        match replacement {
            Some(next) => {
                self.notes.push(format!("PID {pid} restarted as PID {next} (same command)"));
                self.focus = Focus::One { pid: next, command };
                self.generation += 1;
                self.waiting = false;
                Some(vec![next])
            }
            None => {
                if !self.waiting {
                    self.notes.push(format!("PID {pid} ended; waiting for it to start again"));
                }
                self.waiting = true;
                None
            }
        }
    }

    /// Builds the snapshot and pushes the history for `members` (the first is the subject).
    fn record(&mut self, members: &[u32], elapsed: Option<f64>) {
        let subject_pid = if self.focus == Focus::All { self.root } else { Pid::from_u32(members[0]) };
        let Some(subject) = self.sys.process(subject_pid) else { return };

        let uid = subject.user_id().map(|uid| **uid);
        let user = match (uid, &self.user_cache) {
            (Some(uid), Some((cached, name))) if *cached == uid => name.clone(),
            (Some(uid), _) => {
                let name = platform::user_name(uid).or_else(|| Some(uid.to_string()));
                self.user_cache = Some((uid, name.clone()));
                name
            }
            (None, _) => None,
        };

        let processes: Vec<&sysinfo::Process> =
            members.iter().filter_map(|&pid| self.sys.process(Pid::from_u32(pid))).collect();
        let disk_read: u64 = processes.iter().map(|p| p.disk_usage().read_bytes).sum();
        let disk_written: u64 = processes.iter().map(|p| p.disk_usage().written_bytes).sum();
        let total_read: u64 = processes.iter().map(|p| p.disk_usage().total_read_bytes).sum();
        let total_written: u64 = processes.iter().map(|p| p.disk_usage().total_written_bytes).sum();
        let task = processes
            .iter()
            .filter_map(|p| platform::task_info(p.pid().as_u32()))
            .reduce(TaskInfo::merge);
        let heaviest = (processes.len() > 1)
            .then(|| processes.iter().max_by_key(|p| p.memory()))
            .flatten()
            .map(|p| (p.pid().as_u32(), p.name().to_string_lossy().into_owned(), p.memory()));

        let per_sec = |bytes: u64| elapsed.map_or(0.0, |secs| bytes as f64 / secs);
        // Counters from another process (focus moved) are no baseline.
        let previous_task = (self.sampled_generation == self.generation)
            .then(|| self.snapshot.as_ref().and_then(|s| s.task))
            .flatten();
        self.sampled_generation = self.generation;
        // None when the tree lost members since last time: totals went down, not up.
        let counter_rate = |pick: fn(&TaskInfo) -> u32| -> Option<f64> {
            let (now, before, secs) = (task?, previous_task?, elapsed?);
            Some(f64::from(pick(&now).checked_sub(pick(&before))?) / secs)
        };

        let cpu: f32 = processes.iter().map(|p| p.cpu_usage()).sum();
        let cpu_tenths = (cpu * 10.0).round() as u64;
        // The very first CPU reading has no baseline and is always 0; keep it off the chart.
        let cpu_spike = elapsed.is_some() && is_cpu_spike(self.cpu.last(), cpu_tenths);
        let memory: u64 = processes.iter().map(|p| p.memory()).sum();
        let previous_memory = self.memory.last();
        let memory_jump = is_memory_jump(previous_memory, memory);

        let snapshot = Snapshot {
            pid: subject_pid.as_u32(),
            name: subject.name().to_string_lossy().into_owned(),
            status: subject.status().to_string(),
            parent: subject.parent().map(Pid::as_u32),
            user,
            start_time: subject.start_time(),
            run_time: subject.run_time(),
            cpu,
            cpu_spike,
            memory,
            memory_delta: previous_memory.map_or(0, |p| memory as i64 - p as i64),
            memory_jump,
            virtual_memory: processes.iter().map(|p| p.virtual_memory()).sum(),
            read_rate: per_sec(disk_read),
            write_rate: per_sec(disk_written),
            total_read,
            total_written,
            open_files: processes.iter().map(|p| p.open_files()).sum(),
            exe: subject.exe().map(|p| p.display().to_string()),
            cwd: subject.cwd().map(|p| p.display().to_string()),
            command: command_line(subject),
            context_switch_rate: counter_rate(|t| t.context_switches),
            syscall_rate: counter_rate(|t| t.syscalls),
            task,
            processes: processes.len(),
            heaviest,
        };

        if elapsed.is_some() {
            self.cpu.push(cpu_tenths, cpu_spike);
        }
        self.memory.push(memory, memory_jump && snapshot.memory_delta > 0);
        self.snapshot = Some(snapshot);
    }
}

fn command_line(process: &sysinfo::Process) -> String {
    let joined = process.cmd().iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>().join(" ");
    sanitize(joined.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_spikes_need_a_real_jump() {
        assert!(!is_cpu_spike(None, 900));
        assert!(is_cpu_spike(Some(0), 250));
        assert!(!is_cpu_spike(Some(0), 150));
        // 100% -> 140% is +40 points but not +50%.
        assert!(!is_cpu_spike(Some(1000), 1400));
        assert!(is_cpu_spike(Some(1000), 1500));
    }

    #[test]
    fn memory_jumps_both_ways() {
        let mib = |n: u64| n * MIB;
        assert!(!is_memory_jump(Some(mib(132)), mib(133)));
        assert!(is_memory_jump(Some(mib(132)), mib(145)));
        assert!(is_memory_jump(Some(mib(145)), mib(132)));
        // Large processes need 5%, not just 8 MiB.
        assert!(!is_memory_jump(Some(mib(4096)), mib(4106)));
    }
}
