//! Rendering. Reads `App`, never mutates it (apart from recording the output height).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, BorderType, Chart, Clear, Dataset, GraphType, LineGauge, Padding, Paragraph, Wrap};

use crate::app::{App, State};
use crate::logs::{Activity, ActivityLine, Sample, Source};
use crate::render::Segment;
use crate::monitor::{Focus, History, Snapshot};
use crate::streams::Sink;
use crate::theme::*;
use crate::{fmt, platform};

const LABEL_WIDTH: usize = 12;
const MIB: f64 = 1024.0 * 1024.0;

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, charts, body, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(11),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [cpu, memory] = Layout::horizontal([Constraint::Fill(1); 2]).spacing(1).areas(charts);
    let [details, logs] = Layout::horizontal([Constraint::Length(46), Constraint::Fill(1)])
        .spacing(1)
        .areas(body);
    let [activity, output] = Layout::vertical([Constraint::Fill(2), Constraint::Fill(3)]).areas(logs);

    draw_header(frame, app, header);
    draw_cpu(frame, app, cpu);
    draw_memory(frame, app, memory);
    draw_details(frame, app, details);
    draw_activity(frame, app, activity);
    draw_output(frame, app, output);
    draw_footer(frame, app, footer);
    draw_picker(frame, app, frame.area());
}

fn panel(title: &str, accent: Color) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(SURFACE1))
        .title(Line::from(vec![" ".into(), title.fg(accent).bold(), " ".into()]))
        .padding(Padding::horizontal(1))
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let (badge, color) = match app.state {
        State::Running if app.monitor.waiting => (" \u{25cc} waiting for restart ", YELLOW),
        State::Running if app.paused => (" \u{2016} paused ", YELLOW),
        State::Running => (" \u{25cf} running ", GREEN),
        State::Gone => (" \u{2715} exited ", RED),
    };
    let block = panel("procwatch", ACCENT).title(Line::from(badge.fg(color).bold()).right_aligned());

    let monitor = &app.monitor;
    let snapshot = monitor.snapshot.as_ref();
    let name = snapshot.map_or("?", |s| s.name.as_str());
    let uptime = snapshot.map_or_else(String::new, |s| fmt::duration(s.run_time));
    let dim = |s: &'static str| s.fg(OVERLAY1);

    let tree = monitor.tree.len();
    let scope = match (&monitor.focus, tree) {
        (_, 0 | 1) => "".into(),
        (Focus::All, n) => format!("   all {n} processes (t)").fg(LAVENDER),
        (Focus::One { .. }, n) => format!("   1 of {n} processes (t)").fg(LAVENDER),
    };
    let summary = Line::from(vec![
        name.to_string().fg(ROSEWATER).bold(),
        dim("   pid "),
        monitor.pid().to_string().fg(TEXT),
        scope,
        dim("   up "),
        uptime.fg(TEXT),
        dim("   every "),
        fmt::interval(app.interval).fg(TEXT),
        format!("   {} cores · {} RAM", monitor.cores, fmt::bytes(monitor.total_memory)).fg(OVERLAY1),
    ]);
    let command = snapshot
        .map(|s| if s.command.is_empty() { s.exe.clone().unwrap_or_default() } else { s.command.clone() })
        .unwrap_or_default();
    let command = Line::from(vec!["$ ".fg(GREEN), command.fg(SUBTEXT0)]);

    frame.render_widget(Paragraph::new(vec![summary, command]).block(block), area);
}

fn draw_cpu(frame: &mut Frame, app: &App, area: Rect) {
    let cpu = app.monitor.snapshot.as_ref().map_or(0.0, |s| f64::from(s.cpu));
    let share = (cpu / (100.0 * app.monitor.cores as f64)).clamp(0.0, 1.0);
    let history = &app.monitor.cpu;

    let block = panel("CPU", CPU).title(Line::from(format!(" {cpu:.1}% ").fg(CPU).bold()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Zero-based, with a quarter of headroom and never below 10%, so idle noise stays flat.
    let bounds = |_low: f64, high: f64| [0.0, (high * 1.25).max(100.0)];
    let percent = |tenths: f64| format!("{:.0}%", tenths / 10.0);
    let gauge = format!("{:>5.1}% of {} cores", share * 100.0, app.monitor.cores);
    let stats = |range: [f64; 2]| {
        format!(
            "avg {:.1}%  peak {:.1}%  chart {}-{}",
            history.average() as f64 / 10.0,
            history.peak() as f64 / 10.0,
            percent(range[0]),
            percent(range[1]),
        )
    };
    draw_series(frame, inner, history, bounds, CPU, (share, gauge), stats);
}

fn draw_memory(frame: &mut Frame, app: &App, area: Rect) {
    let memory = app.monitor.snapshot.as_ref().map_or(0, |s| s.memory);
    let total = app.monitor.total_memory.max(1);
    let share = memory as f64 / total as f64;
    let history = &app.monitor.memory;

    let block = panel("Memory", MEMORY)
        .title(Line::from(format!(" {} ", fmt::bytes(memory)).fg(MEMORY).bold()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Zoomed on the visible range so small changes show, with at least 1% or 1 MiB of padding.
    let bounds = |low: f64, high: f64| {
        let pad = ((high - low) / 4.0).max(high / 100.0).max(MIB);
        [(low - pad).max(0.0), high + pad]
    };
    let gauge = format!("{:>5.1}% of {}", share * 100.0, fmt::bytes(total));
    let stats = |range: [f64; 2]| {
        format!(
            "avg {}  peak {}  chart {}-{}",
            fmt::bytes(history.average()),
            fmt::bytes(history.peak()),
            fmt::bytes(range[0] as u64),
            fmt::bytes(range[1] as u64),
        )
    };
    draw_series(frame, inner, history, bounds, MEMORY, (share, gauge), stats);
}

/// A braille line chart with spikes marked in red, a load gauge, and a stats line.
fn draw_series(
    frame: &mut Frame,
    area: Rect,
    history: &History,
    bounds: impl Fn(f64, f64) -> [f64; 2],
    color: Color,
    (share, gauge_label): (f64, String),
    stats: impl Fn([f64; 2]) -> String,
) {
    let [chart_area, gauge_area, stats_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1), Constraint::Length(1)]).areas(area);

    // Braille gives two points per cell; new samples enter on the right.
    let capacity = (chart_area.width as usize * 2).max(2);
    let (points, spikes) = history.tail(capacity);
    let offset = capacity - points.len();
    let line: Vec<(f64, f64)> = points
        .iter()
        .enumerate()
        .map(|(i, &v)| ((offset + i) as f64, v as f64))
        .collect();
    let marks: Vec<(f64, f64)> = line
        .iter()
        .zip(spikes)
        .filter_map(|(point, &spike)| spike.then_some(*point))
        .collect();

    let low = points.iter().copied().min().unwrap_or(0) as f64;
    let high = points.iter().copied().max().unwrap_or(0) as f64;
    let range = bounds(low, high);

    let chart = Chart::new(vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(color))
            .data(&line),
        Dataset::default()
            .marker(Marker::Dot)
            .graph_type(GraphType::Scatter)
            .style(Style::new().fg(RED).bold())
            .data(&marks),
    ])
    .x_axis(Axis::default().bounds([0.0, (capacity - 1) as f64]))
    .y_axis(Axis::default().bounds(range));
    frame.render_widget(chart, chart_area);

    frame.render_widget(
        LineGauge::default()
            .ratio(share)
            .label(gauge_label.fg(SUBTEXT0))
            .filled_symbol("\u{2501}")
            .unfilled_symbol("\u{2501}")
            .filled_style(Style::new().fg(load(share)))
            .unfilled_style(Style::new().fg(SURFACE0)),
        gauge_area,
    );
    frame.render_widget(Paragraph::new(stats(range).fg(OVERLAY1)), stats_area);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let lines = match &app.monitor.snapshot {
        Some(snapshot) => detail_lines(app, snapshot),
        None => vec![Line::from("waiting for the first sample".fg(OVERLAY1))],
    };
    let block = panel("Details", ACCENT);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

fn detail_lines(app: &App, s: &Snapshot) -> Vec<Line<'static>> {
    let mut lines = vec![
        section("Process"),
        row("PID", s.pid.to_string()),
        row("Parent", s.parent.map_or("-".into(), |p| p.to_string())),
        row("User", s.user.clone().unwrap_or_else(|| "-".into())),
        row("Status", s.status.clone()),
        row("Started", platform::date_time(s.start_time)),
        row("Uptime", fmt::duration(s.run_time)),
    ];
    if let Some(task) = s.task {
        lines.push(row("Threads", format!("{} ({} running)", task.threads, task.running)));
        lines.push(row("Priority", task.priority.to_string()));
    }
    if let Some(files) = s.open_files {
        lines.push(row("Open files", files.to_string()));
    }
    if s.processes > 1 {
        lines.push(row("Processes", format!("{} (figures are totals)", s.processes)));
    }
    if let Some((pid, name, memory)) = &s.heaviest {
        lines.push(row("Heaviest", format!("{name} ({pid}) {}", fmt::bytes(*memory))));
    }
    lines.push(row("Stdout", app.streams.stdout.describe()));
    lines.push(row("Stderr", app.streams.stderr.describe()));
    if let Some(tap) = app.streams.capture {
        lines.push(row("Captured", format!("by pw run (PID {tap})")));
    }

    lines.extend([
        Line::default(),
        section("Activity"),
        row("Disk read", format!("{}/s · {}", fmt::bytes(s.read_rate as u64), fmt::bytes(s.total_read))),
        row("Disk write", format!("{}/s · {}", fmt::bytes(s.write_rate as u64), fmt::bytes(s.total_written))),
        row("Virtual", fmt::bytes(s.virtual_memory)),
    ]);
    if let Some(rate) = s.context_switch_rate {
        lines.push(row("Ctx switch", format!("{}/s", fmt::count(rate))));
    }
    if let Some(rate) = s.syscall_rate {
        lines.push(row("Syscalls", format!("{}/s", fmt::count(rate))));
    }
    if let Some(task) = s.task {
        lines.push(row("Faults", fmt::count(f64::from(task.faults))));
        lines.push(row("Page-ins", fmt::count(f64::from(task.pageins))));
    }

    lines.extend([
        Line::default(),
        section("Paths"),
        row("Exe", s.exe.clone().unwrap_or_else(|| "-".into())),
        row("Cwd", s.cwd.clone().unwrap_or_else(|| "-".into())),
    ]);
    lines
}

fn section(title: &'static str) -> Line<'static> {
    Line::from(title.fg(ACCENT).bold())
}

fn row(label: &str, value: String) -> Line<'static> {
    Line::from(vec![format!("{label:<LABEL_WIDTH$}").fg(OVERLAY1), value.fg(TEXT)])
}

fn draw_activity(frame: &mut Frame, app: &App, area: Rect) {
    let spikes = app
        .activity
        .iter()
        .filter(|l| matches!(&l.activity, Activity::Sample(s) if s.cpu_spike || s.memory_jump))
        .count();
    let mut title = vec![format!(" {spikes} spikes").fg(if spikes > 0 { PEACH } else { OVERLAY1 })];
    if app.spikes_only {
        title.push(" · spikes only".fg(YELLOW));
    }
    title.push(" ".into());
    let block = panel("Activity", ACCENT).title(Line::from(title).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = app
        .activity
        .iter()
        .rev()
        .filter(|line| !app.spikes_only || line.is_notable())
        .take(inner.height as usize)
        .map(activity_line)
        .collect();
    lines.reverse();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn activity_line(line: &ActivityLine) -> Line<'static> {
    let mut spans = vec![platform::clock(line.at).fg(OVERLAY0), "  ".into()];
    match &line.activity {
        Activity::Sample(sample) => spans.extend(sample_spans(sample)),
        Activity::Event(text) => spans.push(format!("\u{25cf} {text}").fg(ACCENT).italic()),
        Activity::Alert(text) => spans.push(format!("\u{25cf} {text}").fg(RED).bold()),
    }
    Line::from(spans)
}

fn sample_spans(s: &Sample) -> Vec<Span<'static>> {
    let cpu_style = if s.cpu_spike { Style::new().fg(RED).bold() } else { Style::new().fg(CPU) };
    let (delta_style, marker) = match (s.memory_jump, s.memory_delta > 0) {
        (true, true) => (Style::new().fg(PEACH).bold(), "  \u{25b2} memory jump"),
        (true, false) => (Style::new().fg(GREEN).bold(), "  \u{25bc} memory freed"),
        _ => (Style::new().fg(OVERLAY0), ""),
    };
    let mut spans = vec![
        "cpu ".fg(OVERLAY1),
        Span::styled(format!("{:>6.1}%", s.cpu), cpu_style),
        "   mem ".fg(OVERLAY1),
        format!("{:>10}", fmt::bytes(s.memory)).fg(MEMORY),
        "  ".into(),
        Span::styled(format!("{:>10}", fmt::signed_bytes(s.memory_delta)), delta_style),
    ];
    if s.cpu_spike {
        spans.push("  \u{25b2} cpu spike".fg(RED).bold());
    }
    if !marker.is_empty() {
        spans.push(Span::styled(marker, delta_style));
    }
    spans
}

fn draw_output(frame: &mut Frame, app: &App, area: Rect) {
    let position = if app.scroll == 0 {
        "following".fg(GREEN)
    } else {
        format!("\u{2191} {} lines", app.scroll).fg(YELLOW)
    };
    let block = panel("Output", ACCENT).title(
        Line::from(vec![format!(" {} lines · ", app.output.len()).fg(OVERLAY1), position, " ".into()])
            .right_aligned(),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    app.output_height.set(height);
    let lines = if app.output.len() == 0 {
        output_hint(app)
    } else {
        output_rows(app, inner.width as usize, height)
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The newest output that fits, hard-wrapped to the panel width.
fn output_rows(app: &App, width: usize, height: usize) -> Vec<Line<'_>> {
    let text_width = width.saturating_sub(2).max(1);
    let end = app.output.len() - app.scroll.min(app.output.len());
    let mut rows = Vec::with_capacity(height);

    'lines: for line in app.output.iter().take(end).rev() {
        let (gutter, base) = match line.source {
            Source::Stdout => ("\u{2502} ".fg(SURFACE2), Style::new().fg(TEXT)),
            Source::Stderr => ("\u{2502} ".fg(RED), Style::new().fg(MAROON)),
        };
        for (i, spans) in wrap(&line.segments, text_width, base).into_iter().enumerate().rev() {
            let lead = if i == 0 { gutter.clone() } else { "  ".into() };
            let mut row = Vec::with_capacity(spans.len() + 1);
            row.push(lead);
            row.extend(spans);
            rows.push(Line::from(row));
            if rows.len() == height {
                break 'lines;
            }
        }
    }
    rows.reverse();
    rows
}

/// Splits styled segments into rows of at most `width` characters; each
/// segment's style is laid over `base`.
fn wrap(segments: &[Segment], width: usize, base: Style) -> Vec<Vec<Span<'_>>> {
    let mut rows = vec![Vec::new()];
    let mut used = 0;
    for segment in segments {
        let style = base.patch(segment.style);
        let mut rest = segment.text.as_str();
        while !rest.is_empty() {
            if used == width {
                rows.push(Vec::new());
                used = 0;
            }
            let (head, tail) = split_chars(rest, width - used);
            used += head.chars().count();
            if let Some(row) = rows.last_mut() {
                row.push(Span::styled(head, style));
            }
            rest = tail;
        }
    }
    rows
}

fn split_chars(text: &str, n: usize) -> (&str, &str) {
    text.char_indices().nth(n).map_or((text, ""), |(i, _)| text.split_at(i))
}

/// Explains, when there is no output to show, where it goes and how to see it.
fn output_hint(app: &App) -> Vec<Line<'static>> {
    let dim = |s: String| Line::from(s.fg(OVERLAY1));
    if !app.streams.follow.is_empty() {
        let mut lines = vec![dim("waiting for new lines in".into())];
        lines.extend(app.streams.follow.iter().map(|(p, _)| Line::from(format!("  {}", p.display()).fg(TEXT))));
        return lines;
    }
    let mut lines = match &app.streams.stdout {
        Sink::Terminal(tty) => vec![
            Line::from(format!("stdout goes straight to the terminal {tty}.").fg(YELLOW)),
            dim("macOS does not let another program read a terminal, so this".into()),
            dim("output cannot be shown here.".into()),
        ],
        Sink::Pipe { reader: Some((pid, name)) } => vec![
            Line::from(format!("stdout goes into a pipe read by {name} (PID {pid}),").fg(YELLOW)),
            dim("which is not writing it to a file procwatch can follow.".into()),
        ],
        Sink::Pipe { reader: None } => vec![Line::from("stdout goes into a pipe with no reader found.".fg(YELLOW))],
        Sink::Null => vec![Line::from("stdout is discarded (/dev/null).".fg(YELLOW))],
        Sink::File(_) | Sink::Other | Sink::Unknown => {
            vec![Line::from("procwatch cannot tell where stdout goes.".fg(YELLOW))]
        }
    };
    lines.extend([
        Line::default(),
        dim("To see its output exactly as printed, colours included, start it".into()),
        dim("through pw run, then watch it from another terminal:".into()),
        Line::default(),
        Line::from("  pw run npm run start:dev".fg(GREEN)),
        Line::from("  pgrep -n node | pw".fg(GREEN)),
    ]);
    lines
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let keys = [
        ("q", if app.launched() { "quit and stop" } else { "quit" }),
        ("p", if app.paused { "resume" } else { "pause" }),
        ("+/-", "interval"),
        ("s", if app.spikes_only { "all samples" } else { "spikes only" }),
        ("\u{2191}\u{2193}", "scroll output"),
        ("g/G", "top/follow"),
        ("c", "clear"),
        ("t", "processes"),
    ];
    let spans: Vec<Span> = keys
        .into_iter()
        .flat_map(|(key, action)| [format!(" {key} ").fg(ACCENT).bold(), format!("{action}  ").fg(OVERLAY1)])
        .collect();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The process picker: "all processes", then the tree with indentation, each
/// with its memory and CPU. The current focus is marked with a dot.
fn draw_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = &app.picker else { return };
    let rows = app.picker_rows();
    let tree = &app.monitor.tree;

    let width = area.width.saturating_sub(8).clamp(20, 120);
    let height = (rows.len() as u16 + 2).min(area.height.saturating_sub(4)).max(3);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    frame.render_widget(Clear, popup);
    let block = panel("Watch which process?", ACCENT).title_bottom(
        Line::from(" \u{2191}\u{2193} choose \u{b7} enter watch \u{b7} esc close ".fg(OVERLAY1)).right_aligned(),
    );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Scroll so the highlighted row stays visible.
    let visible = inner.height as usize;
    let index = rows.iter().position(|row| *row == picker.selected).unwrap_or(0);
    let first = index.saturating_sub(visible.saturating_sub(1));
    let figures_width = 20;
    let label_width = (inner.width as usize).saturating_sub(figures_width + 4);

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(i, row)| {
            let (label, memory, cpu) = match row {
                None => (
                    format!("All processes ({})", tree.len()),
                    tree.iter().map(|e| e.memory).sum(),
                    tree.iter().map(|e| e.cpu).sum(),
                ),
                Some(pid) => match tree.iter().find(|e| e.pid == *pid) {
                    Some(e) => {
                        let branch = if e.depth > 0 { "\u{2514} " } else { "" };
                        let indent = "  ".repeat(e.depth.saturating_sub(1) + usize::from(e.depth > 0));
                        let label = format!("{indent}{branch}{} ({pid})  {}", e.name, short_command(&e.command));
                        (label, e.memory, e.cpu)
                    }
                    None => (format!("PID {pid}"), 0, 0.0),
                },
            };
            let current = match (&app.monitor.focus, row) {
                (Focus::All, None) => true,
                (Focus::One { pid, .. }, Some(row_pid)) => pid == row_pid,
                _ => false,
            };
            let label: String = label.chars().take(label_width).collect();
            let text = format!("{label:<label_width$}  {:>10} {:>7.1}%", fmt::bytes(memory), cpu);
            let style = if i == index { Style::new().bg(SURFACE0).fg(TEXT).bold() } else { Style::new().fg(SUBTEXT0) };
            Line::from(vec![
                if current { "\u{25cf} ".fg(GREEN) } else { "  ".into() },
                Span::styled(text, style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A command line without its program (shown as the name), long absolute
/// paths cut to their last two parts: `node --enable-source-maps …/dist/main`.
fn short_command(command: &str) -> String {
    command
        .split_whitespace()
        .skip(1)
        .map(|arg| {
            let parts: Vec<&str> = arg.rsplitn(3, '/').collect();
            if arg.starts_with('/') && parts.len() == 3 && !parts[2].is_empty() {
                format!("\u{2026}/{}/{}", parts[1], parts[0])
            } else {
                arg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(segments: &[Segment], width: usize) -> Vec<String> {
        wrap(segments, width, Style::new())
            .iter()
            .map(|row| row.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn shortens_command_lines() {
        assert_eq!(short_command("/usr/bin/node --enable-source-maps /Users/me/app/dist/main"), "--enable-source-maps \u{2026}/dist/main");
        assert_eq!(short_command("pnpm start:dev"), "start:dev");
        assert_eq!(short_command("go run ./cmd/server"), "run ./cmd/server");
        assert_eq!(short_command("java -jar /a.jar"), "-jar /a.jar");
    }

    #[test]
    fn wraps_styled_segments_by_characters() {
        let seg = |text: &str, color: Color| Segment { text: text.into(), style: Style::new().fg(color) };
        assert_eq!(rows(&[], 4), [""]);
        assert_eq!(rows(&[seg("abcdef", RED)], 4), ["abcd", "ef"]);
        assert_eq!(rows(&[seg("é→", RED), seg("·xyz", GREEN)], 3), ["é→·", "xyz"]);

        let segments = [seg("ab", RED), seg("cd", GREEN)];
        let wrapped = wrap(&segments, 3, Style::new().fg(TEXT));
        assert_eq!(wrapped[0][1].style.fg, Some(GREEN));
        assert_eq!(wrapped[1][0].content, "d");
        assert_eq!(wrapped[1][0].style.fg, Some(GREEN));
    }
}






