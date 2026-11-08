use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Gauge, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline},
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::net::{Conn, Segment};

const PALETTE: [Color; 8] = [
    Color::Cyan,
    Color::Magenta,
    Color::Green,
    Color::Yellow,
    Color::LightBlue,
    Color::LightRed,
    Color::LightGreen,
    Color::LightMagenta,
];
const MAX_ENTRIES: usize = 2000;
/// A pause longer than this between same-speaker segments starts a new paragraph.
const PARAGRAPH_GAP_SECS: f64 = 4.0;
const DB_FLOOR: f32 = 60.0;

struct Entry {
    speaker: String,
    color: Color,
    start: f64,
    end: f64,
    text: String,
}

pub struct Stats {
    pub sent: u64,
    pub dropped: u64,
}

pub struct App {
    device: String,
    url: String,
    pub conn: Conn,
    server_state: Option<String>,
    entries: VecDeque<Entry>,
    segments: usize,
    words: usize,
    /// Footer message: (text, is_error, when).
    message: Option<(String, bool, Instant)>,
    /// Capture is not sending audio (before the first start, or after a pause).
    pub paused: bool,
    /// Whether capture has ever been started; distinguishes "idle" from "paused".
    activated: bool,
    show_ts: bool,
    started: Instant,
    level: f32,
    peak_db: f32,
    history: VecDeque<u64>,
    // Scrolling: `follow` pins to the bottom; otherwise `top` is the first visible wrapped line.
    follow: bool,
    top: usize,
    total_lines: usize,
    view_height: usize,
}

impl App {
    pub fn new(device: String, url: String, autostart: bool) -> Self {
        Self {
            device,
            url,
            conn: Conn::Connecting,
            server_state: None,
            entries: VecDeque::new(),
            segments: 0,
            words: 0,
            message: None,
            paused: !autostart,
            activated: autostart,
            show_ts: true,
            started: Instant::now(),
            level: 0.0,
            peak_db: -DB_FLOOR,
            history: VecDeque::new(),
            follow: true,
            top: 0,
            total_lines: 0,
            view_height: 0,
        }
    }

    pub fn set_server_state(&mut self, s: String) {
        self.server_state = Some(s);
    }

    pub fn set_error(&mut self, e: String) {
        self.message = Some((e, true, Instant::now()));
    }

    /// The server refused us for good: show why and stop showing retries.
    pub fn set_fatal(&mut self, e: String) {
        self.conn = Conn::Stopped(e.clone());
        self.set_error(e);
    }

    pub fn set_notice(&mut self, m: String) {
        self.message = Some((m, false, Instant::now()));
    }

    /// The transcript as plain text: one `[mm:ss] Speaker N: text` line per paragraph.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            out.push_str(&format!("[{}] {}: {}\n", fmt_ms(e.start), e.speaker, e.text));
        }
        out
    }

    /// Write the transcript to `dir/mimikiku-<UTC timestamp>.txt` and return the path.
    pub fn save(&self, dir: &Path) -> std::io::Result<PathBuf> {
        if self.entries.is_empty() {
            return Err(std::io::Error::other("nothing to save yet"));
        }
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let path = dir.join(format!("mimikiku-{}.txt", fmt_utc_stamp(secs)));
        std::fs::write(&path, self.to_text())?;
        Ok(path)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.segments = 0;
        self.words = 0;
        self.follow = true;
    }

    /// Start, pause or resume capture; returns the new paused state.
    pub fn toggle_capture(&mut self) -> bool {
        self.paused = !self.paused;
        self.activated = true;
        self.paused
    }

    pub fn toggle_timestamps(&mut self) {
        self.show_ts = !self.show_ts;
    }

    pub fn push_segments(&mut self, segs: Vec<Segment>) {
        for seg in segs {
            let text = seg.text.trim();
            if text.is_empty() {
                continue;
            }
            self.segments += 1;
            self.words += text.split_whitespace().count();
            let speaker = seg.speaker_label();
            match self.entries.back_mut() {
                Some(last) if last.speaker == speaker && seg.start - last.end < PARAGRAPH_GAP_SECS => {
                    last.text.push(' ');
                    last.text.push_str(text);
                    last.end = seg.end;
                }
                _ => {
                    self.entries.push_back(Entry {
                        speaker,
                        color: PALETTE[seg.speaker_index() % PALETTE.len()],
                        start: seg.start,
                        end: seg.end,
                        text: text.to_string(),
                    });
                    if self.entries.len() > MAX_ENTRIES {
                        self.entries.pop_front();
                        // Line counts shift; snap back to live rather than show a jumping view.
                        self.follow = true;
                    }
                }
            }
        }
    }

    /// Advance the meter: `rms` is the latest callback RMS, `peak` the peak since last tick.
    pub fn tick(&mut self, rms: f32, peak: f32) {
        let ratio = db_ratio(rms);
        self.level = if ratio > self.level { ratio } else { (self.level - 0.04).max(ratio) };
        self.peak_db = 20.0 * peak.max(1e-6).log10();
        self.history.push_back((ratio * 100.0) as u64);
        while self.history.len() > 400 {
            self.history.pop_front();
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        if self.follow {
            self.follow = false;
            self.top = self.total_lines.saturating_sub(self.view_height);
        }
        self.top = self.top.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        if self.follow {
            return;
        }
        self.top += n;
        if self.top >= self.total_lines.saturating_sub(self.view_height) {
            self.follow = true;
        }
    }

    pub fn scroll_top(&mut self) {
        self.follow = false;
        self.top = 0;
    }

    pub fn scroll_bottom(&mut self) {
        self.follow = true;
    }

    pub fn page(&self) -> usize {
        self.view_height.saturating_sub(2).max(1)
    }

    pub fn draw(&mut self, f: &mut Frame, stats: &Stats) {
        let [header, meter, body, footer] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        self.draw_meter(f, meter);
        self.draw_transcript(f, body);
        self.draw_footer(f, footer, stats);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let (badge, color) = match &self.conn {
            Conn::Connected => ("● connected".to_string(), Color::Green),
            Conn::Connecting => ("◌ connecting…".to_string(), Color::Yellow),
            Conn::Disconnected { reason, retry_in } => {
                (format!("✖ {reason} — retry in {retry_in}s"), Color::Red)
            }
            Conn::Stopped(reason) => (format!("✖ stopped: {reason}"), Color::Red),
        };
        let elapsed = self.started.elapsed().as_secs();
        let mut second = vec![
            Span::styled("mic ", Style::new().dark_gray()),
            Span::raw(self.device.clone()),
        ];
        if let Some(state) = &self.server_state {
            second.push(Span::styled("  server ", Style::new().dark_gray()));
            let c = if state == "ready" || state == "listening" { Color::Green } else { Color::Yellow };
            second.push(Span::styled(state.clone(), Style::new().fg(c)));
        }
        second.push(Span::styled("  up ", Style::new().dark_gray()));
        second.push(Span::raw(fmt_hms(elapsed)));
        let lines = vec![
            Line::from(vec![
                Span::styled(badge, Style::new().fg(color).bold()),
                Span::styled("  ", Style::new()),
                Span::styled(self.url.clone(), Style::new().dark_gray()),
            ]),
            Line::from(second),
        ];
        let block = Block::bordered().title(" mimikiku-capture ".bold().cyan());
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_meter(&self, f: &mut Frame, area: Rect) {
        let title = if !self.activated {
            " Input — idle, press space to start ".cyan().bold()
        } else if self.paused {
            " Input — PAUSED ".yellow().bold()
        } else {
            " Input ".into()
        };
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [gauge_row, spark_row] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);

        let color = if self.paused {
            Color::DarkGray
        } else if self.level > 0.92 {
            Color::Red
        } else if self.level > 0.75 {
            Color::Yellow
        } else {
            Color::Green
        };
        let label = if self.peak_db <= -DB_FLOOR { "-∞ dB".to_string() } else { format!("{:.0} dB", self.peak_db) };
        f.render_widget(
            Gauge::default()
                .ratio(self.level.clamp(0.0, 1.0) as f64)
                .gauge_style(Style::new().fg(color).bg(Color::Black))
                .label(label),
            gauge_row,
        );
        let w = spark_row.width as usize;
        let skip = self.history.len().saturating_sub(w);
        let data: Vec<u64> = self.history.iter().skip(skip).copied().collect();
        f.render_widget(Sparkline::default().data(&data).max(100).style(Style::new().fg(color)), spark_row);
    }

    fn draw_transcript(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(" Transcript ");
        let inner = block.inner(area);
        // Reserve the scrollbar column so wrapping matches what is drawn.
        let text_area = Rect { width: inner.width.saturating_sub(1), ..inner };
        let width = text_area.width as usize;
        let height = text_area.height as usize;

        let mut lines: Vec<Line> = Vec::new();
        for e in &self.entries {
            let mut head = Vec::new();
            if self.show_ts {
                head.push(Span::styled(format!("[{}] ", fmt_ms(e.start)), Style::new().dark_gray()));
            }
            head.push(Span::styled(e.speaker.clone(), Style::new().fg(e.color).add_modifier(Modifier::BOLD)));
            lines.push(Line::from(head));
            for l in wrap(&e.text, width.saturating_sub(2)) {
                lines.push(Line::from(format!("  {l}")));
            }
        }

        self.total_lines = lines.len();
        self.view_height = height;
        let max_top = lines.len().saturating_sub(height);
        if self.follow {
            self.top = max_top;
        }
        self.top = self.top.min(max_top);

        let title = if self.follow {
            " Transcript ".to_string()
        } else {
            format!(" Transcript — ↓ {} more lines (End to follow) ", max_top - self.top)
        };
        f.render_widget(block.title(title), area);

        let visible: Vec<Line> = lines.into_iter().skip(self.top).take(height).collect();
        if visible.is_empty() {
            let hint = match (&self.conn, self.activated) {
                (_, false) => "Press space to start listening.",
                (Conn::Connected, _) => "Listening… transcripts appear here.",
                _ => "Waiting for the server…",
            };
            f.render_widget(Paragraph::new(hint.dark_gray().italic()), text_area);
        } else {
            f.render_widget(Paragraph::new(visible), text_area);
        }
        let mut sb = ScrollbarState::new(max_top).position(self.top);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None),
            area.inner(Margin { vertical: 1, horizontal: 0 }),
            &mut sb,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect, stats: &Stats) {
        let [msg_row, keys_row] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        if let Some((m, is_err, at)) = &self.message
            && at.elapsed() < Duration::from_secs(10)
        {
            let (text, style) = if *is_err {
                (format!(" ⚠ {m}"), Style::new().red())
            } else {
                (format!(" ✓ {m}"), Style::new().green())
            };
            f.render_widget(Paragraph::new(text).style(style), msg_row);
        }
        let key = |k: &'static str, d: &'static str| {
            [Span::styled(k, Style::new().cyan().bold()), Span::styled(format!(" {d}  "), Style::new().dark_gray())]
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, d) in [("q", "quit"), ("space", if self.activated { "pause" } else { "start" }), ("↑↓/PgUp/PgDn", "scroll"), ("End", "follow"), ("t", "timestamps"), ("s", "save"), ("c", "clear")] {
            spans.extend(key(k, d));
        }
        spans.push(Span::styled(
            format!("│ {} segs · {} words · {} sent · {} dropped", self.segments, self.words, stats.sent, stats.dropped),
            Style::new().dark_gray(),
        ));
        f.render_widget(Paragraph::new(Line::from(spans)), keys_row);
    }
}

fn db_ratio(x: f32) -> f32 {
    let db = 20.0 * x.max(1e-6).log10();
    ((db + DB_FLOOR) / DB_FLOOR).clamp(0.0, 1.0)
}

fn fmt_hms(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn fmt_ms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// `YYYYMMDD-HHMMSS` in UTC (civil-from-days, so no date crate is needed).
fn fmt_utc_stamp(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let rem = epoch_secs % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3600, (rem / 60) % 60, rem % 60)
}

/// Greedy word wrap on character counts; over-long words are hard-split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0;
    for word in text.split_whitespace() {
        let mut word = word;
        let mut wlen = word.chars().count();
        while wlen > width {
            if cur_len > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            let split = word.char_indices().nth(width).map(|(i, _)| i).unwrap_or(word.len());
            lines.push(word[..split].to_string());
            word = &word[split..];
            wlen = word.chars().count();
        }
        if cur_len > 0 && cur_len + 1 + wlen > width {
            lines.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if cur_len > 0 {
            cur.push(' ');
            cur_len += 1;
        }
        cur.push_str(word);
        cur_len += wlen;
    }
    if cur_len > 0 {
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert!(wrap("  ", 5).is_empty());
    }

    #[test]
    fn utc_stamp() {
        assert_eq!(fmt_utc_stamp(0), "19700101-000000");
        assert_eq!(fmt_utc_stamp(1_782_000_000), "20260621-000000");
        assert_eq!(fmt_utc_stamp(951_782_400 + 3661), "20000229-010101");
    }

    #[test]
    fn saves_plain_text() {
        let mut app = App::new("mic".into(), "ws://x".into(), true);
        let dir = std::env::temp_dir();
        assert!(app.save(&dir).is_err());
        app.push_segments(vec![Segment {
            start: 65.0,
            end: 66.0,
            speaker: serde_json::json!(2),
            text: " hi there ".into(),
        }]);
        assert_eq!(app.to_text(), "[01:05] Speaker 2: hi there\n");
        let path = app.save(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), app.to_text());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn starts_idle_until_toggled() {
        let mut app = App::new("mic".into(), "ws://x".into(), false);
        assert!(app.paused && !app.activated);
        assert!(!app.toggle_capture());
        assert!(app.activated);
        assert!(app.toggle_capture());
        assert!(!App::new("mic".into(), "ws://x".into(), true).paused);
    }

    #[test]
    fn db_scale() {
        assert_eq!(db_ratio(0.0), 0.0);
        assert!((db_ratio(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn renders_and_merges_speakers() {
        use ratatui::{Terminal, backend::TestBackend};
        let seg = |start: f64, sp: u64, t: &str| Segment {
            start,
            end: start + 1.0,
            speaker: serde_json::json!(sp),
            text: t.into(),
        };
        let mut app = App::new("mic".into(), "ws://x".into(), true);
        app.push_segments(vec![seg(0.0, 0, "hello"), seg(1.5, 0, "there"), seg(3.0, 1, "hi")]);
        assert_eq!(app.entries.len(), 2);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f, &Stats { sent: 0, dropped: 0 })).unwrap();
        let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("hello there") && text.contains("Speaker 1"));
    }
}
