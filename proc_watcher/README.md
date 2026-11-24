# procwatch

A small macOS terminal monitor for one process, identified by its PID. Catppuccin Frappé colours with a
lavender accent; the background is left to the terminal, so it matches your theme exactly.

```bash
./install.sh                        # release build, installed to ~/.local/bin as procwatch and pw
```

Two ways to use it:

```bash
pw run pnpm start:dev               # 1. run a command inside the monitor: one terminal
pw 45291                            # 2. watch a process that is already running, by PID
pgrep -n -f dist/main | pw          #    (PIDs can be piped in)
```

`pw run` starts the command on a terminal of its own, so it keeps its colours and its usual output,
and shows it live in the Output panel. Quitting stops the command and everything it started.
`-i <ms>` sets the sampling interval in both modes.

## Choosing the process

Commands rarely run as one process: `pnpm start:dev` is pnpm, which starts the Nest CLI (compiler
and file watcher), which starts your app. The same goes for nodemon, `air` for Go, Gradle or Spring
devtools, Metro for React Native.

The first time the watched process has children, a picker opens: **All processes** (totals for the
whole tree, with the heaviest named in Details) or any single process in the tree, each with its
memory and CPU. `t` reopens it at any time. Up/Down choose, Enter watches, Esc closes.

When the chosen process exits, procwatch waits for a process in the tree with the same command line
and follows it: this is how watchers and dev servers restart an app, so restarts are followed
without knowing anything about the tool. The header shows `waiting for restart` in between, and
the Activity log records each restart. Choosing another process starts the charts afresh.

## What it shows

- **CPU**: the process's own CPU as a one-line braille history (spikes in red), the whole
  machine's load as a bar, then every core of the machine (`C0`, `C1`, ...) with its own history
  and percentage, so you can see which cores are busy. Average, peak and the load average below.
- **Memory**: a braille line chart with spikes marked in red, a share-of-machine gauge, average,
  peak and the chart's range. It zooms to the recent range, so small changes are visible instead
  of a solid block.
- **Details**: parent, user, status, start time, threads, priority, open files, where stdout and
  stderr go, disk I/O, context switches and syscalls per second, faults, paths.
- **Activity**: one line per sample (CPU, memory, memory change). CPU spikes (up 20 points and
  by half since the last sample) and memory jumps (5% or 8 MiB either way) are highlighted.
  `s` shows only those.
- **Output**: the process's own stdout/stderr, when it can be read (below).

## Seeing the process's output

macOS does not let one program read another program's terminal, so a process's output can only
be shown if it passes through something procwatch can read.

**Exact output, any program: `pw run`.** Run the command inside the monitor (above), or, to keep it
in your terminal exactly as usual and watch it from another one:

```bash
pw run --plain pnpm start:dev       # runs as usual in this terminal: same colours, Ctrl-C works
pgrep -n -f dist/main | pw          # in another terminal: watch it, or any process it started
```

Either way the command gets a pseudo-terminal of its own, so it keeps its colours and its normal
format, whatever that format is. Its output is written, byte for byte, to
`~/.local/state/procwatch/<pid>.log` (restarted past 64 MiB, removed on exit). `pw <pid>` finds that
capture for the PID or any of its ancestors, so restarts in watch mode are followed too. With
`--plain`, the exit code is the command's.

`--plain` is meant to cost nothing: one thread, asleep until there are keys, output or a signal (no
timers, no idle wakeups), about 1.5 MB of memory. Window resizes are passed on, and `kill`, closing
the terminal or Ctrl-C reach the command, which still cleans up the capture.

**Without `pw run`**, output is followed when it reaches a file:

- redirected: `npm run start:dev > /tmp/app.log 2>&1`
- piped through a writer: `npm run start:dev 2>&1 | tee /tmp/app.log`
- any `*.log` file the process itself has open for writing

Most programs drop colours, and some switch to JSON, when they do not write to a terminal. ANSI
colours that do reach a file are kept and drawn with the terminal's own palette. JSON log records
(any object with a `level`) are made readable, e.g.
`[15:39:37.129] INFO: GET /api/actions 200 11 ms · 1.3 KiB → ActionsController.list`, followed by the
other fields, dimmed. New files (daily or size rotation) are picked up every 30 seconds.

The Details panel says where stdout and stderr go, and which `pw run` is captured.

## Keys

| Key | Action |
|---|---|
| `q` / `Esc` | quit (with `pw run`, also stops the command) |
| `p` / `Space` | pause sampling |
| `+` / `-` | slower / faster sampling |
| `s` | only spikes in the activity log |
| `Up` `Down` `PgUp` `PgDn` | scroll output |
| `g` / `G` | top of output / follow |
| `c` | clear both logs |
| `t` / `Tab` | choose the process to watch (Up/Down, Enter, Esc) |

## Notes

- CPU is per core, like Activity Monitor: 250% means two and a half cores busy.
- The per-core rows are the whole machine. macOS does not tell another program which core a
  process ran on (that needs root and kernel tracing), and the scheduler moves threads between
  cores many times a second anyway: one thread at 100% shows up as about 20% on each of four or
  five cores. Compare the busy cores with the process's own figure to judge how much is yours.
- If the PID exits, or is reused by a new process, the badge turns to `exited` and the last
  values stay on screen.
- macOS only: threads, priority, context switches, syscalls, faults and stream detection use `libproc`.
- Dependencies: `ratatui` (crossterm backend only), `sysinfo` (`system` feature only), `libc`.
