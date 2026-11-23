//! Application state and input handling. No drawing here; see `ui`.

use std::cell::Cell;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::logs::{Activity, ActivityLine, OutputLine, Ring, Sample, Source};
use crate::monitor::{Focus, Monitor, Sampled};
use crate::render::Segment;
use crate::run::{self, Launched};
use crate::streams::{self, Streams};
use crate::{fmt, platform, tail};

/// Steps for `+` / `-`.
const INTERVALS_MS: [u64; 7] = [100, 250, 500, 1_000, 2_000, 5_000, 10_000];
/// About an hour of samples at the default interval.
const ACTIVITY_LINES: usize = 4_000;
const OUTPUT_LINES: usize = 5_000;
/// Most output lines taken per frame, so a chatty process cannot starve input.
const MAX_LINES_PER_DRAIN: usize = 2_000;
/// Log files rotate (daily, by size); look for newly opened ones this often.
const REDISCOVER_EVERY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Gone,
}

pub struct App {
    pub monitor: Monitor,
    pub streams: Streams,
    pub output: Ring<OutputLine>,
    pub activity: Ring<ActivityLine>,
    pub state: State,
    pub paused: bool,
    pub spikes_only: bool,
    pub interval: Duration,
    /// Output lines scrolled up from the newest; 0 follows the tail.
    pub scroll: usize,
    /// Output viewport height at the last draw, for paging.
    pub output_height: Cell<usize>,
    pub quit: bool,
    lines: Receiver<(Source, Vec<Segment>)>,
    tx: Sender<(Source, Vec<Segment>)>,
    last_discovery: Instant,
    /// The command `pw run` started, stopped when the app is dropped.
    launched: Option<Launched>,
    /// The process picker, when open.
    pub picker: Option<Picker>,
    /// The picker opens by itself once, the first time there are child processes.
    picker_offered: bool,
}

/// The process picker: rows are "all processes" then the tree, depth first.
pub struct Picker {
    /// The highlighted row: `None` for "all processes", else a PID.
    pub selected: Option<u32>,
}

impl App {
    /// `pw <pid>`: watches an existing process, finding its output where it can.
    pub fn attach(pid: u32, interval: Duration) -> Result<Self, String> {
        let monitor = Monitor::new(pid, Focus::One { pid, command: String::new() })?;
        let mut app = Self::start(monitor, interval, None, |s| {
            format!("watching {} (PID {pid}), {} resident", s.name, fmt::bytes(s.memory))
        })?;
        app.adopt(streams::discover(pid));
        Ok(app)
    }

    /// `pw run <command>`: starts the command and watches it with everything it starts.
    pub fn launch(command: &[String], interval: Duration) -> Result<Self, String> {
        let launched = run::launch(command)?;
        let pid = launched.pid();
        let monitor = Monitor::new(pid, Focus::All)?;
        let shown = command.join(" ");
        // Its output is known: the capture this process writes.
        let mut streams = streams::discover(pid);
        streams.follow = vec![(launched.capture().to_path_buf(), Source::Stdout)];
        streams.capture = Some(std::process::id());
        let mut app = Self::start(monitor, interval, Some(launched), |_| {
            format!("started `{shown}` (PID {pid}); quitting stops it")
        })?;
        app.adopt(streams);
        Ok(app)
    }

    fn start(
        mut monitor: Monitor,
        interval: Duration,
        launched: Option<Launched>,
        greeting: impl FnOnce(&crate::monitor::Snapshot) -> String,
    ) -> Result<Self, String> {
        let pid = monitor.root();
        if matches!(monitor.sample(), Sampled::Gone) {
            return Err(format!("no process with PID {pid}"));
        }

        let (tx, lines) = mpsc::channel();
        let mut app = Self {
            monitor,
            streams: Streams::default(),
            output: Ring::new(OUTPUT_LINES),
            activity: Ring::new(ACTIVITY_LINES),
            state: State::Running,
            paused: false,
            spikes_only: false,
            interval,
            scroll: 0,
            output_height: Cell::new(0),
            quit: false,
            lines,
            tx,
            last_discovery: Instant::now(),
            launched,
            picker: None,
            picker_offered: false,
        };

        if let Some(text) = app.monitor.snapshot.as_ref().map(greeting) {
            app.event(text);
        }
        Ok(app)
    }

    /// Takes in a fresh view of the streams and starts following any new file.
    fn adopt(&mut self, found: Streams) {
        for (path, source) in found.follow {
            if self.streams.follow.iter().any(|(known, _)| *known == path) {
                continue;
            }
            tail::follow(path.clone(), source, self.tx.clone());
            self.event(format!("following {}", path.display()));
            self.streams.follow.push((path, source));
        }
        self.streams.stdout = found.stdout;
        self.streams.stderr = found.stderr;
        self.streams.capture = found.capture;
    }

    pub fn event(&mut self, text: impl Into<String>) {
        self.log(Activity::Event(text.into()));
    }

    fn log(&mut self, activity: Activity) {
        self.activity.push(ActivityLine { at: platform::now_secs(), activity });
    }

    pub fn follows_output(&self) -> bool {
        !self.streams.follow.is_empty()
    }

    /// Takes one sample unless paused or the process is gone.
    pub fn tick(&mut self) {
        self.reap();
        if self.paused || self.state == State::Gone {
            return;
        }
        // A launched command's output is already followed; only attached ones rotate files.
        if self.launched.is_none() && self.last_discovery.elapsed() >= REDISCOVER_EVERY {
            self.last_discovery = Instant::now();
            self.adopt(streams::discover(self.monitor.root()));
        }
        let threads_before = self.threads();
        let generation = self.monitor.generation;
        let sampled = self.monitor.sample();
        for note in self.monitor.take_notes() {
            self.event(note);
        }
        match sampled {
            Sampled::Gone => {
                self.state = State::Gone;
                self.log(Activity::Alert(format!("PID {} has exited", self.monitor.root())));
                return;
            }
            Sampled::Waiting => return,
            Sampled::Updated => {}
        }
        self.offer_picker();
        if let Some(s) = &self.monitor.snapshot {
            let sample = Sample {
                cpu: s.cpu,
                cpu_spike: s.cpu_spike,
                memory: s.memory,
                memory_delta: s.memory_delta,
                memory_jump: s.memory_jump,
            };
            self.log(Activity::Sample(sample));
        }
        if let (Some(before), Some(after)) = (threads_before, self.threads())
            && before != after
            && generation == self.monitor.generation
        {
            self.event(format!("threads {before} \u{2192} {after}"));
        }
    }

    pub fn launched(&self) -> bool {
        self.launched.is_some()
    }

    /// Opens the picker the first time the watched process has children.
    fn offer_picker(&mut self) {
        if self.picker_offered || self.monitor.tree.len() < 2 {
            return;
        }
        self.picker_offered = true;
        self.open_picker();
        self.event("it runs other processes: choose which to watch (t reopens the list)");
    }

    fn open_picker(&mut self) {
        let selected = match &self.monitor.focus {
            Focus::All => None,
            Focus::One { pid, .. } => Some(*pid),
        };
        self.picker = Some(Picker { selected });
    }

    /// The picker's rows: `None` for "all processes", then every PID of the tree.
    pub fn picker_rows(&self) -> Vec<Option<u32>> {
        std::iter::once(None).chain(self.monitor.tree.iter().map(|e| Some(e.pid))).collect()
    }

    fn picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = &self.picker else { return };
        let rows = self.picker_rows();
        let index = rows.iter().position(|row| *row == picker.selected).unwrap_or(0);
        let moved = |by: isize| rows[index.saturating_add_signed(by).min(rows.len() - 1)];
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.picker = Some(Picker { selected: moved(-1) }),
            KeyCode::Down | KeyCode::Char('j') => self.picker = Some(Picker { selected: moved(1) }),
            KeyCode::Enter => {
                let focus = match rows[index] {
                    None => Focus::All,
                    Some(pid) => {
                        let command = self.monitor.tree.iter().find(|e| e.pid == pid).map(|e| e.command.clone());
                        Focus::One { pid, command: command.unwrap_or_default() }
                    }
                };
                self.picker = None;
                if focus != self.monitor.focus {
                    self.monitor.set_focus(focus);
                    let what = match &self.monitor.focus {
                        Focus::All => format!("all {} processes", self.monitor.tree.len()),
                        Focus::One { pid, .. } => format!("PID {pid}"),
                    };
                    self.event(format!("now watching {what}"));
                    if self.state == State::Running {
                        self.monitor.sample();
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('t') => self.picker = None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    /// Reports how a command started by `pw run` ended.
    fn reap(&mut self) {
        if self.state == State::Gone {
            return;
        }
        let Some(launched) = self.launched.as_mut() else { return };
        if let Ok(Some(status)) = launched.child.try_wait() {
            self.state = State::Gone;
            let text = format!("command ended ({status})");
            self.log(if status.success() { Activity::Event(text) } else { Activity::Alert(text) });
        }
    }

    fn threads(&self) -> Option<i32> {
        self.monitor.snapshot.as_ref()?.task.map(|t| t.threads)
    }

    /// Moves followed output into the scrollback. Returns whether anything arrived.
    pub fn drain_output(&mut self) -> bool {
        let mut received = 0;
        for (source, segments) in self.lines.try_iter().take(MAX_LINES_PER_DRAIN) {
            self.output.push(OutputLine { source, segments });
            received += 1;
        }
        // Keep the same lines on screen while the user is scrolled up.
        if self.scroll > 0 {
            self.scroll = (self.scroll + received).min(self.max_scroll());
        }
        received > 0
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.picker.is_some() {
            self.picker_key(key);
            return;
        }
        let page = self.output_height.get().max(1) as isize;
        match key.code {
            KeyCode::Char('t') | KeyCode::Tab if !self.monitor.tree.is_empty() => self.open_picker(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                self.paused = !self.paused;
                self.event(if self.paused { "sampling paused" } else { "sampling resumed" });
            }
            KeyCode::Char('s') => self.spikes_only = !self.spikes_only,
            KeyCode::Char('+') | KeyCode::Char('=') => self.step_interval(true),
            KeyCode::Char('-') | KeyCode::Char('_') => self.step_interval(false),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(-1),
            KeyCode::PageUp => self.scroll_by(page),
            KeyCode::PageDown => self.scroll_by(-page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = self.max_scroll(),
            KeyCode::End | KeyCode::Char('G') => self.scroll = 0,
            KeyCode::Char('c') => {
                self.output.clear();
                self.activity.clear();
                self.scroll = 0;
            }
            _ => {}
        }
    }

    fn max_scroll(&self) -> usize {
        self.output.len().saturating_sub(self.output_height.get())
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta).min(self.max_scroll());
    }

    fn step_interval(&mut self, slower: bool) {
        let current = self.interval.as_millis() as u64;
        let next = if slower {
            INTERVALS_MS.iter().find(|&&ms| ms > current)
        } else {
            INTERVALS_MS.iter().rev().find(|&&ms| ms < current)
        };
        if let Some(&ms) = next {
            self.interval = Duration::from_millis(ms);
        }
    }
}
