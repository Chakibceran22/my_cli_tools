//! Thin libc wrappers for what sysinfo does not expose: kernel task counters,
//! process trees, user names, local wall-clock time and terminal handling.

use std::ffi::{CStr, OsStr};
use std::fs::OpenOptions;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;

/// Cumulative scheduler counters for a task. Counts are kernel `int`s that may wrap.
#[derive(Clone, Copy, Debug)]
pub struct TaskInfo {
    pub threads: i32,
    pub running: i32,
    pub priority: i32,
    pub context_switches: u32,
    pub syscalls: u32,
    pub faults: u32,
    pub pageins: u32,
}

pub fn task_info(pid: u32) -> Option<TaskInfo> {
    let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`, a plain C struct
    // that is valid when zeroed; we only read it when the full struct was written.
    let written = unsafe {
        libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTASKINFO, 0, info.as_mut_ptr().cast(), size)
    };
    if written != size {
        return None;
    }
    // SAFETY: fully initialised above.
    let info = unsafe { info.assume_init() };
    Some(TaskInfo {
        threads: info.pti_threadnum,
        running: info.pti_numrunning,
        priority: info.pti_priority,
        context_switches: info.pti_csw as u32,
        syscalls: info.pti_syscalls_unix.wrapping_add(info.pti_syscalls_mach) as u32,
        faults: info.pti_faults as u32,
        pageins: info.pti_pageins as u32,
    })
}

impl TaskInfo {
    /// Totals for a group of processes; the priority stays the first one's.
    pub fn merge(self, other: TaskInfo) -> TaskInfo {
        TaskInfo {
            threads: self.threads + other.threads,
            running: self.running + other.running,
            priority: self.priority,
            context_switches: self.context_switches.wrapping_add(other.context_switches),
            syscalls: self.syscalls.wrapping_add(other.syscalls),
            faults: self.faults.wrapping_add(other.faults),
            pageins: self.pageins.wrapping_add(other.pageins),
        }
    }
}

/// Direct children of `pid`.
pub fn children(pid: u32) -> Vec<u32> {
    let pid = pid as libc::pid_t;
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::proc_listchildpids(pid, std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Headroom for children started between the two calls.
    let mut pids = vec![0 as libc::pid_t; count as usize + 16];
    let bytes = (pids.len() * size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: the buffer is `bytes` long.
    let count = unsafe { libc::proc_listchildpids(pid, pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(count.max(0) as usize);
    pids.into_iter().filter(|&p| p > 0).map(|p| p as u32).collect()
}

/// `root` and every process below it with its depth, depth first, so each
/// process comes right after its parent. Siblings are ordered by PID.
pub fn process_tree(root: u32) -> Vec<(u32, usize)> {
    const LIMIT: usize = 4096;
    let mut tree = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((pid, depth)) = stack.pop() {
        if tree.len() == LIMIT {
            break;
        }
        tree.push((pid, depth));
        let mut found = children(pid);
        found.sort_unstable();
        stack.extend(found.into_iter().rev().map(|child| (child, depth + 1)));
    }
    tree
}

/// Resolves a uid to its login name.
pub fn user_name(uid: u32) -> Option<String> {
    let mut pwd = MaybeUninit::<libc::passwd>::zeroed();
    let mut buf = [0 as libc::c_char; 1024];
    let mut result = std::ptr::null_mut();
    // SAFETY: every pointer refers to a live local of the advertised size.
    let rc = unsafe { libc::getpwuid_r(uid, pwd.as_mut_ptr(), buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: on success `pw_name` points to a NUL-terminated string inside `buf`.
    let name = unsafe { CStr::from_ptr((*result).pw_name) };
    Some(name.to_string_lossy().into_owned())
}

fn local_tm(unix_secs: u64) -> Option<libc::tm> {
    let time = unix_secs as libc::time_t;
    let mut tm = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both pointers are valid; localtime_r is the re-entrant variant.
    let ok = unsafe { !libc::localtime_r(&time, tm.as_mut_ptr()).is_null() };
    // SAFETY: localtime_r filled `tm` when it returned non-null.
    ok.then(|| unsafe { tm.assume_init() })
}

/// `HH:MM:SS` in local time.
pub fn clock(unix_secs: u64) -> String {
    match local_tm(unix_secs) {
        Some(tm) => format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        None => "--:--:--".into(),
    }
}

/// `HH:MM:SS.mmm` in local time, from epoch milliseconds.
pub fn clock_millis(millis: i64) -> String {
    let secs = millis.div_euclid(1000).max(0) as u64;
    match local_tm(secs) {
        Some(tm) => format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, millis.rem_euclid(1000)),
        None => "--:--:--.---".into(),
    }
}

/// `YYYY-MM-DD HH:MM:SS` in local time.
pub fn date_time(unix_secs: u64) -> String {
    match local_tm(unix_secs) {
        Some(tm) => format!(
            "{}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        None => "unknown".into(),
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Points stdin back at the terminal after a PID was piped in, so keys work.
/// Opens the terminal's real device (named after stdout), because macOS
/// kqueue cannot poll `/dev/tty`, which is what crossterm would fall back to.
pub fn stdin_from_terminal() -> io::Result<()> {
    // SAFETY: ttyname returns a pointer to a static NUL-terminated buffer or null;
    // nothing else calls it concurrently at startup.
    let name = unsafe { libc::ttyname(libc::STDOUT_FILENO) };
    if name.is_null() {
        return Err(io::Error::other("stdout is not a terminal"));
    }
    // SAFETY: checked non-null above.
    let path = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes()).to_owned();
    let tty = OpenOptions::new().read(true).write(true).open(path)?;
    // SAFETY: both descriptors are open; dup2 replaces fd 0 atomically.
    if unsafe { libc::dup2(tty.as_raw_fd(), libc::STDIN_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_process_tree() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let me = std::process::id();
        assert!(children(me).contains(&child.id()));
        let tree = process_tree(me);
        assert_eq!(tree[0], (me, 0));
        assert!(tree.contains(&(child.id(), 1)));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
