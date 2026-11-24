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
    // Tall enough for the CPU panel's core grid: totals, a heading, the grid and stats.
    let charts_height = (core_rows(app.monitor.machine.cores.len()) as u16 + 6).max(11);
    let [header, charts, body, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(charts_height),
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

/// The process's own CPU, the machine's, then every core of the machine.
fn draw_cpu(frame: &mut Frame, app: &App, area: Rect) {
    let monitor = &app.monitor;
    let machine = &monitor.machine;
    let snapshot = monitor.snapshot.as_ref();
    let cpu = snapshot.map_or(0.0, |s| f64::from(s.cpu));
    let spike = snapshot.is_some_and(|s| s.cpu_spike);

    let block = panel("CPU", CPU).title(Line::from(format!(" {cpu:.1}% ").fg(CPU).bold()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [process_row, machine_row, heading, grid, stats] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    // The process, in percent of one core like Activity Monitor, so it can pass
    // 100%. Charted against one full core, or its own peak when higher.
    const LABEL: usize = 8;
    const VALUE: usize = 8;
    let history = &monitor.cpu;
    let width = (process_row.width as usize).saturating_sub(LABEL + VALUE);
    let (points, spikes) = history.tail(width * 2);
    let full = points.iter().copied().max().unwrap_or(0).max(1000) as f64;
    let value_style = if spike { Style::new().fg(RED).bold() } else { Style::new().fg(CPU).bold() };
    let mut spans = vec![format!("{:<LABEL$}", "process").fg(SUBTEXT0)];
    spans.extend(sparkline(points, spikes, full, width, |_, spike| if spike { RED } else { CPU }));
    spans.push(Span::styled(format!("{cpu:>VALUE$.1}%", VALUE = VALUE - 1), value_style));
    frame.render_widget(Paragraph::new(Line::from(spans)), process_row);

    // The whole machine, 0-100% across all cores.
    let total = machine.total as f64 / 10.0;
    let mut spans = vec![format!("{:<LABEL$}", "machine").fg(SUBTEXT0)];
    spans.extend(bar(total / 100.0, width));
    spans.push(format!("{total:>VALUE$.1}%", VALUE = VALUE - 1).fg(load(total / 100.0)));
    frame.render_widget(Paragraph::new(Line::from(spans)), machine_row);

    if !machine.cores.is_empty() {
        let title = format!("{} cores, whole machine ", machine.cores.len());
        let rule = "\u{2500}".repeat((heading.width as usize).saturating_sub(title.chars().count()));
        frame.render_widget(Paragraph::new(Line::from(vec![title.fg(OVERLAY0), rule.fg(SURFACE0)])), heading);
        draw_cores(frame, &machine.cores, grid);
    }

    let mut text = format!(
        "avg {:.1}%  peak {:.1}%",
        history.average() as f64 / 10.0,
        history.peak() as f64 / 10.0,
    );
    if let Some([one, five, fifteen]) = machine.load_average {
        text.push_str(&format!("  load {one:.2} {five:.2} {fifteen:.2}"));
    }
    frame.render_widget(Paragraph::new(text.fg(OVERLAY1)), stats);
}

/// Rows of the core grid when it has room: two columns up to 16 cores, then
/// as many columns as keep it at eight rows.
fn core_rows(cores: usize) -> usize {
    cores.div_ceil(cores.div_ceil(8).max(2))
}

/// Every core as `C3  <history>  42%`, numbered down each column, columns split by a rule.
fn draw_cores(frame: &mut Frame, cores: &[History], area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let columns = cores.len().div_ceil(area.height as usize).max(2).min(cores.len());
    let rows = cores.len().div_ceil(columns);
    let area = Rect { height: area.height.min(rows as u16), ..area };
    let label_width = format!("C{}", cores.len() - 1).len();

    let rects = Layout::horizontal(vec![Constraint::Fill(1); columns]).spacing(3).split(area);
    for (column, rect) in rects.iter().enumerate() {
        if column > 0 {
            let rule = Rect { x: rect.x.saturating_sub(2), width: 1, ..*rect };
            let lines = vec![Line::from("\u{2502}".fg(SURFACE1)); rule.height as usize];
            frame.render_widget(Paragraph::new(lines), rule);
        }
        let lines: Vec<Line> = cores
            .iter()
            .enumerate()
            .skip(column * rows)
            .take(rows)
            .map(|(index, history)| core_line(index, history, label_width, rect.width as usize))
            .collect();
        frame.render_widget(Paragraph::new(lines), *rect);
    }
}

fn core_line(index: usize, history: &History, label_width: usize, width: usize) -> Line<'static> {
    const VALUE: usize = 5;
    let tenths = history.last().unwrap_or(0);
    let ratio = tenths as f64 / 1000.0;
    // Under 1% reads as idle: dim, so the busy cores stand out.
    let value_color = if tenths < 10 { OVERLAY0 } else { load(ratio) };
    let spark_width = width.saturating_sub(label_width + 1 + VALUE);
    let (points, _) = history.tail(spark_width * 2);

    let mut spans = vec![format!("{:<label_width$} ", format!("C{index}")).fg(OVERLAY1)];
    spans.extend(sparkline(points, &[], 1000.0, spark_width, |ratio, _| load(ratio)));
    spans.push(format!("{:>4.0}%", tenths as f64 / 10.0).fg(value_color));
    Line::from(spans)
}

/// One row of braille, two samples per cell, newest on the right. Each sample
/// keeps its bottom dot, so a quiet stretch reads as a dotted baseline, and
/// gains up to three more as `value / full` rises. A cell takes
/// `color(ratio, spike)` of its busier sample; cells under 5% are dimmed.
fn sparkline(
    points: &[u64],
    spikes: &[bool],
    full: f64,
    width: usize,
    color: impl Fn(f64, bool) -> Color,
) -> Vec<Span<'static>> {
    let slots = width * 2;
    let start = points.len().saturating_sub(slots);
    let pad = slots - (points.len() - start);
    let sample = |slot: usize| -> (f64, bool) {
        match slot.checked_sub(pad) {
            Some(i) => ((points[start + i] as f64 / full).clamp(0.0, 1.0), spikes.get(start + i) == Some(&true)),
            None => (0.0, false),
        }
    };

    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_color = None;
    for cell in 0..width {
        let (left, left_spike) = sample(cell * 2);
        let (right, right_spike) = sample(cell * 2 + 1);
        let (ratio, spike) = (left.max(right), left_spike || right_spike);
        let cell_color = if ratio < 0.05 && !spike { SURFACE1 } else { color(ratio, spike) };
        if run_color.is_some_and(|c| c != cell_color) {
            spans.push(std::mem::take(&mut run).fg(run_color.unwrap_or(SURFACE1)));
        }
        run_color = Some(cell_color);
        run.push(braille(dots(left), dots(right)));
    }
    if let Some(c) = run_color {
        spans.push(run.fg(c));
    }
    spans
}

/// Dots in one braille column for a ratio: 1 (the baseline) to 4.
fn dots(ratio: f64) -> usize {
    1 + (ratio * 3.0).round().clamp(0.0, 3.0) as usize
}

/// A braille cell with `left` and `right` columns filled from the bottom.
fn braille(left: usize, right: usize) -> char {
    const LEFT: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
    const RIGHT: [u32; 4] = [0x80, 0x20, 0x10, 0x08];
    let bits: u32 = LEFT[..left.min(4)].iter().chain(&RIGHT[..right.min(4)]).sum();
    char::from_u32(0x2800 + bits).unwrap_or(' ')
}

/// A `width`-cell bar filled to `ratio`, green, yellow or red with the load.
fn bar(ratio: f64, width: usize) -> Vec<Span<'static>> {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    vec!["\u{2501}".repeat(filled).fg(load(ratio)), "\u{2501}".repeat(width - filled).fg(SURFACE0)]
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

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn braille_columns_fill_from_the_bottom() {
        assert_eq!(braille(1, 1), '\u{28c0}');
        assert_eq!(braille(4, 4), '\u{28ff}');
        assert_eq!(braille(4, 1), '\u{28c7}');
        assert_eq!((dots(0.0), dots(0.1), dots(0.5), dots(1.0), dots(7.0)), (1, 1, 3, 4, 4));
    }

    #[test]
    fn sparklines_put_the_newest_on_the_right() {
        let spans = sparkline(&[1000, 1000, 0, 0], &[], 1000.0, 3, |_, _| GREEN);
        // Padding, then (100%, 100%), then (0%, 0%).
        assert_eq!(text(&spans), "\u{28c0}\u{28ff}\u{28c0}");
        assert_eq!(spans.iter().map(|s| s.style.fg).collect::<Vec<_>>(), [Some(SURFACE1), Some(GREEN), Some(SURFACE1)]);
        // Only the last `width * 2` points are drawn.
        assert_eq!(text(&sparkline(&[1000; 10], &[], 1000.0, 2, |_, _| GREEN)), "\u{28ff}\u{28ff}");
        assert!(sparkline(&[500], &[], 1000.0, 0, |_, _| GREEN).is_empty());
        let spiked = sparkline(&[0, 10], &[false, true], 1000.0, 1, |_, spike| if spike { RED } else { GREEN });
        assert_eq!(spiked[0].style.fg, Some(RED));
    }

    #[test]
    fn core_grid_keeps_to_eight_rows() {
        assert_eq!(core_rows(8), 4);
        assert_eq!(core_rows(15), 8);
        assert_eq!(core_rows(16), 8);
        assert_eq!(core_rows(24), 8);
        assert_eq!(core_rows(32), 8);
        assert_eq!(core_rows(1), 1);
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






