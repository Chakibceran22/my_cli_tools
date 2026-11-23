//! Command-line parsing. The only target is a PID, given as an argument or piped on stdin.

use std::io::{IsTerminal, Read};
use std::time::Duration;

pub const USAGE: &str = "\
procwatch - watch one process from your terminal

USAGE:
    procwatch [OPTIONS] <PID>                     watch a running process
    <command that prints a PID> | procwatch [OPTIONS]
    procwatch run [OPTIONS] <COMMAND> [ARGS...]   run a command inside the monitor
    procwatch run --plain <COMMAND> [ARGS...]     run it in this terminal as usual,
                                                  to watch by PID from another one

EXAMPLES:
    procwatch 45291
    pgrep -n node | procwatch
    lsof -ti :3000 | procwatch -i 500
    procwatch run pnpm start:dev
    procwatch run --plain pnpm start:dev   (then: pgrep -n -f dist/main | procwatch)

OPTIONS:
    -i, --interval <MS>   sampling interval in milliseconds (default 1000, min 100)
    -h, --help            print this help

RUN:
    `run` starts the command on a terminal of its own, so it keeps its colours
    and its usual output. The monitor shows that output live, and CPU and memory
    for the command and every process it starts. Quitting stops the command.
    With --plain, the command runs in this terminal exactly as usual instead.

PROCESSES:
    When the watched process has children, a picker opens once: all processes
    as totals, or any single one. `t` reopens it. A chosen process that exits
    is followed to the next one in the tree with the same command line, which
    is how watchers and dev servers restart an app.

OUTPUT OF A PID:
    macOS does not let one program read another program's terminal. For a
    command started with `procwatch run` (or any process it started), the exact
    output is shown, colours included. Otherwise output is shown when it reaches
    a file (`> app.log`, `| tee app.log`, or `*.log` files the process writes),
    with JSON log lines made readable.

KEYS:
    q / Esc     quit                    p / Space   pause sampling
    + / -       slower / faster         s           only spikes in the activity log
    Up / Down   scroll output           PgUp/PgDn   scroll output a page
    g / G       top / follow output     c           clear both logs                t / Tab     choose the process to watch
";

pub enum Command {
    Watch(Args),
    Run { command: Vec<String>, plain: bool, interval: Duration },
}

pub struct Args {
    pub pid: u32,
    pub interval: Duration,
    /// Further PIDs that arrived on stdin and were not used.
    pub ignored: usize,
}

/// Parses `std::env::args`. `Ok(None)` means help was requested.
pub fn parse() -> Result<Option<Command>, String> {
    let mut args = std::env::args().skip(1).peekable();
    if args.next_if(|arg| arg == "run").is_some() {
        let mut plain = false;
        let mut interval_ms: u64 = 1000;
        // Options until the command starts; everything after belongs to the command.
        while let Some(arg) = args.next_if(|arg| arg.starts_with('-')) {
            match arg.as_str() {
                "--plain" => plain = true,
                "-i" | "--interval" => interval_ms = parse_interval(args.next())?,
                "-h" | "--help" => return Ok(None),
                "--" => break,
                other => return Err(format!("unknown option for run: {other}")),
            }
        }
        let command: Vec<String> = args.collect();
        if command.is_empty() {
            return Err("`run` needs a command, e.g. `procwatch run pnpm start:dev`".into());
        }
        return Ok(Some(Command::Run { command, plain, interval: Duration::from_millis(interval_ms) }));
    }
    let mut interval_ms: u64 = 1000;
    let mut pid = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-i" | "--interval" => interval_ms = parse_interval(args.next())?,
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            value if pid.is_some() => return Err(format!("only one PID can be watched, got another: {value}")),
            value => pid = Some(parse_pid(value)?),
        }
    }

    let (pid, ignored) = match pid {
        Some(pid) => (pid, 0),
        None => pid_from_stdin()?,
    };
    Ok(Some(Command::Watch(Args {
        pid,
        interval: Duration::from_millis(interval_ms),
        ignored,
    })))
}

/// Milliseconds, at least 100.
fn parse_interval(value: Option<String>) -> Result<u64, String> {
    let value = value.ok_or("--interval needs a value in milliseconds")?;
    let ms: u64 = value.parse().map_err(|_| format!("invalid interval: {value}"))?;
    Ok(ms.max(100))
}

fn parse_pid(value: &str) -> Result<u32, String> {
    value
        .parse()
        .ok()
        .filter(|&pid| pid > 0)
        .ok_or_else(|| format!("not a PID: {value}"))
}

/// Takes the first PID from piped stdin and counts the rest.
fn pid_from_stdin() -> Result<(u32, usize), String> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err("missing PID".into());
    }
    let mut input = String::new();
    stdin
        .read_to_string(&mut input)
        .map_err(|err| format!("cannot read stdin: {err}"))?;
    let mut tokens = input.split_whitespace();
    let first = tokens.next().ok_or("no PID on stdin")?;
    Ok((parse_pid(first)?, tokens.count()))
}
