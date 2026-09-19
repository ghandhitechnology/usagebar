//! Everything one account reported, without squeezing it into a panel. Opened with d,
//! and the arrow keys walk through the accounts.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::model::{AccountRef, Health};
use crate::ui::{self, App, DIM, FAINT, TEXT};

pub struct Detail {
    /// Index into the visible accounts.
    pub index: usize,
}

pub enum Action {
    Keep,
    Close,
}

impl Detail {
    pub fn new() -> Self {
        Self { index: 0 }
    }
}

/// Accounts in the order the panels are drawn, so walking left to right in the detail
/// view matches walking across the grid.
pub fn visible(app: &App) -> Vec<&AccountRef> {
    let shown = |id: &str| {
        app.accounts
            .iter()
            .find(|account| account.id == id && !account.hidden)
    };
    let mut out: Vec<&AccountRef> = app.reports.iter().filter_map(|r| shown(&r.key)).collect();
    for account in app.accounts.iter().filter(|account| !account.hidden) {
        if !out.iter().any(|seen| seen.id == account.id) {
            out.push(account);
        }
    }
    out
}

/// What the bottom bar shows while this screen is open.
pub fn keys(app: &App, detail: &Detail) -> Vec<(String, String)> {
    let count = visible(app).len();
    let position = if count == 0 {
        "0/0".to_string()
    } else {
        format!("{}/{}", detail.index.min(count - 1) + 1, count)
    };
    vec![
        ("←/→".into(), format!("account {position}")),
        ("esc".into(), "close".into()),
    ]
}

pub fn handle(detail: &mut Detail, app: &mut App, key: KeyEvent) -> Action {
    let accounts = visible(app);
    match key.code {
        KeyCode::Esc | KeyCode::Char('d') => return Action::Close,
        KeyCode::Left | KeyCode::Up => {
            detail.index = detail.index.saturating_sub(1);
        }
        KeyCode::Right | KeyCode::Down if !accounts.is_empty() => {
            detail.index = (detail.index + 1).min(accounts.len() - 1);
        }
        _ => {}
    }
    Action::Keep
}

pub fn draw(frame: &mut Frame, app: &App, detail: &Detail, area: Rect) {
    let accounts = visible(app);
    if accounts.is_empty() {
        return;
    }
    let index = detail.index.min(accounts.len() - 1);
    let account = accounts[index];
    let report = app.report_for(&account.id);

    let width = (area.width.saturating_sub(4)).min(92);
    let text_width = width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    match report {
        None => {
            lines.push(Line::from(Span::styled(
                "waiting for the first reading…",
                Style::default().fg(DIM),
            )));
        }
        Some(report) => {
            let now = chrono::Utc::now();
            if report.windows.is_empty() {
                lines.push(Line::from(Span::styled(
                    health_detail(&report.health)
                        .unwrap_or_else(|| "no windows reported".into()),
                    Style::default().fg(DIM),
                )));
            }
            for window in &report.windows {
                lines.push(window_line(window, text_width, now));
            }
            if let Some(detail) = health_detail(&report.health) {
                if !report.windows.is_empty() {
                    lines.push(Line::from(Span::styled(
                        ui::clip(&detail, text_width),
                        Style::default().fg(Color::Rgb(0xD8, 0xA8, 0x57)),
                    )));
                }
            }
            if !report.facts.is_empty() {
                lines.push(Line::from(""));
                for fact in &report.facts {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {}", ui::pad(&fact.label, 20)),
                            Style::default().fg(DIM),
                        ),
                        Span::styled(
                            ui::clip(&fact.value, text_width.saturating_sub(24)),
                            Style::default().fg(TEXT),
                        ),
                    ]));
                }
            }
            if !report.notes.is_empty() {
                lines.push(Line::from(""));
                for note in &report.notes {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", ui::clip(note, text_width.saturating_sub(2))),
                        Style::default().fg(FAINT),
                    )));
                }
            }
        }
    }

    lines.push(Line::from(""));
    if let Some(stored) = app.store.get(&account.id) {
        if let Some(origin) = &stored.origin {
            lines.push(Line::from(vec![
                Span::styled("  credentials  ", Style::default().fg(DIM)),
                Span::styled(
                    ui::clip(&origin.display().to_string(), text_width.saturating_sub(15)),
                    Style::default().fg(FAINT),
                ),
            ]));
        }
    }
    let refreshed = app
        .last_refresh
        .map(|at| format!("{}s ago", at.elapsed().as_secs()))
        .unwrap_or_else(|| "never".into());
    lines.push(Line::from(vec![
        Span::styled("  refreshed    ", Style::default().fg(DIM)),
        Span::styled(refreshed, Style::default().fg(FAINT)),
    ]));

    let height = (lines.len() + 2).min(area.height.saturating_sub(2) as usize) as u16;
    let box_area = ui::centered(area, width, height);

    let name = match &account.label {
        Some(label) => format!("{} · {label}", account.provider.display()),
        None => report
            .and_then(|report| report.account.clone())
            .map(|vendor| format!("{} · {vendor}", account.provider.display()))
            .unwrap_or_else(|| account.provider.display().to_string()),
    };
    let right = match report {
        Some(report) => {
            let mut bits = vec![report.source.glyph().to_string()];
            if let Some(plan) = &report.plan {
                bits.push(plan.clone());
            }
            bits.push(health_word(&report.health).to_string());
            bits.join(" · ")
        }
        None => "no report yet".to_string(),
    };
    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(FAINT))
        .title(Line::from(Span::styled(
            format!(" {name} "),
            Style::default()
                .fg(ui::provider_color(account.provider))
                .add_modifier(Modifier::BOLD),
        )))
        .title_top(
            Line::from(Span::styled(format!(" {right} "), Style::default().fg(DIM))).right_aligned(),
        );
    let inner = block.inner(box_area);
    frame.render_widget(ratatui::widgets::Clear, box_area);
    frame.render_widget(block, box_area);
    if inner.height == 0 || inner.width < 30 {
        return;
    }
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn window_line(
    window: &crate::model::Window,
    width: usize,
    now: chrono::DateTime<chrono::Utc>,
) -> Line<'static> {
    let reset_text = window
        .resets_at
        .map(|reset| {
            format!(
                "resets {} (in {})",
                reset.with_timezone(&chrono::Local).format("%b %d %H:%M"),
                ui::countdown(reset, now)
            )
        })
        .unwrap_or_default();
    // The bar takes what the label, the percentage and the reset text leave behind.
    let fixed = 2 + 17 + 1 + 4 + if reset_text.is_empty() { 0 } else { 2 };
    let bar_width = width
        .saturating_sub(fixed + reset_text.chars().count())
        .clamp(6, 24);
    let mut spans = vec![Span::styled(
        format!("  {}", ui::pad(&window.label, 16)),
        Style::default().fg(TEXT),
    )];
    spans.extend(ui::bar_spans(window.used_percent, bar_width));
    spans.push(Span::styled(
        format!(" {:>3.0}%", window.used_percent),
        Style::default()
            .fg(ui::ramp(window.used_percent, 0.8))
            .add_modifier(Modifier::BOLD),
    ));
    if !reset_text.is_empty() {
        spans.push(Span::styled(
            format!("  {}", ui::clip(&reset_text, width.saturating_sub(fixed + bar_width))),
            Style::default().fg(DIM),
        ));
    }
    Line::from(spans)
}

fn health_word(health: &Health) -> &'static str {
    match health {
        Health::Ok => "ok",
        Health::NoQuota(_) => "no quota",
        Health::Stale { .. } => "stale",
        Health::Unavailable(_) => "unavailable",
    }
}

fn health_detail(health: &Health) -> Option<String> {
    match health {
        Health::Ok => None,
        Health::NoQuota(why) => Some(why.clone()),
        Health::Stale { why, since } => Some(format!(
            "stale for {}s: {why}",
            since.elapsed().as_secs()
        )),
        Health::Unavailable(why) => Some(why.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;
    use crate::model::{Fact, ProviderId, Report, Window};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn app_with_report(report: Report) -> App {
        let dir = std::env::temp_dir().join(format!("usagebar-detail-{}", std::process::id()));
        let mut app = App::new(
            60,
            Arc::new(MemoryStore::new()),
            crate::config::SortMode::Manual,
            Arc::new(crate::credentials::FileStore::load(dir.join("credentials.json"))),
        );
        app.accounts = vec![AccountRef::new("codex", ProviderId::Codex)];
        app.absorb(vec![report]);
        app
    }

    /// The detail view exists to show what a panel cannot: every window with its exact
    /// reset time, and the flat vendor numbers, including the quiet ones.
    #[test]
    fn detail_shows_windows_resets_and_quiet_facts() {
        let mut report = Report::new(ProviderId::Codex).key("codex");
        report.windows.push(Window::new("Weekly", 95.0));
        report.facts.push(Fact::new("banked resets", "2"));
        report.facts.push(Fact::quiet("gpt-6-astra", "available"));
        let app = app_with_report(report);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &Detail::new(), frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(text.contains("Weekly"));
        assert!(text.contains("banked resets"));
        assert!(text.contains("gpt-6-astra"));
        assert!(text.contains("resets"));
    }

    /// A provider that failed still opens, and says why instead of showing a blank box.
    #[test]
    fn detail_explains_a_failed_provider() {
        let app = app_with_report(
            Report::failed(ProviderId::Codex, "token rejected".into()).key("codex"),
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &Detail::new(), frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(text.contains("token rejected"));
        assert!(text.contains("unavailable"));
    }

    /// The bottom bar carries the controls, including which account is on screen.
    #[test]
    fn keys_name_the_account_and_its_position() {
        let mut app = app_with_report(Report::new(ProviderId::Codex).key("codex"));
        app.accounts.push(AccountRef::new("claude", ProviderId::Claude));
        let keys = keys(&app, &Detail::new());
        assert_eq!(keys[0].0, "←/→");
        assert!(keys[0].1.contains("1/2"), "{:?}", keys[0]);
        assert_eq!(keys[1].0, "esc");
    }
}
