//! Rendering. A dense panel grid with vendor-accurate numbers, tuned for a dark terminal.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::model::{Health, Report};

pub struct App {
    pub reports: Vec<Report>,
    /// Observed samples per (provider, window). These are our own readings of vendor
    /// numbers, so the sparkline never invents history.
    pub history: HashMap<(String, String), VecDeque<f64>>,
    pub last_refresh: Option<Instant>,
    pub refreshing: bool,
    pub paused: bool,
    pub interval_secs: u64,
}

impl App {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            reports: Vec::new(),
            history: HashMap::new(),
            last_refresh: None,
            refreshing: false,
            paused: false,
            interval_secs,
        }
    }

    pub fn absorb(&mut self, reports: Vec<Report>) {
        let previous: HashMap<(String, Option<String>), Report> = self
            .reports
            .iter()
            .map(|report| {
                (
                    (report.provider.to_string(), report.account.clone()),
                    report.clone(),
                )
            })
            .collect();
        let last_good = self.last_refresh.unwrap_or_else(Instant::now);

        let reports: Vec<Report> = reports
            .into_iter()
            .map(|mut report| {
                // A rate limit or a network blip should not erase a panel that had a real
                // vendor number a moment ago; keep the number and say how old it is.
                if report.windows.is_empty() {
                    if let Health::Unavailable(why) = &report.health {
                        let key = (report.provider.to_string(), report.account.clone());
                        if let Some(last) = previous.get(&key).filter(|r| !r.windows.is_empty()) {
                            report.windows = last.windows.clone();
                            report.notes = last.notes.clone();
                            report.plan = report.plan.or_else(|| last.plan.clone());
                            report.health = Health::Stale {
                                why: why.clone(),
                                since: last_good,
                            };
                        }
                    }
                }
                report
            })
            .collect();

        for report in &reports {
            for window in &report.windows {
                let slot = self
                    .history
                    .entry((report.provider.to_string(), window.label.clone()))
                    .or_default();
                slot.push_back(window.used_percent);
                while slot.len() > 64 {
                    slot.pop_front();
                }
            }
        }
        self.reports = reports;
        self.last_refresh = Some(Instant::now());
        self.refreshing = false;
    }

    fn samples(&self, report: &Report, label: &str) -> Vec<f64> {
        self.history
            .get(&(report.provider.to_string(), label.to_string()))
            .map(|slot| slot.iter().copied().collect())
            .unwrap_or_default()
    }
}

// ------------------------------------------------------------------ palette

const ACCENT: Color = Color::Rgb(0xD9, 0x77, 0x57);
const DIM: Color = Color::Rgb(0x6B, 0x6F, 0x7A);
const FAINT: Color = Color::Rgb(0x3C, 0x40, 0x48);
const TEXT: Color = Color::Rgb(0xC8, 0xCC, 0xD4);
const TRACK: Color = Color::Rgb(0x33, 0x36, 0x3D);

fn provider_color(name: &str) -> Color {
    match name {
        "Claude" => Color::Rgb(0xD9, 0x77, 0x57),
        "Codex" => Color::Rgb(0x4F, 0xB8, 0x9A),
        "OpenCode Go" => Color::Rgb(0x7A, 0xA2, 0xF7),
        "Cursor" => Color::Rgb(0xB9, 0xC2, 0xD6),
        "Grok" => Color::Rgb(0x9C, 0xA3, 0xAF),
        "Devin" => Color::Rgb(0x6E, 0x9E, 0xE8),
        "Command Code" => Color::Rgb(0xE0, 0xA8, 0x5E),
        _ => DIM,
    }
}

/// Filled-bar ramp, cool when there is headroom and hot when there is not.
fn ramp(percent: f64, position: f64) -> Color {
    let (from, to) = match percent {
        p if p >= 95.0 => ((0xD6, 0x45, 0x45), (0xF2, 0x6B, 0x6B)),
        p if p >= 85.0 => ((0xE0, 0x7A, 0x5F), (0xF2, 0x9E, 0x7E)),
        p if p >= 70.0 => ((0xD8, 0xA8, 0x57), (0xEA, 0xC4, 0x72)),
        _ => ((0x5E, 0xB8, 0x8A), (0x8F, 0xDC, 0xA6)),
    };
    let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * position) as u8;
    Color::Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

const PARTIALS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

fn bar_spans(percent: f64, width: usize) -> Vec<Span<'static>> {
    let clamped = percent.clamp(0.0, 100.0);
    let exact = clamped / 100.0 * width as f64;
    let full = exact.floor() as usize;
    let fraction = exact - full as f64;
    let mut spans = Vec::with_capacity(width);
    for index in 0..width {
        let position = index as f64 / width.max(1) as f64;
        if index < full {
            spans.push(Span::styled(
                "█",
                Style::default().fg(ramp(clamped, position)),
            ));
        } else if index == full && fraction > 0.08 {
            let step = ((fraction * 8.0).ceil() as usize).clamp(1, 7) - 1;
            spans.push(Span::styled(
                PARTIALS[step].to_string(),
                Style::default().fg(ramp(clamped, position)),
            ));
        } else {
            spans.push(Span::styled("░", Style::default().fg(TRACK)));
        }
    }
    spans
}

const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Absolute 0-100 sparkline, so a flat line at 90% looks different from one at 5%.
fn spark_spans(samples: &[f64], width: usize) -> Vec<Span<'static>> {
    let take = samples.len().min(width);
    let slice = &samples[samples.len() - take..];
    slice
        .iter()
        .map(|value| {
            let step = ((value.clamp(0.0, 100.0) / 100.0) * 7.0).round() as usize;
            Span::styled(SPARK[step].to_string(), Style::default().fg(FAINT))
        })
        .collect()
}

fn countdown(reset: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (reset - now).num_seconds();
    if seconds <= 0 {
        return "now".into();
    }
    let (days, hours, minutes) = (
        seconds / 86_400,
        (seconds % 86_400) / 3_600,
        (seconds % 3_600) / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".into();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

fn pad(text: &str, width: usize) -> String {
    let clipped = clip(text, width);
    let len = clipped.chars().count();
    format!("{clipped}{}", " ".repeat(width.saturating_sub(len)))
}

// ------------------------------------------------------------------ draw

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    header(frame, app, chunks[0]);
    grid(frame, app, chunks[1]);
    footer(frame, app, chunks[2]);
}

fn header(frame: &mut Frame, app: &App, area: Rect) {
    let now = Utc::now();
    let elapsed = app
        .last_refresh
        .map(|t| t.elapsed().as_secs())
        .map(|s| format!("{s}s"))
        .unwrap_or_else(|| "–".into());

    let status = if app.paused {
        Span::styled("paused", Style::default().fg(ACCENT))
    } else if app.refreshing {
        Span::styled("refreshing…", Style::default().fg(ACCENT))
    } else {
        Span::styled(format!("updated {elapsed} ago"), Style::default().fg(DIM))
    };

    let tightest = app
        .reports
        .iter()
        .filter_map(|report| {
            report
                .windows
                .iter()
                .map(|window| (window.used_percent, report.provider, window))
                .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut second = vec![Span::styled("  ", Style::default()), status];
    if let Some((percent, provider, window)) = tightest {
        second.push(Span::styled("  ·  ", Style::default().fg(FAINT)));
        second.push(Span::styled(
            format!("{} {} ", provider, window.label),
            Style::default().fg(TEXT),
        ));
        second.push(Span::styled(
            format!("{percent:.0}% "),
            Style::default()
                .fg(ramp(percent, 0.7))
                .add_modifier(Modifier::BOLD),
        ));
        if let Some(reset) = window.resets_at {
            second.push(Span::styled(
                format!("resets in {}", countdown(reset, now)),
                Style::default().fg(DIM),
            ));
        }
    }

    let clock = format!("{} UTC", now.format("%H:%M:%S"));
    let title = Line::from(vec![
        Span::styled(
            "USAGEBAR",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   every {}s", app.interval_secs),
            Style::default().fg(FAINT),
        ),
    ]);
    let right = Span::styled(clock, Style::default().fg(DIM));
    let width = area.width as usize;
    let gap = width.saturating_sub(title.width() + right.width()).max(1);
    let line = Line::from(
        title
            .spans
            .into_iter()
            .chain(std::iter::once(Span::raw(" ".repeat(gap))))
            .chain(std::iter::once(right))
            .collect::<Vec<_>>(),
    );

    frame.render_widget(Paragraph::new(vec![line, Line::from(second)]), area);
}

fn footer(frame: &mut Frame, app: &App, area: Rect) {
    let ok = app
        .reports
        .iter()
        .filter(|r| matches!(r.health, Health::Ok))
        .count();
    let keys = Line::from(vec![
        Span::styled(" q ", Style::default().fg(ACCENT)),
        Span::styled("quit ", Style::default().fg(DIM)),
        Span::styled(" r ", Style::default().fg(ACCENT)),
        Span::styled("refresh now ", Style::default().fg(DIM)),
        Span::styled(" space ", Style::default().fg(ACCENT)),
        Span::styled("pause ", Style::default().fg(DIM)),
        Span::styled(
            format!(" {ok}/{} reporting", app.reports.len()),
            Style::default().fg(FAINT),
        ),
    ]);
    frame.render_widget(Paragraph::new(keys), area);
}

/// Cards flow left to right, wrapping into rows; a row is as tall as its tallest card.
fn grid(frame: &mut Frame, app: &App, area: Rect) {
    if app.reports.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "collecting first sample…",
                Style::default().fg(DIM),
            ))),
            area,
        );
        return;
    }

    let columns = match area.width {
        w if w >= 156 => 3,
        w if w >= 102 => 2,
        _ => 1,
    };
    let rows: Vec<&[Report]> = app.reports.chunks(columns).collect();
    let heights: Vec<u16> = rows
        .iter()
        .map(|row| row.iter().map(card_height).max().unwrap_or(4))
        .collect();

    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            heights
                .iter()
                .map(|h| Constraint::Length(*h))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (row_index, row) in rows.iter().enumerate() {
        let rect = row_rects[row_index];
        if rect.height < 3 {
            continue;
        }
        let cells = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                (0..columns)
                    .map(|_| Constraint::Ratio(1, columns as u32))
                    .collect::<Vec<_>>(),
            )
            .split(rect);
        for (card_index, report) in row.iter().enumerate() {
            card(frame, app, report, cells[card_index]);
        }
    }
}

fn card_height(report: &Report) -> u16 {
    let content = match &report.health {
        Health::Unavailable(_) => 2,
        Health::Stale { .. } => report.windows.len() * 2 + 1,
        Health::Ok | Health::NoQuota(_) => {
            report.windows.len() * 2 + usize::from(!report.notes.is_empty())
        }
    };
    (content + 2) as u16
}

fn card(frame: &mut Frame, app: &App, report: &Report, area: Rect) {
    let accent = provider_color(report.provider);
    let healthy = matches!(report.health, Health::Ok | Health::Stale { .. });

    let mut title = vec![Span::styled(
        format!(" {} ", report.provider),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )];
    if let Some(account) = &report.account {
        title.push(Span::styled(
            format!("{} ", crate::model::short_account(account)),
            Style::default().fg(FAINT),
        ));
    }
    let badge = match (&report.plan, &report.health) {
        (Some(plan), _) => Span::styled(format!(" {} ", plan), Style::default().fg(DIM)),
        (None, Health::Stale { .. }) => Span::styled(
            " stale ",
            Style::default().fg(Color::Rgb(0xD8, 0xA8, 0x57)),
        ),
        (None, Health::Ok) => Span::styled(
            format!(" {} ", report.source.glyph()),
            Style::default().fg(FAINT),
        ),
        (None, _) => Span::styled(" n/a ", Style::default().fg(FAINT)),
    };

    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if healthy { FAINT } else { TRACK }))
        .title(Line::from(title))
        .title_top(Line::from(badge).right_aligned());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width < 12 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    match &report.health {
        Health::Unavailable(why) => {
            lines.push(Line::from(Span::styled(
                clip(why, inner.width as usize),
                Style::default().fg(Color::Rgb(0xC9, 0x7B, 0x7B)),
            )));
            lines.push(Line::from(Span::styled(
                "no vendor number to show",
                Style::default().fg(FAINT),
            )));
        }
        Health::NoQuota(why) => {
            lines.push(Line::from(Span::styled(
                clip(why, inner.width as usize),
                Style::default().fg(DIM),
            )));
        }
        Health::Ok => {}
        Health::Stale { .. } => {}
    }

    let width = inner.width as usize;
    let now = Utc::now();
    let mut shown_reset: Option<DateTime<Utc>> = None;
    for window in &report.windows {
        let samples = app.samples(report, &window.label);
        let spark_width = if samples.len() >= 2 {
            8.min(width / 6)
        } else {
            0
        };
        let label_width = 10.min(width / 4);
        let reserved = label_width + 1 + 4 + 1 + spark_width + 1;
        let bar_width = width.saturating_sub(reserved).clamp(6, 44);

        let mut row = vec![
            Span::styled(pad(&window.label, label_width), Style::default().fg(TEXT)),
            Span::raw(" "),
        ];
        row.extend(bar_spans(window.used_percent, bar_width));
        row.push(Span::raw(" "));
        row.push(Span::styled(
            format!("{:>3.0}%", window.used_percent),
            Style::default()
                .fg(ramp(window.used_percent, 0.8))
                .add_modifier(Modifier::BOLD),
        ));
        if spark_width > 0 {
            row.push(Span::raw(" "));
            row.extend(spark_spans(&samples, spark_width));
        }
        lines.push(Line::from(row));

        // Windows that share a billing cycle would otherwise repeat the same countdown on
        // every line of the card.
        let reset = match window.resets_at {
            Some(at) if Some(at) == shown_reset => String::new(),
            Some(at) => {
                shown_reset = Some(at);
                format!("resets in {}", countdown(at, now))
            }
            None if window.used_percent <= 0.0 => "idle".into(),
            None => String::new(),
        };
        let mut sub = vec![
            Span::raw(" ".repeat(label_width + 1)),
            Span::styled(reset, Style::default().fg(DIM)),
        ];
        if let Some(detail) = &window.detail {
            let used = label_width + 1 + sub[1].width();
            let gap = width.saturating_sub(used + detail.chars().count());
            if gap > 1 {
                sub.push(Span::raw(" ".repeat(gap)));
                sub.push(Span::styled(
                    clip(detail, width / 2),
                    Style::default().fg(FAINT),
                ));
            }
        }
        lines.push(Line::from(sub));
    }

    if let Health::Stale { why, since } = &report.health {
        let age = since.elapsed().as_secs();
        lines.push(Line::from(vec![
            Span::styled(
                format!("stale {age}s · "),
                Style::default().fg(Color::Rgb(0xD8, 0xA8, 0x57)),
            ),
            Span::styled(
                clip(why, width.saturating_sub(14)),
                Style::default().fg(FAINT),
            ),
        ]));
    }

    if !report.notes.is_empty() {
        lines.push(Line::from(Span::styled(
            clip(&report.notes.join(" · "), width),
            Style::default().fg(FAINT),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Window;
    use chrono::TimeZone;

    /// A failed poll must keep the previous vendor number, marked stale, rather than
    /// blanking a panel that was reporting fine a minute ago.
    #[test]
    fn failure_keeps_last_good_reading() {
        let mut app = App::new(60);
        let mut good = Report::new("Claude");
        good.windows.push(Window::new("Weekly", 42.0));
        app.absorb(vec![good]);

        app.absorb(vec![Report::failed("Claude", "HTTP 429".into())]);

        let report = &app.reports[0];
        assert_eq!(report.windows.len(), 1);
        assert_eq!(report.windows[0].used_percent, 42.0);
        match &report.health {
            Health::Stale { why, .. } => assert_eq!(why, "HTTP 429"),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    /// Without a previous reading there is nothing honest to show, so the error stands.
    #[test]
    fn failure_without_history_stays_unavailable() {
        let mut app = App::new(60);
        app.absorb(vec![Report::failed("Claude", "HTTP 429".into())]);
        assert!(matches!(app.reports[0].health, Health::Unavailable(_)));
        assert!(app.reports[0].windows.is_empty());
    }

    #[test]
    fn countdown_picks_the_right_unit() {
        let base = Utc.with_ymd_and_hms(2026, 9, 19, 12, 0, 0).unwrap();
        let at = |secs: i64| base + chrono::Duration::seconds(secs);
        assert_eq!(countdown(at(90), base), "1m");
        assert_eq!(countdown(at(3 * 3600 + 14 * 60), base), "3h 14m");
        assert_eq!(countdown(at(2 * 86_400 + 5 * 3600), base), "2d 5h");
        assert_eq!(countdown(at(0), base), "now");
        assert_eq!(countdown(at(-60), base), "now");
    }

    #[test]
    fn bar_splits_into_fill_and_track() {
        let spans = bar_spans(50.0, 10);
        assert_eq!(spans.len(), 10);
        let filled = spans.iter().filter(|s| s.content == "█").count();
        assert_eq!(filled, 5);
    }
}
