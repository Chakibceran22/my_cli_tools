//! `pw run <command>`: runs a command on a pseudo-terminal that procwatch owns.
//!
//! The command believes it writes to a real terminal, so it keeps its colours
//! and its usual format, whatever that format is. Everything it prints is
//! written, byte for byte, to a capture file that the monitor follows for the
//! command or any of its descendants.
//!
//! - `launch`: the monitor runs the command and shows it (`pw run`).
//! - `run_plain`: the command runs in this terminal as usual, its output also
//!   passed through unchanged; watch it with `pw <pid>` elsewhere (`pw run --plain`).
//!   One thread, asleep in poll(2) until there are keys, output or a signal;
//!   signals reach the loop through a pipe.

use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

/// The capture starts over past this size, so a long-running server cannot fill the disk.
const MAX_CAPTURE: u64 = 64 * 1024 * 1024;
/// How long leftover output is still read once the command has exited, in ms.
const DRAIN_MS: libc::c_int = 100;
/// Signals relayed to the command's process group when procwatch receives them.
const FORWARDED: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT];
/// How long a stopped command gets to shut down cleanly: STOP_POLLS x STOP_POLL.
const STOP_POLLS: u32 = 40;
const STOP_POLL: Duration = Duration::from_millis(50);

/// Where captures live: `~/.local/state/procwatch`.
pub fn capture_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/procwatch"))
}

/// The capture written by the `pw run` whose PID is `tap_pid`.
pub fn capture_path(tap_pid: u32) -> Option<PathBuf> {
    Some(capture_dir()?.join(format!("{tap_pid}.log")))
}

/// `pw run --plain`: the command runs in this terminal as usual, captured for
/// `pw <pid>` in another one. Returns its exit code.
pub fn run_plain(command: &[String]) -> ExitCode {
    match try_run_plain(command) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("procwatch: {err}");
            ExitCode::FAILURE
        }
    }
}

fn try_run_plain(command: &[String]) -> Result<ExitCode, String> {
    // Signals first, so a command that exits immediately is not missed.
    let signals = signal_pipe().map_err(|err| format!("cannot set up signals: {err}"))?;
    let Pty { mut child, master, capture, path } = spawn_on_pty(command)?;

    let raw_mode = RawMode::enable(libc::STDIN_FILENO);
    let mut tap = Tap { master, output: Output::new(capture, true) };
    let result = tap.pump(&mut child, signals.as_raw_fd());
    drop(raw_mode);
    let _ = fs::remove_file(&path);

    let status = result.map_err(|err| format!("relaying `{}`: {err}", command[0]))?;
    Ok(exit_code(status))
}

/// A command started by `pw run` for the monitor to show. Its output goes only
/// to the capture, which the monitor follows. Dropping it stops the command
/// and everything it started, and removes the capture.
pub struct Launched {
    pub child: Child,
    capture: PathBuf,
}

impl Launched {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn capture(&self) -> &Path {
        &self.capture
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        stop_group(&mut self.child);
        let _ = fs::remove_file(&self.capture);
    }
}

/// Starts `command` on a pseudo-terminal, its output relayed to the capture by
/// a thread that sleeps in read(2) until there is some.
pub fn launch(command: &[String]) -> Result<Launched, String> {
    let Pty { child, master, capture, path } = spawn_on_pty(command)?;
    thread::spawn(move || {
        let mut output = Output::new(capture, false);
        let mut buf = [0u8; 16 * 1024];
        while let Ok(read @ 1..) = read_fd(master.as_raw_fd(), &mut buf) {
            output.write(&buf[..read]);
        }
    });
    Ok(Launched { child, capture: path })
}

/// SIGTERM to the command's process group, a moment to shut down cleanly, then
/// SIGKILL for anything left, so nothing outlives procwatch.
fn stop_group(child: &mut Child) {
    // The command is a session leader: its PID is its process group.
    let group = -(child.id() as libc::pid_t);
    // SAFETY: plain kill(2) on our child's process group.
    unsafe { libc::kill(group, libc::SIGTERM) };
    for _ in 0..STOP_POLLS {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        thread::sleep(STOP_POLL);
    }
    // SAFETY: as above.
    unsafe { libc::kill(group, libc::SIGKILL) };
    let _ = child.wait();
}

struct Pty {
    child: Child,
    master: File,
    capture: File,
    path: PathBuf,
}

/// Starts `command` in a new session whose controlling terminal is a fresh
/// pseudo-terminal the size of this one, with a new capture file named after
/// this process.
fn spawn_on_pty(command: &[String]) -> Result<Pty, String> {
    let (program, args) = command.split_first().ok_or("nothing to run")?;
    let dir = capture_dir().ok_or("HOME is not set")?;
    fs::create_dir_all(&dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    remove_stale_captures(&dir);
    let path = dir.join(format!("{}.log", std::process::id()));
    let capture = File::create(&path).map_err(|err| format!("cannot create {}: {err}", path.display()))?;

    let (master, slave) = open_pty(terminal_size(libc::STDOUT_FILENO))?;
    let clone = |fd: &OwnedFd| fd.try_clone().map_err(|err| format!("cannot duplicate the terminal: {err}"));

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::from(clone(&slave)?))
        .stdout(Stdio::from(clone(&slave)?))
        .stderr(Stdio::from(clone(&slave)?));
    // SAFETY: runs between fork and exec, and only makes async-signal-safe calls.
    // A new session with the pty as its controlling terminal, as in any terminal app.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = fs::remove_file(&path);
            return Err(format!("cannot run `{program}`: {err}"));
        }
    };
    Ok(Pty { child, master: File::from(master), capture, path })
}

struct Tap {
    master: File,
    output: Output,
}

impl Tap {
    /// Moves bytes until the command exits: keys to the command, its output to
    /// this terminal and the capture, size changes and signals to the command.
    fn pump(&mut self, child: &mut Child, signals: RawFd) -> io::Result<ExitStatus> {
        let mut buf = [0u8; 16 * 1024];
        let mut stdin_open = true;
        let mut size = None;
        self.sync_size(&mut size);

        loop {
            let mut fds = [
                poll_fd(self.master.as_raw_fd()),
                poll_fd(signals),
                poll_fd(if stdin_open { libc::STDIN_FILENO } else { -1 }),
            ];
            if !wait_for(&mut fds, -1)? {
                continue;
            }

            if fds[1].revents != 0 {
                for signal in drain_signals(signals) {
                    match signal {
                        libc::SIGWINCH => self.sync_size(&mut size),
                        // The command is a session leader: its PID is its process group.
                        s if FORWARDED.contains(&s) => {
                            // SAFETY: plain kill(2) on our child's process group.
                            unsafe { libc::kill(-(child.id() as libc::pid_t), s) };
                        }
                        _ => {}
                    }
                }
                if let Some(status) = child.try_wait()? {
                    self.forward_leftovers(&mut buf);
                    return Ok(status);
                }
            }

            if fds[0].revents != 0 {
                match read_fd(self.master.as_raw_fd(), &mut buf) {
                    Ok(read) if read > 0 => self.output.write(&buf[..read]),
                    // EOF or EIO: every process holding the terminal is gone.
                    _ => return child.wait(),
                }
            }

            if fds[2].revents != 0 {
                match read_fd(libc::STDIN_FILENO, &mut buf) {
                    Ok(read) if read > 0 => self.master.write_all(&buf[..read])?,
                    _ => stdin_open = false,
                }
            }
        }
    }

    /// Output still buffered in the terminal when the command exited.
    fn forward_leftovers(&mut self, buf: &mut [u8]) {
        let mut fds = [poll_fd(self.master.as_raw_fd())];
        while matches!(wait_for(&mut fds, DRAIN_MS), Ok(true)) {
            match read_fd(self.master.as_raw_fd(), buf) {
                Ok(read) if read > 0 => self.output.write(&buf[..read]),
                _ => break,
            }
        }
    }

    /// Gives the command's terminal this terminal's size; the kernel then sends it SIGWINCH.
    fn sync_size(&self, last: &mut Option<(u16, u16)>) {
        let Some(size) = terminal_size(libc::STDOUT_FILENO) else { return };
        if *last != Some((size.ws_row, size.ws_col)) {
            // SAFETY: TIOCSWINSZ reads one winsize.
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size as *const libc::winsize) };
            *last = Some((size.ws_row, size.ws_col));
        }
    }
}

/// The capture file, and with `echo` this terminal too.
struct Output {
    capture: File,
    captured: u64,
    echo: bool,
}

impl Output {
    fn new(capture: File, echo: bool) -> Self {
        Self { capture, captured: 0, echo }
    }

    fn write(&mut self, chunk: &[u8]) {
        if self.echo {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(chunk).and_then(|()| stdout.flush());
        }

        if self.captured + chunk.len() as u64 > MAX_CAPTURE {
            let _ = self.capture.set_len(0).and_then(|()| self.capture.seek(SeekFrom::Start(0)));
            self.captured = 0;
        }
        if self.capture.write_all(chunk).is_ok() {
            self.captured += chunk.len() as u64;
        }
    }
}

fn poll_fd(fd: RawFd) -> libc::pollfd {
    libc::pollfd { fd, events: libc::POLLIN, revents: 0 }
}

/// poll(2) with a timeout in ms (-1 waits forever). `false` on timeout or when a
/// signal interrupted it.
fn wait_for(fds: &mut [libc::pollfd], timeout_ms: libc::c_int) -> io::Result<bool> {
    // SAFETY: `fds` is a valid array of pollfd of the given length.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if ready < 0 {
        let err = io::Error::last_os_error();
        return if err.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(err) };
    }
    Ok(ready > 0)
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: reads at most `buf.len()` bytes into `buf`.
        let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if read >= 0 {
            return Ok(read as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Write end of the signal pipe, for the handler.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

/// Writes the signal number into the pipe, waking the poll loop.
extern "C" fn on_signal(signal: libc::c_int) {
    // SAFETY: only async-signal-safe calls; errno is saved and restored around write(2),
    // and a full pipe (already woken) is fine to ignore.
    unsafe {
        let errno = *libc::__error();
        let byte = signal as u8;
        libc::write(SIGNAL_PIPE.load(Ordering::Relaxed), (&raw const byte).cast(), 1);
        *libc::__error() = errno;
    }
}

/// A non-blocking pipe fed by handlers for SIGCHLD, SIGWINCH and the forwarded
/// signals. Returns the read end.
fn signal_pipe() -> io::Result<OwnedFd> {
    let mut ends = [0 as RawFd; 2];
    // SAFETY: pipe(2) writes two descriptors into the array.
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in ends {
        // SAFETY: plain fcntl on descriptors we just created.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK);
        }
    }
    // The write end lives for the whole process, owned by the handler.
    SIGNAL_PIPE.store(ends[1], Ordering::Relaxed);

    for signal in [libc::SIGCHLD, libc::SIGWINCH].into_iter().chain(FORWARDED) {
        // SAFETY: a zeroed sigaction is valid; the handler is async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = MaybeUninit::zeroed().assume_init();
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    // SAFETY: the read end is ours alone.
    Ok(unsafe { OwnedFd::from_raw_fd(ends[0]) })
}

/// The signal numbers waiting in the pipe.
fn drain_signals(fd: RawFd) -> Vec<libc::c_int> {
    let mut buf = [0u8; 64];
    let mut signals = Vec::new();
    while let Ok(read) = read_fd(fd, &mut buf) {
        if read == 0 {
            break;
        }
        signals.extend(buf[..read].iter().map(|&b| libc::c_int::from(b)));
    }
    signals
}

fn open_pty(size: Option<libc::winsize>) -> Result<(OwnedFd, OwnedFd), String> {
    let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
    let mut size = size;
    let window = size.as_mut().map_or(std::ptr::null_mut(), |s| s as *mut libc::winsize);
    // SAFETY: the out-pointers are valid; name and termios may be null.
    let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), window) };
    if rc != 0 {
        return Err(format!("cannot open a pseudo-terminal: {}", io::Error::last_os_error()));
    }
    // SAFETY: openpty returned two fresh descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // SAFETY: plain fcntl on a descriptor we own. Keeps them out of the command,
        // which gets the slave only as its stdio.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((master, slave))
}

fn terminal_size(fd: RawFd) -> Option<libc::winsize> {
    let mut size = MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: TIOCGWINSZ writes one winsize into the pointer.
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0;
    // SAFETY: filled on success, and valid when zeroed anyway.
    let size = unsafe { size.assume_init() };
    (ok && size.ws_col > 0).then_some(size)
}

/// Puts this terminal in raw mode, so every key (Ctrl-C included) reaches the
/// command's own terminal. Restored on drop. `None` when stdin is not a terminal.
struct RawMode {
    fd: RawFd,
    original: libc::termios,
}

impl RawMode {
    fn enable(fd: RawFd) -> Option<Self> {
        let mut original = MaybeUninit::<libc::termios>::zeroed();
        // SAFETY: tcgetattr fills the struct on success.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: filled above.
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        // SAFETY: both point to valid termios structs.
        let ok = unsafe {
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw) == 0
        };
        ok.then_some(Self { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the settings read in `enable`.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

/// Removes captures whose `pw run` is no longer alive (killed with SIGKILL).
fn remove_stale_captures(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|entry| entry.path()) {
        let pid = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<libc::pid_t>().ok());
        if let Some(pid) = pid
            && path.extension().is_some_and(|ext| ext == "log")
            && !is_alive(pid)
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn is_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    exists || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The command's exit code, or 128 + signal like a shell.
fn exit_code(status: ExitStatus) -> ExitCode {
    let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    ExitCode::from(code.clamp(0, 255) as u8)
}
