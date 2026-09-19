//! The setup overlay: what is shown, in what order, and how often it refreshes.
//! Everything here saves as it is changed; there is no separate apply step.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::config::{self, SortMode};
use crate::input::TextInput;
use crate::model::{AccountRef, Health};
use crate::ui::{self, App, ACCENT, DIM, FAINT, TEXT};

#[derive(Debug, Default)]
pub struct Settings {
    pub selected: usize,
    /// Set while the interval is being typed.
    pub editing: Option<TextInput>,
    /// The account whose removal is waiting for a second press.
    pub confirm_remove: Option<String>,
    pub note: Option<String>,
}

pub enum Action {
    Keep,
    Close,
    Refresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Account(usize),
    AddAccount,
    Sort,
    Interval,
}

fn rows(app: &App) -> Vec<Row> {
    let mut rows: Vec<Row> = (0..app.accounts.len()).map(Row::Account).collect();
    rows.push(Row::AddAccount);
    rows.push(Row::Sort);
    rows.push(Row::Interval);
    rows
}

pub fn handle(settings: &mut Settings, app: &mut App, key: KeyEvent) -> Action {
    if let Some(input) = settings.editing.as_mut() {
        return match key.code {
            KeyCode::Enter => {
                let typed = input.value().trim().to_string();
                settings.editing = None;
                if typed.is_empty() {
                    return Action::Keep;
                }
                match typed.parse::<u64>() {
                    Ok(secs) => {
                        let secs = secs.max(config::MIN_INTERVAL);
                        app.interval_secs = secs;
                        app.config.interval_secs = secs;
                        settings.note = app.save_config().map(|why| format!("not saved: {why}"));
                        settings.note = Some(format!("refreshing every {secs}s"));
                    }
                    Err(_) => settings.note = Some(format!("\"{typed}\" is not a number")),
                }
                Action::Keep
            }
            KeyCode::Esc => {
                settings.editing = None;
                Action::Keep
            }
            _ => {
                input.handle_key(key);
                Action::Keep
            }
        };
    }

    let list = rows(app);
    if list.is_empty() {
        return Action::Close;
    }
    settings.selected = settings.selected.min(list.len() - 1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('s') => return Action::Close,
        KeyCode::Up => {
            settings.selected = settings.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            settings.selected = (settings.selected + 1).min(list.len() - 1);
        }
        _ => {}
    }

    match list[settings.selected] {
        Row::Account(index) => match key.code {
            KeyCode::Char(' ') => {
                let mut accounts = app.accounts.clone();
                accounts[index].hidden = !accounts[index].hidden;
                let label = account_name(&accounts[index]);
                settings.note = app
                    .save_accounts(accounts)
                    .map(|why| format!("not saved: {why}"));
                settings.note = Some(match settings.note.take() {
                    Some(why) => why,
                    None => format!(
                        "{label} {}",
                        if app.accounts[index].hidden {
                            "hidden"
                        } else {
                            "shown"
                        }
                    ),
                });
                return Action::Refresh;
            }
            KeyCode::Char('K') => {
                return move_account(settings, app, index, -1);
            }
            KeyCode::Char('J') => {
                return move_account(settings, app, index, 1);
            }
            KeyCode::Char('x') => {
                return remove_account(settings, app, index);
            }
            _ => {}
        },
        Row::AddAccount => {
            if matches!(key.code, KeyCode::Enter) {
                settings.note = Some("adding accounts arrives with the setup wizard".into());
            }
        }
        Row::Sort => {
            if matches!(
                key.code,
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
            ) {
                app.config.sort = match app.config.sort {
                    SortMode::Manual => SortMode::Smart,
                    SortMode::Smart => SortMode::Manual,
                };
                settings.note = app.save_config().map(|why| format!("not saved: {why}"));
                return Action::Refresh;
            }
        }
        Row::Interval => match key.code {
            KeyCode::Enter => {
                // Starts empty: the current value is on screen, and typing should
                // replace it rather than append to it.
                settings.editing = Some(TextInput::new());
            }
            KeyCode::Left | KeyCode::Right => {
                let delta: i64 = if key.code == KeyCode::Left { -5 } else { 5 };
                let secs = (app.interval_secs as i64 + delta).max(config::MIN_INTERVAL as i64) as u64;
                app.interval_secs = secs;
                app.config.interval_secs = secs;
                settings.note = app.save_config().map(|why| format!("not saved: {why}"));
            }
            _ => {}
        },
    }
    Action::Keep
}

fn move_account(settings: &mut Settings, app: &mut App, index: usize, delta: i64) -> Action {
    let target = index as i64 + delta;
    let mut accounts = app.accounts.clone();
    if target < 0 || target >= accounts.len() as i64 {
        return Action::Keep;
    }
    accounts.swap(index, target as usize);
    settings.selected = target as usize;
    settings.note = app
        .save_accounts(accounts)
        .map(|why| format!("not saved: {why}"));
    Action::Refresh
}

fn remove_account(settings: &mut Settings, app: &mut App, index: usize) -> Action {
    let id = app.accounts[index].id.clone();
    if settings.confirm_remove.as_deref() != Some(id.as_str()) {
        settings.confirm_remove = Some(id);
        settings.note = Some(format!(
            "press x again to remove {} and forget its credentials",
            account_name(&app.accounts[index])
        ));
        return Action::Keep;
    }
    settings.confirm_remove = None;
    let mut accounts = app.accounts.clone();
    accounts.remove(index);
    settings.selected = settings.selected.min(accounts.len().saturating_sub(1));
    app.forget_credentials(&id);
    settings.note = app
        .save_accounts(accounts)
        .map(|why| format!("not saved: {why}"));
    Action::Refresh
}

fn account_name(account: &AccountRef) -> String {
    format!(
        "{} {}",
        account.provider.display(),
        account.label.clone().unwrap_or_default()
    )
    .trim()
    .to_string()
}

// ------------------------------------------------------------------ drawing

pub fn draw(frame: &mut Frame, app: &App, settings: &Settings, area: Rect) {
    let list = rows(app);
    let height = (list.len() + 6).min(area.height as usize) as u16;
    let width = (area.width.saturating_sub(4)).min(76);
    let box_area = centered(area, width, height);
    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(FAINT))
        .title(Line::from(Span::styled(
            " Setup ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .title_top(
            Line::from(Span::styled(" esc to close ", Style::default().fg(FAINT))).right_aligned(),
        );
    let inner = block.inner(box_area);
    frame.render_widget(ratatui::widgets::Clear, box_area);
    frame.render_widget(block, box_area);
    if inner.height == 0 || inner.width < 20 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (row_index, row) in list.iter().enumerate() {
        let selected = row_index == settings.selected;
        let line = match row {
            Row::Account(index) => {
                let account = &app.accounts[*index];
                let mark = if account.hidden { "○" } else { "●" };
                let name = account_name(account);
                let (status, color) = status_of(app, account);
                Line::from(vec![
                    Span::styled(
                        if selected { " ▸ " } else { "   " },
                        Style::default().fg(ACCENT),
                    ),
                    Span::styled(
                        format!("{mark} "),
                        Style::default().fg(if account.hidden { FAINT } else { ACCENT }),
                    ),
                    Span::styled(
                        ui::pad(&name, inner.width as usize / 2),
                        Style::default()
                            .fg(if account.hidden { FAINT } else { TEXT })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(status, Style::default().fg(color)),
                ])
            }
            Row::AddAccount => {
                let span = Span::styled(
                    "   + add account…",
                    Style::default().fg(if selected { ACCENT } else { DIM }),
                );
                Line::from(vec![
                    Span::styled(if selected { " ▸ " } else { "   " }, Style::default().fg(ACCENT)),
                    span,
                ])
            }
            Row::Sort => setting_line(
                selected,
                "Sort",
                match app.config.sort {
                    SortMode::Manual => "manual · the order below",
                    SortMode::Smart => "smart · worst first",
                },
            ),
            Row::Interval => {
                let value = match &settings.editing {
                    Some(input) if input.is_blank() => "▏type seconds, enter to save".to_string(),
                    Some(input) => format!("{}s▏", input.value()),
                    None => format!("{}s", app.interval_secs),
                };
                setting_line(selected, "Interval", &value)
            }
        };
        lines.push(line);
    }
    lines.push(Line::from(""));
    let hint = match &settings.note {
        Some(note) => Line::from(Span::styled(
            ui::clip(note, inner.width as usize),
            Style::default().fg(ACCENT),
        )),
        None => Line::from(Span::styled(
            ui::clip(
                "space show/hide · shift+↑↓ move · x remove · changes save as you make them",
                inner.width as usize,
            ),
            Style::default().fg(FAINT),
        )),
    };
    lines.push(hint);

    frame.render_widget(Paragraph::new(lines), inner);
    if let Some(input) = &settings.editing {
        // Put the real cursor in the interval field so typing feels normal.
        let (_, column) = input.display(inner.width as usize);
        let y = inner.y + (list.len() as u16) + 1;
        let x = inner.x + 13 + column as u16;
        frame.set_cursor_position((x.min(inner.x + inner.width - 1), y));
    }
}

fn setting_line(selected: bool, name: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { " ▸ " } else { "   " },
            Style::default().fg(ACCENT),
        ),
        Span::styled(
            format!("{}", ui::pad(name, 10)),
            Style::default().fg(if selected { TEXT } else { DIM }),
        ),
        Span::styled(
            ui::clip(value, 48),
            Style::default().fg(if selected { TEXT } else { DIM }),
        ),
    ])
}

fn status_of(app: &App, account: &AccountRef) -> (String, ratatui::style::Color) {
    if account.hidden {
        return ("hidden".into(), FAINT);
    }
    match app.report_for(&account.id) {
        Some(report) => match (&report.health, report.peak()) {
            (Health::Unavailable(_), _) => ("unavailable".into(), DIM),
            (_, Some(peak)) => {
                let window = report
                    .windows
                    .iter()
                    .max_by(|a, b| {
                        a.used_percent
                            .partial_cmp(&b.used_percent)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|w| w.label.clone())
                    .unwrap_or_default();
                (format!("{peak:.0}% {window}"), DIM)
            }
            _ => ("no data".into(), DIM),
        },
        None => ("…".into(), FAINT),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .flex(Flex::Center)
        .constraints([Constraint::Length(height)])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .flex(Flex::Center)
        .constraints([Constraint::Length(width)])
        .split(vertical[0]);
    horizontal[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::credentials::MemoryStore;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn test_app() -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("usagebar-settings-{}", std::process::id()));
        let app = App::new(
            60,
            Arc::new(MemoryStore::new()),
            SortMode::Manual,
        );
        (app, dir)
    }

    /// Reordering writes through to the config file so a restart keeps the new order.
    #[test]
    fn reordering_persists_the_account_list() {
        let (mut app, dir) = test_app();
        app.accounts = vec![
            AccountRef::new("claude", crate::model::ProviderId::Claude),
            AccountRef::new("codex", crate::model::ProviderId::Codex),
        ];
        app.config.accounts = app.accounts.clone();
        app.config_path = crate::config::config_path(&dir);
        app.persisted = true;
        app.file_store = Arc::new(crate::credentials::FileStore::load(dir.join("credentials.json")));

        let mut settings = Settings::default();
        settings.selected = 1;
        let action = handle(
            &mut settings,
            &mut app,
            KeyEvent::new(KeyCode::Char('K'), crossterm::event::KeyModifiers::SHIFT),
        );
        assert!(matches!(action, Action::Refresh));
        let saved = config::load(&app.config_path).unwrap().unwrap();
        assert_eq!(saved.accounts[0].id, "codex");
        assert_eq!(saved.accounts[1].id, "claude");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The overlay has to draw in a side panel without losing rows or panicking.
    #[test]
    fn draws_in_a_narrow_pane() {
        let (mut app, _dir) = test_app();
        app.accounts = vec![AccountRef::new("claude", crate::model::ProviderId::Claude)];
        let settings = Settings::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &settings, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(text.contains("Setup"));
        assert!(text.contains("Claude"));
        assert!(text.contains("Interval"));
    }
}
