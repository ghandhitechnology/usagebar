//! The setup overlay: what is shown, in what order, and how often it refreshes.
//! Everything here saves as it is changed; there is no separate apply step.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
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
    /// Set while an account name is being typed.
    pub renaming: Option<Rename>,
    /// The account whose removal is waiting for a second press.
    pub confirm_remove: Option<String>,
    pub note: Option<String>,
}

/// The name being typed, with the account it belongs to: the list can change under an
/// open field, and the id is what survives that.
#[derive(Debug)]
pub struct Rename {
    pub id: String,
    pub input: TextInput,
}

pub enum Action {
    Keep,
    Close,
    Refresh,
    OpenWizard,
}

/// What the bottom bar shows while this screen is open.
pub fn keys() -> Vec<(String, String)> {
    vec![
        ("↑↓".into(), "move".into()),
        ("space".into(), "show/hide".into()),
        ("shift+↑↓".into(), "reorder".into()),
        ("r".into(), "rename".into()),
        ("x".into(), "remove".into()),
        ("esc".into(), "close".into()),
    ]
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
                        settings.note = match app.save_config() {
                            Some(why) => Some(format!("not saved: {why}")),
                            None => Some(format!("refreshing every {secs}s")),
                        };
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

    // A rename field keeps the keys until it is saved or cancelled.
    if let Some(rename) = settings.renaming.as_mut() {
        return match key.code {
            KeyCode::Enter => {
                let typed = rename.input.value().trim().to_string();
                let id = rename.id.clone();
                settings.renaming = None;
                let mut accounts = app.accounts.clone();
                let Some(index) = accounts.iter().position(|account| account.id == id) else {
                    return Action::Keep;
                };
                let provider = accounts[index].provider;
                // Clearing the field is how a name is taken back off an account, and
                // typing the provider's own name back is the same as never having named
                // it: the row would otherwise read "Codex Codex".
                accounts[index].label =
                    (!typed.is_empty() && typed != provider.display()).then_some(typed);
                settings.note = app
                    .save_accounts(accounts)
                    .map(|why| format!("not saved: {why}"));
                Action::Refresh
            }
            KeyCode::Esc => {
                settings.renaming = None;
                settings.note = None;
                Action::Keep
            }
            _ => {
                rename.input.handle_key(key);
                Action::Keep
            }
        };
    }

    let list = rows(app);
    if list.is_empty() {
        return Action::Close;
    }
    settings.selected = settings.selected.min(list.len() - 1);

    // Moving an account: shift with the arrow keys, or J and K for terminals that
    // swallow the modifier.
    let moving = match key.code {
        KeyCode::Char('K') => Some(-1),
        KeyCode::Char('J') => Some(1),
        KeyCode::Up
            if key
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT) =>
        {
            Some(-1)
        }
        KeyCode::Down
            if key
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT) =>
        {
            Some(1)
        }
        _ => None,
    };
    if let (Some(delta), Row::Account(index)) = (moving, list[settings.selected]) {
        return move_account(settings, app, index, delta);
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('s') => return Action::Close,
        KeyCode::Up | KeyCode::BackTab => {
            settings.selected = settings.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Tab => {
            settings.selected = (settings.selected + 1).min(list.len() - 1);
        }
        _ => {}
    }

    match list[settings.selected] {
        Row::Account(index) => match key.code {
            KeyCode::Char(' ') => {
                let mut accounts = app.accounts.clone();
                accounts[index].hidden = !accounts[index].hidden;
                let label = accounts[index].name();
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
            KeyCode::Char('r') => {
                let account = &app.accounts[index];
                settings.renaming = Some(Rename {
                    id: account.id.clone(),
                    // The field opens on the title the row is showing, so the name it
                    // reads can be rewritten rather than only added to.
                    input: TextInput::with_value(account.name()),
                });
                settings.note = Some(format!(
                    "renaming {} · enter to save, esc to cancel",
                    account.name()
                ));
            }
            KeyCode::Char('x') => {
                return remove_account(settings, app, index);
            }
            _ => {}
        },
        Row::AddAccount => {
            if matches!(key.code, KeyCode::Enter) {
                return Action::OpenWizard;
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
                let secs =
                    (app.interval_secs as i64 + delta).max(config::MIN_INTERVAL as i64) as u64;
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
            app.accounts[index].name()
        ));
        return Action::Keep;
    }
    settings.confirm_remove = None;
    let mut accounts = app.accounts.clone();
    accounts.remove(index);
    settings.selected = settings.selected.min(accounts.len().saturating_sub(1));
    // The config first: if that write fails the account is still whole, credentials and all.
    match app.save_accounts(accounts) {
        Some(why) => settings.note = Some(format!("not saved: {why}")),
        None => {
            app.forget_credentials(&id);
            settings.note = None;
        }
    }
    Action::Refresh
}

// ------------------------------------------------------------------ drawing

pub fn draw(frame: &mut Frame, app: &App, settings: &Settings, area: Rect) {
    let list = rows(app);
    let height = (list.len() + 6).min(area.height as usize) as u16;
    let width = (area.width.saturating_sub(4)).min(80);
    let box_area = ui::centered(area, width, height);
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
    let mut cursor: Option<(u16, u16)> = None;
    for (row_index, row) in list.iter().enumerate() {
        let selected = row_index == settings.selected;
        let line = match row {
            Row::Account(index) => {
                let account = &app.accounts[*index];
                let mark = if account.hidden { "○" } else { "●" };
                let name_width = inner.width as usize / 2;
                let renaming = settings
                    .renaming
                    .as_ref()
                    .filter(|rename| rename.id == account.id);
                let (name, tail, color) = match renaming {
                    // The field is the title while it is open, so it starts where the
                    // title is read. Drawing the provider name beside it would repeat
                    // the text the field already holds.
                    Some(rename) => {
                        let (shown, column) = rename
                            .input
                            .display((inner.width as usize).saturating_sub(5));
                        cursor = Some((inner.x + 5 + column as u16, inner.y + row_index as u16));
                        (String::new(), shown, TEXT)
                    }
                    None => {
                        let (status, color) = status_of(app, account);
                        (ui::pad(&account.name(), name_width), status, color)
                    }
                };
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
                        name,
                        Style::default()
                            .fg(if account.hidden { FAINT } else { TEXT })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(tail, Style::default().fg(color)),
                ])
            }
            Row::AddAccount => {
                let span = Span::styled(
                    "   + add account…",
                    Style::default().fg(if selected { ACCENT } else { DIM }),
                );
                Line::from(vec![
                    Span::styled(
                        if selected { " ▸ " } else { "   " },
                        Style::default().fg(ACCENT),
                    ),
                    span,
                ])
            }
            Row::Sort => setting_line(
                selected,
                "Sort",
                match app.config.sort {
                    SortMode::Manual => "manual · the order above",
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
    let content_height = inner.height.saturating_sub(1);
    let offset = settings
        .selected
        .saturating_add(1)
        .saturating_sub(content_height as usize)
        .min(list.len().saturating_sub(content_height as usize));
    frame.render_widget(
        Paragraph::new(lines).scroll((offset as u16, 0)),
        Rect {
            height: content_height,
            ..inner
        },
    );
    let note = settings.note.clone().unwrap_or_else(|| {
        if list.len() > content_height as usize {
            format!("{} / {} · ↑↓ to scroll", settings.selected + 1, list.len())
        } else {
            String::new()
        }
    });
    frame.render_widget(
        Paragraph::new(Span::styled(
            ui::clip(&note, inner.width as usize),
            Style::default().fg(ACCENT),
        )),
        Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        },
    );
    if let Some(input) = &settings.editing {
        // Put the real cursor in the interval field, which is the last row of the list.
        let (_, column) = input.display(inner.width as usize);
        let y = inner.y + (list.len() - offset) as u16 - 1;
        let x = inner.x + 13 + column as u16;
        frame.set_cursor_position((
            x.min(inner.x + inner.width - 1),
            y.min(inner.y + inner.height - 1),
        ));
    }
    if let Some((x, y)) = cursor {
        // The rename field sits where the status does, after the padded account name.
        frame.set_cursor_position((
            x.min(inner.x + inner.width - 1),
            y.min(inner.y + inner.height - 1),
        ));
    }
}

fn setting_line(selected: bool, name: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { " ▸ " } else { "   " },
            Style::default().fg(ACCENT),
        ),
        Span::styled(
            ui::pad(name, 10),
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
        Some(report) => match &report.health {
            Health::Unavailable(_) => ("unavailable".into(), DIM),
            _ => match &report.account {
                Some(email) if !email.is_empty() => (email.chars().take(7).collect(), DIM),
                _ => (String::new(), DIM),
            },
        },
        None => ("…".into(), FAINT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    /// One directory per test: they run in parallel and every one of them writes a
    /// config file.
    fn test_app(name: &str) -> (App, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("usagebar-settings-{}-{name}", std::process::id()));
        let app = App::new(
            60,
            Arc::new(MemoryStore::new()),
            SortMode::Manual,
            Arc::new(crate::credentials::FileStore::load(
                dir.join("credentials.json"),
            )),
        );
        (app, dir)
    }

    fn press(settings: &mut Settings, app: &mut App, code: KeyCode) -> Action {
        handle(
            settings,
            app,
            KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        )
    }

    /// The field opens on the title the row is showing, so an unnamed account starts
    /// from its provider name and the name it is given replaces that text.
    #[test]
    fn renaming_persists_the_label() {
        let (mut app, dir) = test_app("rename");
        app.accounts = vec![AccountRef::new("claude", crate::model::ProviderId::Claude)];
        app.config.accounts = app.accounts.clone();
        app.config_path = crate::config::config_path(&dir);
        app.persisted = true;
        app.file_store = Arc::new(crate::credentials::FileStore::load(
            dir.join("credentials.json"),
        ));

        let mut settings = Settings::default();
        assert!(matches!(
            press(&mut settings, &mut app, KeyCode::Char('r')),
            Action::Keep
        ));
        // The unnamed account opens on its provider name, which the user rewrites.
        assert_eq!(settings.renaming.as_ref().unwrap().input.value(), "Claude");
        // Saving that title back unchanged is not a name, so the account stays unnamed.
        assert!(matches!(
            press(&mut settings, &mut app, KeyCode::Enter),
            Action::Refresh
        ));
        assert_eq!(app.accounts[0].label, None);

        press(&mut settings, &mut app, KeyCode::Char('r'));
        settings.renaming.as_mut().unwrap().input.set("");
        for ch in "work laptop".chars() {
            press(&mut settings, &mut app, KeyCode::Char(ch));
        }
        assert!(matches!(
            press(&mut settings, &mut app, KeyCode::Enter),
            Action::Refresh
        ));
        assert_eq!(app.accounts[0].label.as_deref(), Some("work laptop"));
        let saved = config::load(&app.config_path).unwrap().unwrap();
        assert_eq!(saved.accounts[0].label.as_deref(), Some("work laptop"));

        // esc throws the edit away, including anything typed into a prefilled field.
        press(&mut settings, &mut app, KeyCode::Char('r'));
        press(&mut settings, &mut app, KeyCode::Char('!'));
        press(&mut settings, &mut app, KeyCode::Esc);
        assert_eq!(app.accounts[0].label.as_deref(), Some("work laptop"));

        // A named account opens on the name it was given, ready to be rewritten.
        press(&mut settings, &mut app, KeyCode::Char('r'));
        assert_eq!(
            settings.renaming.as_ref().unwrap().input.value(),
            "work laptop"
        );
        settings.renaming.as_mut().unwrap().input.set("");
        press(&mut settings, &mut app, KeyCode::Enter);
        assert_eq!(app.accounts[0].label, None);
        let saved = config::load(&app.config_path).unwrap().unwrap();
        assert_eq!(saved.accounts[0].label, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reordering writes through to the config file so a restart keeps the new order.
    #[test]
    fn reordering_persists_the_account_list() {
        let (mut app, dir) = test_app("reorder");
        app.accounts = vec![
            AccountRef::new("claude", crate::model::ProviderId::Claude),
            AccountRef::new("codex", crate::model::ProviderId::Codex),
        ];
        app.config.accounts = app.accounts.clone();
        app.config_path = crate::config::config_path(&dir);
        app.persisted = true;
        app.file_store = Arc::new(crate::credentials::FileStore::load(
            dir.join("credentials.json"),
        ));

        let mut settings = Settings {
            selected: 1,
            ..Settings::default()
        };
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

    /// The field takes the title's place on that row, with the cursor riding the text
    /// instead of staying wherever the last edit left it.
    #[test]
    fn renaming_draws_the_field_at_the_cursor() {
        use ratatui::layout::Position;
        let (mut app, _dir) = test_app("rename-draw");
        app.accounts = vec![AccountRef::new("claude", crate::model::ProviderId::Claude)];
        let settings = Settings {
            renaming: Some(Rename {
                id: "claude".into(),
                input: TextInput::with_value("work"),
            }),
            ..Settings::default()
        };
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
        // The field holds the title, so the row does not print the provider name twice.
        assert!(!text.contains("Claude"));
        let width = buffer.area.width as usize;
        let row: String = buffer
            .content()
            .chunks(width)
            .map(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .find(|row| row.contains("work"))
            .unwrap();
        assert!(row.contains("● work"));
        // The field opens where the title is read, cursor four characters into it.
        assert_eq!(
            terminal.backend().cursor_position(),
            Position { x: 12, y: 8 }
        );
    }

    /// A named account is listed by its name here too, so a rename does not leave the
    /// provider name sitting in front of it.
    #[test]
    fn a_named_account_lists_without_its_provider_name() {
        let (mut app, _dir) = test_app("named-row");
        let mut account = AccountRef::new("claude", crate::model::ProviderId::Claude);
        account.label = Some("Chatgpt".into());
        app.accounts = vec![account];
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
        assert!(text.contains("Chatgpt"));
        assert!(!text.contains("Claude"));
    }

    /// The overlay has to draw in a side panel without losing rows or panicking.
    #[test]
    fn draws_in_a_narrow_pane() {
        let (mut app, _dir) = test_app("narrow");
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
