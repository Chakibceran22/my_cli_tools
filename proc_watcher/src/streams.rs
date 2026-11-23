//! Finds where a process's stdout and stderr go, and which files can be
//! followed to show its output:
//!
//! - a `pw run` capture of the process or one of its ancestors: the exact
//!   terminal output, colours included (preferred, nothing else is followed),
//! - stdout/stderr redirected to a file (`> app.log`),
//! - stdout/stderr piped into a process that writes a file (`| tee app.log`),
//! - `*.log` files the process itself has open for writing.
//!
//! A terminal cannot be read by another program, so those are only described.
//! Implemented with macOS `libproc`.

use std::path::PathBuf;

use crate::logs::Source;
use crate::run;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Sink {
    Terminal(String),
    File(PathBuf),
    /// `reader` is the process reading the other end, when one was found.
    Pipe { reader: Option<(u32, String)> },
    Null,
    Other,
    #[default]
    Unknown,
}

impl Sink {
    pub fn describe(&self) -> String {
        match self {
            Sink::Terminal(tty) => format!("terminal {}", tty.trim_start_matches("/dev/")),
            Sink::File(path) => path.display().to_string(),
            Sink::Pipe { reader: Some((pid, name)) } => format!("pipe to {name} ({pid})"),
            Sink::Pipe { reader: None } => "pipe".into(),
            Sink::Null => "/dev/null".into(),
            Sink::Other => "socket or device".into(),
            Sink::Unknown => "unknown".into(),
        }
    }
}

#[derive(Default)]
pub struct Streams {
    pub stdout: Sink,
    pub stderr: Sink,
    /// Files to follow, each with the stream it carries. No duplicates.
    pub follow: Vec<(PathBuf, Source)>,
    /// PID of the `pw run` whose capture is followed, if any.
    pub capture: Option<u32>,
}

pub fn discover(pid: u32) -> Streams {
    let fds = sys::list_fds(pid);
    let stdout = sys::sink(pid, &fds, 1);
    let stderr = sys::sink(pid, &fds, 2);

    if let Some((tap, path)) = find_capture(pid) {
        return Streams { stdout, stderr, follow: vec![(path, Source::Stdout)], capture: Some(tap) };
    }

    let mut follow: Vec<(PathBuf, Source)> = Vec::new();
    let mut add = |path: PathBuf, source: Source| {
        if !follow.iter().any(|(known, _)| *known == path) {
            follow.push((path, source));
        }
    };
    for (sink, source) in [(&stdout, Source::Stdout), (&stderr, Source::Stderr)] {
        match sink {
            Sink::File(path) => add(path.clone(), source),
            Sink::Pipe { reader: Some((reader, _)) } => {
                for path in sys::written_files(*reader, &sys::list_fds(*reader)) {
                    add(path, source);
                }
            }
            _ => {}
        }
    }
    for path in sys::written_files(pid, &fds) {
        if path.extension().is_some_and(|ext| ext == "log") {
            add(path, Source::Stdout);
        }
    }

    Streams { stdout, stderr, follow, capture: None }
}

/// Walks up from `pid` to the first `pw run` that has a capture.
fn find_capture(pid: u32) -> Option<(u32, PathBuf)> {
    let mut current = pid;
    for _ in 0..64 {
        if let Some(path) = run::capture_path(current)
            && path.exists()
            && matches!(sys::process_name(current).as_str(), "procwatch" | "pw")
        {
            return Some((current, path));
        }
        current = sys::parent(current).filter(|&parent| parent > 1)?;
    }
    None
}

mod sys {
    use std::ffi::OsString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    use super::Sink;

    // Not exported by the libc crate; values and layouts from <sys/proc_info.h>.
    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
    const PROC_PIDFDPIPEINFO: libc::c_int = 6;
    const FWRITE: u32 = 0x2;

    #[repr(C)]
    struct ProcFileInfo {
        open_flags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guard_flags: u32,
    }

    #[repr(C)]
    struct VnodeFdInfoWithPath {
        file: ProcFileInfo,
        vnode: libc::vnode_info_path,
    }

    #[repr(C)]
    struct PipeInfo {
        stat: libc::vinfo_stat,
        handle: u64,
        peer_handle: u64,
        status: i32,
        reserved: i32,
    }

    #[repr(C)]
    struct PipeFdInfo {
        file: ProcFileInfo,
        pipe: PipeInfo,
    }

    pub struct Fd {
        fd: i32,
        kind: u32,
    }

    pub fn list_fds(pid: u32) -> Vec<Fd> {
        let pid = pid as libc::c_int;
        let entry = size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer asks only for the size needed.
        let needed = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(needed as usize / entry);
        let capacity = (fds.capacity() * entry) as libc::c_int;
        // SAFETY: the kernel writes at most `capacity` bytes into the vector's buffer.
        let written = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), capacity) };
        if written <= 0 {
            return Vec::new();
        }
        // SAFETY: the first `written` bytes are initialised `proc_fdinfo` records.
        unsafe { fds.set_len(written as usize / entry) };
        fds.into_iter().map(|f| Fd { fd: f.proc_fd, kind: f.proc_fdtype }).collect()
    }

    /// Calls `proc_pidfdinfo`. `T` must be one of the plain C structs above.
    fn fd_info<T>(pid: u32, fd: i32, flavor: libc::c_int) -> Option<T> {
        let mut out = MaybeUninit::<T>::zeroed();
        let size = size_of::<T>() as libc::c_int;
        // SAFETY: the kernel writes at most `size` bytes into `out`.
        let written = unsafe { libc::proc_pidfdinfo(pid as libc::c_int, fd, flavor, out.as_mut_ptr().cast(), size) };
        // SAFETY: the whole struct was written; it is plain integers either way.
        (written == size).then(|| unsafe { out.assume_init() })
    }

    struct Vnode {
        path: PathBuf,
        regular: bool,
        writable: bool,
    }

    fn vnode(pid: u32, fd: i32) -> Option<Vnode> {
        let info: VnodeFdInfoWithPath = fd_info(pid, fd, PROC_PIDFDVNODEPATHINFO)?;
        let bytes: Vec<u8> = info
            .vnode
            .vip_path
            .as_flattened()
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Some(Vnode {
            path: PathBuf::from(OsString::from_vec(bytes)),
            regular: info.vnode.vip_vi.vi_stat.vst_mode & libc::S_IFMT == libc::S_IFREG,
            writable: info.file.open_flags & FWRITE != 0,
        })
    }

    fn pipe(pid: u32, fd: i32) -> Option<PipeInfo> {
        fd_info::<PipeFdInfo>(pid, fd, PROC_PIDFDPIPEINFO).map(|info| info.pipe)
    }

    pub fn sink(pid: u32, fds: &[Fd], fd: i32) -> Sink {
        let Some(entry) = fds.iter().find(|f| f.fd == fd) else {
            return Sink::Unknown;
        };
        match entry.kind as libc::c_int {
            libc::PROX_FDTYPE_VNODE => match vnode(pid, fd) {
                Some(v) if v.path.as_os_str() == "/dev/null" => Sink::Null,
                Some(v) if v.regular => Sink::File(v.path),
                // Byte prefix, not `Path::starts_with`, which compares whole components.
                Some(v) if v.path.as_os_str().as_bytes().starts_with(b"/dev/tty") || v.path.as_os_str() == "/dev/console" => {
                    Sink::Terminal(v.path.display().to_string())
                }
                Some(_) => Sink::Other,
                None => Sink::Unknown,
            },
            libc::PROX_FDTYPE_PIPE => Sink::Pipe {
                reader: pipe(pid, fd).and_then(|p| pipe_reader(pid, p.peer_handle)),
            },
            _ => Sink::Other,
        }
    }

    /// Regular files `pid` has open for writing.
    pub fn written_files(pid: u32, fds: &[Fd]) -> Vec<PathBuf> {
        fds.iter()
            .filter(|f| f.kind as libc::c_int == libc::PROX_FDTYPE_VNODE)
            .filter_map(|f| vnode(pid, f.fd))
            .filter(|v| v.regular && v.writable)
            .map(|v| v.path)
            .collect()
    }

    /// Finds the process holding the read end of a pipe, by scanning every
    /// visible process for a pipe whose handle is our peer. Runs once at startup.
    fn pipe_reader(writer: u32, peer: u64) -> Option<(u32, String)> {
        all_pids()
            .into_iter()
            .filter(|&pid| pid != writer)
            .find(|&pid| {
                list_fds(pid)
                    .iter()
                    .filter(|f| f.kind as libc::c_int == libc::PROX_FDTYPE_PIPE)
                    .any(|f| pipe(pid, f.fd).is_some_and(|p| p.handle == peer))
            })
            .map(|pid| (pid, process_name(pid)))
    }

    fn all_pids() -> Vec<u32> {
        // SAFETY: a null buffer asks only for the count.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Vec::new();
        }
        // Headroom for processes started between the two calls.
        let mut pids = vec![0 as libc::c_int; count as usize + 64];
        let bytes = (pids.len() * size_of::<libc::c_int>()) as libc::c_int;
        // SAFETY: the buffer is `bytes` long.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(count.max(0) as usize);
        pids.into_iter().filter(|&pid| pid > 0).map(|pid| pid as u32).collect()
    }

    pub fn parent(pid: u32) -> Option<u32> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the kernel writes at most `size` bytes into `info`.
        let written =
            unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
        // SAFETY: fully written when the sizes match.
        (written == size).then(|| unsafe { info.assume_init() }.pbi_ppid)
    }

    pub fn process_name(pid: u32) -> String {
        let mut buf = [0u8; 256];
        // SAFETY: the kernel writes at most `buf.len()` bytes.
        let len = unsafe { libc::proc_name(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if len <= 0 {
            return "?".into();
        }
        String::from_utf8_lossy(&buf[..len as usize]).into_owned()
    }
}
