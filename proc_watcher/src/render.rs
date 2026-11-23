//! Turns raw output lines into styled segments for the Output panel.
//!
//! - ANSI colours are kept and mapped to the terminal's own palette, so
//!   coloured output looks exactly as it does in the terminal.
//! - JSON log lines (pino, bunyan, winston...) are rendered as readable lines:
//!   `[15:39:37.129] INFO: GET /api/x 200 16.1 ms · 555 B → Handler  key=value`.

use ratatui::style::{Color, Modifier, Style};

use crate::json::{self, Value};
use crate::theme::*;
use crate::{fmt, platform};

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub text: String,
    pub style: Style,
}

/// Collects segments, merging neighbours that share a style.
#[derive(Default)]
struct Builder {
    segments: Vec<Segment>,
}

impl Builder {
    fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        match self.segments.last_mut() {
            Some(last) if last.style == style => last.text.push_str(&text),
            _ => self.segments.push(Segment { text, style }),
        }
    }
}

/// Renders one raw output line.
pub fn line(raw: &[u8]) -> Vec<Segment> {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches(['\n', '\r']);
    // Progress bars redraw with `\r`; only the last state matters.
    let text = text.rsplit('\r').next().unwrap_or_default();

    if text.trim_start().starts_with('{')
        && let Some(record) = json::parse(text)
        && let Some(segments) = log_record(&record)
    {
        return segments;
    }
    ansi(text)
}

/// The line as plain text: escapes and control characters removed, tabs expanded.
pub fn sanitize(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches(['\n', '\r']);
    let text = text.rsplit('\r').next().unwrap_or_default();
    ansi(text).into_iter().map(|s| s.text).collect()
}

/// Splits text on ANSI escapes, applying SGR (colour) sequences and dropping the rest.
fn ansi(text: &str) -> Vec<Segment> {
    let mut out = Builder::default();
    let mut style = Style::new();
    let mut plain = String::new();
    let mut chars = text.chars();

    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let last = chars.find(|&c| {
                        let is_final = ('@'..='~').contains(&c);
                        if !is_final {
                            params.push(c);
                        }
                        is_final
                    });
                    if last == Some('m') {
                        out.push(std::mem::take(&mut plain), style);
                        style = sgr(style, &params);
                    }
                }
                // OSC (titles, hyperlinks): ends with BEL or ESC \.
                Some(']') if chars.find(|&c| c == '\x07' || c == '\x1b') == Some('\x1b') => {
                    chars.next();
                }
                _ => {}
            },
            '\t' => plain.push_str("    "),
            c if c.is_control() => {}
            c => plain.push(c),
        }
    }
    out.push(plain, style);
    out.segments
}

/// Applies one SGR sequence (`ESC [ params m`) to `style`.
fn sgr(mut style: Style, params: &str) -> Style {
    let mut codes = params.split(';').map(|p| p.parse::<u16>().unwrap_or(0));
    while let Some(code) = codes.next() {
        style = match code {
            0 => Style::new(),
            1 => style.add_modifier(Modifier::BOLD),
            2 => style.add_modifier(Modifier::DIM),
            3 => style.add_modifier(Modifier::ITALIC),
            4 => style.add_modifier(Modifier::UNDERLINED),
            7 => style.add_modifier(Modifier::REVERSED),
            9 => style.add_modifier(Modifier::CROSSED_OUT),
            22 => style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style.remove_modifier(Modifier::ITALIC),
            24 => style.remove_modifier(Modifier::UNDERLINED),
            27 => style.remove_modifier(Modifier::REVERSED),
            29 => style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg(ansi_color(code - 30)),
            90..=97 => style.fg(ansi_color(code - 90 + 8)),
            40..=47 => style.bg(ansi_color(code - 40)),
            100..=107 => style.bg(ansi_color(code - 100 + 8)),
            38 => extended_color(&mut codes).map_or(style, |c| style.fg(c)),
            48 => extended_color(&mut codes).map_or(style, |c| style.bg(c)),
            39 => style.fg(Color::Reset),
            49 => style.bg(Color::Reset),
            _ => style,
        };
    }
    style
}

/// `5;n` (256 colours) or `2;r;g;b` (true colour), after a 38 or 48.
fn extended_color(codes: &mut impl Iterator<Item = u16>) -> Option<Color> {
    match codes.next()? {
        5 => Some(Color::Indexed(codes.next()? as u8)),
        2 => Some(Color::Rgb(codes.next()? as u8, codes.next()? as u8, codes.next()? as u8)),
        _ => None,
    }
}

/// The 16 ANSI colours, left to the terminal's palette so they match it exactly.
fn ansi_color(index: u16) -> Color {
    const PALETTE: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    PALETTE[usize::from(index) % 16]
}

/// Fields shown in a line's head, or too noisy to repeat in its tail.
const HEAD_FIELDS: [&str; 24] = [
    "level", "time", "timestamp", "msg", "message", "context", "method", "route", "url", "path", "status",
    "statusCode", "ms", "responseTime", "bytes", "handler", "err", "error", "event", "service", "pid", "hostname",
    "ua", "userAgent",
];

/// Renders a structured log record. `None` when the object has no `level`,
/// so ordinary JSON output is left as it is.
fn log_record(record: &Value) -> Option<Vec<Segment>> {
    let fields = record.as_object()?;
    let level = record.get("level")?;
    let mut out = Builder::default();
    let text = |name: &str| record.get(name).map(Value::text);

    if let Some(time) = record.get("time").or(record.get("timestamp")).and_then(timestamp) {
        out.push(format!("[{time}] "), Style::new());
    }
    let (label, color) = level_label(level);
    out.push(format!("{label}: "), Style::new().fg(color));
    if let Some(context) = text("context") {
        out.push(format!("[{context}] "), Style::new().fg(YELLOW));
    }

    let method = text("method");
    if let Some(method) = &method {
        out.push(format!("{method} "), Style::new().fg(method_color(method)));
        if let Some(target) = text("route").or(text("url")).or(text("path")) {
            out.push(format!("{target} "), Style::new());
        }
        if let Some(status) = text("status").or(text("statusCode")) {
            out.push(format!("{status} "), Style::new().fg(status_color(&status)));
        }
        let mut figures = Vec::new();
        if let Some(ms) = text("ms").or(text("responseTime")) {
            figures.push(format!("{ms} ms"));
        }
        if let Some(bytes) = text("bytes") {
            figures.push(bytes.parse().map_or(bytes, fmt::bytes));
        }
        out.push(figures.join(" · "), Style::new().fg(SUBTEXT0));
        if let Some(handler) = text("handler") {
            out.push(format!(" \u{2192} {handler}"), Style::new().fg(LAVENDER));
        }
    }

    match text("event") {
        Some(event) if method.is_none() || event != "request" => {
            out.push(format!("{event} "), Style::new().fg(LAVENDER));
        }
        _ => {}
    }
    if let Some(message) = text("msg").or(text("message")) {
        out.push(message, Style::new());
    }
    if let Some(error) = record.get("err").or(record.get("error")) {
        let message = error.get("message").or(error.get("type")).unwrap_or(error).text();
        out.push(format!(" {message}"), Style::new().fg(RED));
    }

    for (key, value) in fields {
        if HEAD_FIELDS.contains(&key.as_str()) || *value == Value::Null {
            continue;
        }
        out.push(format!("  {key}="), Style::new().fg(OVERLAY0));
        out.push(shorten(value.text()), Style::new().fg(OVERLAY1));
    }
    Some(out.segments)
}

fn level_label(level: &Value) -> (String, Color) {
    let label = match level {
        Value::Number(n) => match n.parse::<u32>().unwrap_or(30) {
            0..=10 => "TRACE",
            11..=20 => "DEBUG",
            21..=30 => "INFO",
            31..=40 => "WARN",
            41..=50 => "ERROR",
            _ => "FATAL",
        }
        .to_string(),
        other => other.text().to_uppercase(),
    };
    let color = match label.as_str() {
        "TRACE" => OVERLAY1,
        "DEBUG" => BLUE,
        "INFO" => GREEN,
        "WARN" | "WARNING" => YELLOW,
        _ => RED,
    };
    (label, color)
}

fn method_color(method: &str) -> Color {
    match method {
        "GET" => GREEN,
        "POST" => YELLOW,
        "PUT" => BLUE,
        "PATCH" => PINK,
        "DELETE" => RED,
        _ => TEXT,
    }
}

fn status_color(status: &str) -> Color {
    match status.as_bytes().first() {
        Some(b'2') => GREEN,
        Some(b'3') => TEAL,
        Some(b'4') => YELLOW,
        Some(b'5') => RED,
        _ => TEXT,
    }
}

/// UUIDs to their first 8 characters, other long values cut at 48.
fn shorten(value: String) -> String {
    const MAX: usize = 48;
    let bytes = value.as_bytes();
    let is_uuid = value.len() == 36 && [8, 13, 18, 23].iter().all(|&i| bytes[i] == b'-');
    if is_uuid {
        return value[..8].to_string();
    }
    match value.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}\u{2026}", &value[..cut]),
        None => value,
    }
}

/// `HH:MM:SS.mmm` in local time, from epoch milliseconds or an ISO-8601 string.
fn timestamp(value: &Value) -> Option<String> {
    let millis = match value {
        Value::Number(n) => {
            let n: f64 = n.parse().ok()?;
            // Seconds or milliseconds since the epoch.
            if n < 1e11 { (n * 1000.0) as i64 } else { n as i64 }
        }
        Value::String(s) => iso_millis(s)?,
        _ => return None,
    };
    Some(platform::clock_millis(millis))
}

/// Parses `2026-11-23T14:39:37.129Z` (or with a `+01:00` offset) to epoch milliseconds.
fn iso_millis(s: &str) -> Option<i64> {
    let num = |from: usize, to: usize| s.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);

    let mut rest = s.get(19..)?;
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        let kept = &fraction[..digits.min(3)];
        millis = kept.parse::<i64>().unwrap_or(0) * 10_i64.pow(3 - kept.len() as u32);
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        zone => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone.get(1..3)?.parse().ok()?;
            let minutes: i64 = zone.get(zone.len() - 2..)?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(seconds * 1000 + millis)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(segments: &[Segment]) -> String {
        segments.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn sanitize_strips_escapes_and_controls() {
        assert_eq!(sanitize(b"\x1b[1;32mok\x1b[0m done\r\n"), "ok done");
        assert_eq!(sanitize(b"10%\r50%\r100%\n"), "100%");
        assert_eq!(sanitize(b"\x1b]0;title\x07a\tb"), "a    b");
        assert_eq!(sanitize(b"\x1b]8;;http://x\x1b\\link"), "link");
        assert_eq!(sanitize(b"a\xffb"), "a\u{fffd}b");
    }

    #[test]
    fn keeps_ansi_colours() {
        let segments = line(b"\x1b[32mINFO\x1b[39m: \x1b[1;38;2;1;2;3mhi\x1b[0m!");
        assert_eq!(plain(&segments), "INFO: hi!");
        assert_eq!(segments[0].style, Style::new().fg(Color::Green));
        assert_eq!(segments[1].style, Style::new().fg(Color::Reset));
        assert_eq!(segments[2].style, Style::new().fg(Color::Rgb(1, 2, 3)).add_modifier(Modifier::BOLD));
        assert_eq!(segments[3].style, Style::new());
    }

    #[test]
    fn renders_request_records() {
        let raw = br#"{"level":"info","time":"2026-11-23T14:39:37.129Z","service":"api","event":"request","reqId":"ca5a7304-f162-43ab-a5dc-4d465d971f1d","method":"GET","route":"/api/portal/:token","handler":"PortalController.context","status":200,"ms":16.1,"bytes":555,"user":null,"ip":"127.0.0.1"}"#;
        let text = plain(&line(raw));
        let time = platform::clock_millis(iso_millis("2026-11-23T14:39:37.129Z").unwrap());
        assert_eq!(
            text,
            format!("[{time}] INFO: GET /api/portal/:token 200 16.1 ms · 555 B \u{2192} PortalController.context  reqId=ca5a7304  ip=127.0.0.1")
        );
    }

    #[test]
    fn renders_message_records() {
        let raw = br#"{"level":50,"time":1700000000000,"context":"Database","msg":"lost connection","err":{"type":"Error","message":"ECONNRESET"}}"#;
        let text = plain(&line(raw));
        assert!(text.ends_with("ERROR: [Database] lost connection ECONNRESET"), "{text}");
    }

    #[test]
    fn leaves_other_json_alone() {
        assert_eq!(plain(&line(br#"{"a":1}"#)), r#"{"a":1}"#);
    }

    #[test]
    fn parses_iso_times() {
        assert_eq!(iso_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_millis("2026-11-23T14:39:37.129Z"), Some(1_795_444_777_129));
        assert_eq!(iso_millis("2026-11-23T15:39:37.1+01:00"), Some(1_795_444_777_100));
    }
}
