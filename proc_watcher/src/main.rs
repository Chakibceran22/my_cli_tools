//! procwatch: a small macOS terminal monitor for a single process.

#[cfg(not(target_os = "macos"))]
compile_error!("procwatch is a macOS tool: it reads process details through libproc.");

mod app;
mod cli;
mod fmt;
mod logs;
mod monitor;
mod json;
mod platform;
mod render;
mod run;
mod streams;
mod tail;
mod theme;
mod ui;

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::app::App;

/// How often followed output is picked up between samples.
const OUTPUT_POLL: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    let command = match cli::parse() {
        Ok(Some(command)) => command,
        Ok(None) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("procwatch: {err}\nRun `procwatch --help` for usage.");
            return ExitCode::from(2);
        }
    };

    let built = match command {
        cli::Command::Run { command, plain: true, .. } => return run::run_plain(&command),
        cli::Command::Run { command, interval, .. } => App::launch(&command, interval),
        cli::Command::Watch(args) => App::attach(args.pid, args.interval).map(|mut app| {
            if args.ignored > 0 {
                app.event(format!("{} more PIDs on stdin were ignored", args.ignored));
            }
            app
        }),
    };
    let mut app = match built {
        Ok(app) => app,
        Err(err) => {
            eprintln!("procwatch: {err}");
            return ExitCode::FAILURE;
        }
    };

    if !io::stdin().is_terminal()
        && let Err(err) = platform::stdin_from_terminal()
    {
        eprintln!("procwatch: cannot read keys from the terminal: {err}");
        return ExitCode::FAILURE;
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    drop(app); // stops a command started by `run`

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("procwatch: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Event loop: samples on the interval, drains followed output, redraws only
/// when something changed, and otherwise sleeps in `event::poll`.
fn run(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    let mut next_sample = Instant::now() + app.interval;
    let mut dirty = true;

    while !app.quit {
        if Instant::now() >= next_sample {
            app.tick();
            next_sample = Instant::now() + app.interval;
            dirty = true;
        }
        dirty |= app.drain_output();

        if dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            dirty = false;
        }

        let mut timeout = next_sample.saturating_duration_since(Instant::now());
        if app.follows_output() {
            timeout = timeout.min(OUTPUT_POLL);
        }
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let interval = app.interval;
                    app.on_key(key);
                    if app.interval != interval {
                        next_sample = Instant::now() + app.interval;
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
    Ok(())
}
