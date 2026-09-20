//! First-run setup: connect the credentials already on this machine, or paste one in.
//! Nothing is written until the finish screen, so a half-finished wizard leaves no trace.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::config::SortMode;
use crate::credentials::{self, Credential, CredentialStore, StoredCredential};
use crate::input::TextInput;
use crate::model::{AccountRef, ProviderId, Report};
use crate::providers::{self, Detected};
use crate::ui::{self, App, ACCENT, DIM, FAINT, TEXT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// What the scan found, with checkboxes.
    Welcome,
    /// Which provider to connect by hand.
    Provider,
    /// The fields for that provider, plus the check.
    Connect,
    /// What will be saved.
    Done,
}

pub struct Wizard {
    pub step: Step,
    /// True when this is the first run rather than an account being added later.
    pub first_run: bool,
    pub detected: Vec<Detected>,
    pub picked: Vec<bool>,
    pub pending: Vec<Pending>,
    pub provider_pick: usize,
    pub welcome_row: usize,
    pub connect: Option<Connect>,
    pub note: Option<String>,
    pub verifying: bool,
    verify_rx: Option<Receiver<Result<Report, String>>>,
    pub sort: SortMode,
    /// Rendering clamps this offset after a terminal resize.
    scroll: Cell<usize>,
}

pub struct Pending {
    pub account: AccountRef,
    pub credential: Credential,
    pub origin: Option<PathBuf>,
    pub summary: String,
}

pub struct Connect {
    pub provider: ProviderId,
    pub fields: Vec<Field>,
    pub focus: usize,
    /// Raw credentials are available only after opening the advanced form.
    pub advanced: bool,
    /// The vendor's answer after a successful check.
    pub verified: Option<String>,
    pub error: Option<String>,
}

pub struct Field {
    pub label: &'static str,
    pub input: TextInput,
}

pub enum Action {
    Keep,
    Close,
    /// The wizard wrote config and credentials; the app should refresh.
    Saved,
}

impl Wizard {
    pub fn new(detected: Vec<Detected>, sort: SortMode) -> Self {
        let picked = detected.iter().map(|entry| entry.error.is_none()).collect();
        let note = detected
            .iter()
            .find(|entry| entry.error.is_some())
            .and_then(|entry| entry.error.clone());
        Self {
            step: Step::Welcome,
            first_run: true,
            detected,
            picked,
            pending: Vec::new(),
            provider_pick: 0,
            welcome_row: 0,
            connect: None,
            note,
            verifying: false,
            verify_rx: None,
            sort,
            scroll: Cell::new(0),
        }
    }

    /// Opened from setup to connect one more account: no scan, no skip semantics.
    pub fn new_add(sort: SortMode) -> Self {
        let mut wizard = Self::new(Vec::new(), sort);
        wizard.first_run = false;
        wizard
    }

    /// Called every tick so a background check can land without blocking the UI.
    pub fn poll(&mut self) {
        let Some(rx) = &self.verify_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(report)) => {
                self.verifying = false;
                self.verify_rx = None;
                let who = report.account.clone().or_else(|| report.label.clone());
                let plan = report.plan.clone().unwrap_or_default();
                let summary = match (who, plan.is_empty()) {
                    (Some(who), false) if who == plan => plan,
                    (Some(who), false) => format!("{who} · {plan}"),
                    (Some(who), true) => who,
                    (None, false) => plan,
                    (None, true) => "connected".into(),
                };
                if let Some(connect) = self.connect.as_mut() {
                    connect.verified = Some(summary.clone());
                    connect.error = None;
                    // A blank name field takes the vendor's own account name.
                    if let Some(label) = connect.fields.last_mut().filter(|f| f.input.is_blank()) {
                        if let Some(account) = &report.account {
                            label.input.set(account.clone());
                        }
                    }
                }
                self.note = Some(format!("checked: {summary}"));
            }
            Ok(Err(why)) => {
                self.verifying = false;
                self.verify_rx = None;
                self.note = None;
                if let Some(connect) = self.connect.as_mut() {
                    connect.error = Some(why);
                    connect.verified = None;
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.verifying = false;
                self.verify_rx = None;
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(connect) = self.connect.as_mut() {
            if let Some(field) = connect.fields.get_mut(connect.focus) {
                field.input.paste(text);
                connect.verified = None;
                connect.error = None;
            }
        }
    }

    /// How many accounts the finish screen will save.
    pub fn total(&self) -> usize {
        self.selected_detected().count() + self.pending.len()
    }

    fn selected_detected(&self) -> impl Iterator<Item = &Detected> {
        self.detected
            .iter()
            .zip(self.picked.iter())
            .filter(|(entry, picked)| **picked && entry.credential.is_some())
            .map(|(entry, _)| entry)
    }
}

/// The wizard's rows, so hit-testing and drawing cannot drift apart.
fn welcome_rows(wizard: &Wizard) -> usize {
    wizard.detected.len() + wizard.pending.len() + 1
}

pub fn handle(wizard: &mut Wizard, app: &mut App, key: KeyEvent) -> Action {
    if wizard.verifying {
        if key.code == KeyCode::Esc {
            wizard.verify_rx = None;
            wizard.verifying = false;
            wizard.note = None;
            if let Some(connect) = wizard.connect.as_mut() {
                connect.error = Some("check cancelled".into());
            }
        }
        return Action::Keep;
    }
    match wizard.step {
        Step::Welcome => welcome(wizard, app, key),
        Step::Provider => provider(wizard, key),
        Step::Connect => connect(wizard, app, key),
        Step::Done => done(wizard, app, key),
    }
}

fn welcome(wizard: &mut Wizard, _app: &mut App, key: KeyEvent) -> Action {
    let rows = welcome_rows(wizard);
    let add_row = wizard.detected.len() + wizard.pending.len();
    match key.code {
        KeyCode::Esc => {
            // Skipping writes an empty config, which is what keeps the wizard from
            // opening again on the next run. Auto-detection still applies.
            return Action::Close;
        }
        KeyCode::Up | KeyCode::BackTab => wizard.welcome_row = wizard.welcome_row.saturating_sub(1),
        KeyCode::Down | KeyCode::Tab => wizard.welcome_row = (wizard.welcome_row + 1).min(rows - 1),
        KeyCode::Char(' ') => {
            if wizard.welcome_row < wizard.detected.len() {
                let index = wizard.welcome_row;
                if wizard.detected[index].credential.is_some() {
                    wizard.picked[index] = !wizard.picked[index];
                }
            } else if wizard.welcome_row < add_row {
                // Taking back an account that was just connected.
                let index = wizard.welcome_row - wizard.detected.len();
                wizard.pending.remove(index);
                wizard.welcome_row = wizard.welcome_row.min(add_row.saturating_sub(2));
            }
        }
        KeyCode::Char('a') => {
            wizard.step = Step::Provider;
            wizard.provider_pick = 0;
        }
        KeyCode::Enter => {
            if wizard.welcome_row == add_row {
                wizard.step = Step::Provider;
                wizard.provider_pick = 0;
                return Action::Keep;
            }
            wizard.step = Step::Done;
            wizard.scroll.set(0);
            wizard.note = None;
        }
        _ => {}
    }
    Action::Keep
}

fn provider(wizard: &mut Wizard, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            wizard.step = Step::Welcome;
            wizard.note = None;
        }
        KeyCode::Up | KeyCode::BackTab => {
            wizard.provider_pick = wizard.provider_pick.saturating_sub(1)
        }
        KeyCode::Down | KeyCode::Tab => {
            wizard.provider_pick = (wizard.provider_pick + 1).min(ProviderId::ALL.len() - 1)
        }
        KeyCode::Enter => {
            let provider = ProviderId::ALL[wizard.provider_pick];
            wizard.connect = Some(Connect::new(provider));
            wizard.step = Step::Connect;
            wizard.note = None;
        }
        _ => {}
    }
    Action::Keep
}

fn connect(wizard: &mut Wizard, app: &mut App, key: KeyEvent) -> Action {
    let Some(connect) = wizard.connect.as_mut() else {
        wizard.step = Step::Provider;
        return Action::Keep;
    };
    match key.code {
        KeyCode::Esc => {
            wizard.step = Step::Provider;
            wizard.connect = None;
            wizard.note = None;
        }
        KeyCode::F(2) => {
            connect.advanced = true;
            connect.focus = 0;
        }
        KeyCode::Up | KeyCode::BackTab => {
            let fields = connect.visible_fields();
            let current = fields
                .iter()
                .position(|&index| index == connect.focus)
                .unwrap_or(0);
            connect.focus = fields[current.saturating_sub(1)];
        }
        KeyCode::Down | KeyCode::Tab => {
            let fields = connect.visible_fields();
            let current = fields
                .iter()
                .position(|&index| index == connect.focus)
                .unwrap_or(0);
            connect.focus = fields[(current + 1).min(fields.len() - 1)];
        }
        KeyCode::Char('s')
            if key.modifiers.contains(KeyModifiers::CONTROL) && connect.error.is_some() =>
        {
            return save_connect(wizard, app);
        }
        KeyCode::Enter => {
            if connect.verified.is_some() {
                return save_connect(wizard, app);
            }
            return check(wizard);
        }
        _ => {
            if let Some(field) = connect.fields.get_mut(connect.focus) {
                if field.input.handle_key(key) {
                    // Typing in the name does not invalidate the check; changing a
                    // credential does.
                    if connect.focus + 1 < connect.fields.len() {
                        connect.verified = None;
                    }
                    connect.error = None;
                    wizard.note = None;
                }
            }
        }
    }
    Action::Keep
}

/// Start the live check on a worker thread; the answer arrives via poll().
fn check(wizard: &mut Wizard) -> Action {
    let Some(connect) = wizard.connect.as_ref() else {
        return Action::Keep;
    };
    match connect.build(connect.provider) {
        Ok((credential, origin)) => {
            let provider = connect.provider;
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(providers::verify(provider, &credential, origin.as_deref()));
            });
            wizard.verify_rx = Some(rx);
            wizard.verifying = true;
            wizard.note = Some(format!("checking {}", provider.display()));
        }
        Err(why) => {
            if let Some(connect) = wizard.connect.as_mut() {
                connect.error = Some(why);
            }
        }
    }
    Action::Keep
}

/// Move the checked connection into the pending list. Nothing is written yet.
fn save_connect(wizard: &mut Wizard, app: &mut App) -> Action {
    let (credential, origin, provider, label) = {
        let Some(connect) = wizard.connect.as_ref() else {
            return Action::Keep;
        };
        let Ok((credential, origin)) = connect.build(connect.provider) else {
            return Action::Keep;
        };
        let label = connect
            .fields
            .last()
            .map(|field| field.input.value().trim().to_string())
            .filter(|value| !value.is_empty());
        (credential, origin, connect.provider, label)
    };
    let summary = match wizard.connect.as_ref() {
        Some(connect) if connect.verified.is_some() => connect
            .verified
            .clone()
            .unwrap_or_else(|| "connected".into()),
        Some(connect) if connect.error.is_some() => format!(
            "saved unchecked: {}",
            connect.error.clone().unwrap_or_default()
        ),
        _ => "connected".into(),
    };

    let existing = find_existing(wizard, app, provider, &credential, origin.as_deref());
    // Ids are allocated against the config and the connections made so far in this same
    // session, or two accounts of one provider would collide and replace each other.
    let id = existing.unwrap_or_else(|| {
        let taken: Vec<&str> = app
            .config
            .accounts
            .iter()
            .map(|account| account.id.as_str())
            .chain(
                wizard
                    .pending
                    .iter()
                    .map(|pending| pending.account.id.as_str()),
            )
            .collect();
        crate::config::free_id(provider, taken.into_iter())
    });
    let account = AccountRef {
        id,
        provider,
        label,
        hidden: false,
        problem: None,
    };
    let pending = Pending {
        account,
        credential,
        origin,
        summary,
    };
    // Reconnecting the same account replaces the old credential instead of adding a row.
    match wizard
        .pending
        .iter()
        .position(|other| other.account.id == pending.account.id)
    {
        Some(index) => wizard.pending[index] = pending,
        None => wizard.pending.push(pending),
    }
    wizard.connect = None;
    wizard.step = Step::Welcome;
    wizard.note = Some("account ready; add another or press enter to continue".into());
    wizard.welcome_row = wizard
        .welcome_row
        .min(welcome_rows(wizard).saturating_sub(1));
    Action::Keep
}

fn done(wizard: &mut Wizard, app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Up | KeyCode::BackTab => wizard.scroll.set(wizard.scroll.get().saturating_sub(1)),
        KeyCode::Down | KeyCode::Tab => wizard.scroll.set(wizard.scroll.get().saturating_add(1)),
        KeyCode::Esc => {
            wizard.step = Step::Welcome;
            wizard.note = None;
        }
        KeyCode::Char('t') => {
            wizard.sort = match wizard.sort {
                SortMode::Manual => SortMode::Smart,
                SortMode::Smart => SortMode::Manual,
            };
        }
        KeyCode::Enter => match finish(wizard, app) {
            Ok(()) => return Action::Saved,
            Err(why) => wizard.note = Some(why),
        },
        _ => {}
    }
    Action::Keep
}

/// Write the config and the credentials the wizard collected.
pub fn finish(wizard: &mut Wizard, app: &mut App) -> Result<(), String> {
    let already = already_imported(app);
    // Two Go keys with no vendor names need ordinals, the same as a fresh scan gives them.
    let mut go_count = app
        .config
        .accounts
        .iter()
        .filter(|account| account.provider == ProviderId::OpenCodeGo)
        .count();
    let go_total = go_count
        + wizard
            .selected_detected()
            .filter(|entry| entry.provider == ProviderId::OpenCodeGo)
            .count();
    for (entry, picked) in wizard.detected.iter().zip(wizard.picked.iter()) {
        if !picked {
            continue;
        }
        let Some(credential) = &entry.credential else {
            continue;
        };
        // A rescan finds the same credentials again; do not import them twice.
        let same_origin = entry
            .origin
            .as_ref()
            .is_some_and(|origin| already.origins.iter().any(|seen| seen == origin));
        let same_secret = already.secrets.iter().any(|seen| seen == credential);
        if same_origin || same_secret {
            continue;
        }
        let taken: Vec<&str> = app
            .config
            .accounts
            .iter()
            .map(|account| account.id.as_str())
            .chain(
                wizard
                    .pending
                    .iter()
                    .map(|pending| pending.account.id.as_str()),
            )
            .collect();
        let id = crate::config::free_id(entry.provider, taken.into_iter());
        let mut account = AccountRef::new(id.clone(), entry.provider);
        if entry.provider == ProviderId::OpenCodeGo && go_total > 1 {
            go_count += 1;
            account.label = Some(format!("#{go_count}"));
        }
        app.config.accounts.push(account);
        app.file_store.put(
            &id,
            StoredCredential::new(credential.clone()).with_origin(entry.origin.clone()),
        )?;
    }
    for pending in &wizard.pending {
        // Pasting a secret that matches what came from a vendor file keeps the file
        // link, so refreshes elsewhere are still picked up.
        let existing = app.file_store.get(&pending.account.id);
        let origin = match (&pending.origin, &existing) {
            (Some(origin), _) => Some(origin.clone()),
            (None, Some(existing)) if existing.secret == pending.credential => {
                existing.origin.clone()
            }
            _ => None,
        };
        app.file_store.put(
            &pending.account.id,
            StoredCredential::new(pending.credential.clone()).with_origin(origin),
        )?;
        match app
            .config
            .accounts
            .iter()
            .position(|account| account.id == pending.account.id)
        {
            Some(index) => {
                // Reconnecting keeps the name the user gave it unless they typed a new one.
                let previous = app.config.accounts[index].label.clone();
                app.config.accounts[index] = pending.account.clone();
                if app.config.accounts[index].label.is_none() {
                    app.config.accounts[index].label = previous;
                }
            }
            None => app.config.accounts.push(pending.account.clone()),
        }
    }
    app.config.sort = wizard.sort;
    if !app.interval_locked {
        app.config.interval_secs = app.interval_secs;
    }
    // Finishing with a list means the list is the source of truth; finishing with
    // nothing selected is the same as skipping, so keep scanning each run.
    app.config.detect = Some(app.config.accounts.is_empty());
    app.accounts = app.config.accounts.clone();
    app.store = std::sync::Arc::clone(&app.file_store)
        as std::sync::Arc<dyn crate::credentials::CredentialStore>;
    if let Some(why) = app.save_config() {
        return Err(why);
    }
    app.persisted = true;
    app.boot_note = None;
    Ok(())
}

struct Imported {
    origins: Vec<PathBuf>,
    secrets: Vec<Credential>,
}

fn already_imported(app: &App) -> Imported {
    let mut imported = Imported {
        origins: Vec::new(),
        secrets: Vec::new(),
    };
    for account in &app.config.accounts {
        if let Some(stored) = app.file_store.get(&account.id) {
            if let Some(origin) = stored.origin {
                imported.origins.push(origin);
            }
            imported.secrets.push(stored.secret);
        }
    }
    imported
}

/// The account a new connection belongs to, when it is one we already have.
fn find_existing(
    wizard: &Wizard,
    app: &App,
    provider: ProviderId,
    credential: &Credential,
    origin: Option<&Path>,
) -> Option<String> {
    let matches = |stored: &Option<StoredCredential>, id: &str| {
        let Some(stored) = stored else {
            return None;
        };
        let same_secret = stored.secret == *credential;
        let same_origin = match (origin, stored.origin.as_deref()) {
            (Some(origin), Some(seen)) => origin == seen,
            _ => false,
        };
        (same_secret || same_origin).then(|| id.to_string())
    };
    for account in app
        .config
        .accounts
        .iter()
        .filter(|account| account.provider == provider)
    {
        if let Some(id) = matches(&app.file_store.get(&account.id), &account.id) {
            return Some(id);
        }
    }
    for pending in wizard
        .pending
        .iter()
        .filter(|pending| pending.account.provider == provider)
    {
        let same_secret = pending.credential == *credential;
        let same_origin = match (origin, pending.origin.as_deref()) {
            (Some(origin), Some(seen)) => origin == seen,
            _ => false,
        };
        if same_secret || same_origin {
            return Some(pending.account.id.clone());
        }
    }
    None
}

impl Connect {
    pub fn new(provider: ProviderId) -> Self {
        let path = credentials::vendor_file(provider)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let mut fields = vec![Field::new("Login file", TextInput::with_value(path))];
        match provider {
            ProviderId::Claude => {
                fields.push(Field::new("Access token", TextInput::new().secret()));
                fields.push(Field::new("Refresh token", TextInput::new().secret()));
            }
            ProviderId::Codex => {
                fields.push(Field::new("Access token", TextInput::new().secret()));
                fields.push(Field::new("Account id", TextInput::new()));
            }
            _ => {
                let label = match provider {
                    ProviderId::OpenCodeGo => "Go API key",
                    ProviderId::Cursor => "Access token",
                    ProviderId::Grok => "Session key",
                    _ => "API key",
                };
                fields.push(Field::new(label, TextInput::new().secret()));
            }
        }
        fields.push(Field::new("Name (optional)", TextInput::new()));
        let focus = if provider == ProviderId::OpenCodeGo {
            1
        } else {
            fields.len() - 1
        };
        Self {
            provider,
            fields,
            focus,
            advanced: provider == ProviderId::OpenCodeGo,
            verified: None,
            error: None,
        }
    }

    fn visible_fields(&self) -> Vec<usize> {
        if self.provider == ProviderId::OpenCodeGo {
            (1..self.fields.len()).collect()
        } else if self.advanced {
            (0..self.fields.len()).collect()
        } else {
            vec![self.fields.len() - 1]
        }
    }

    /// A pasted secret wins over the file field, so the prefilled path stays a
    /// convenience rather than something to clear out first.
    pub fn build(&self, provider: ProviderId) -> Result<(Credential, Option<PathBuf>), String> {
        let field = |index: usize| {
            self.fields
                .get(index)
                .map(|field| field.input.value().trim().to_string())
                .unwrap_or_default()
        };
        let path = field(0);
        let pasted = match provider {
            ProviderId::Claude => {
                let (access, refresh) = (field(1), field(2));
                if access.is_empty() && refresh.is_empty() {
                    None
                } else if access.is_empty() || refresh.is_empty() {
                    return Err("a pasted Claude login needs both tokens".into());
                } else {
                    Some(Credential::ClaudeOauth {
                        access_token: access,
                        refresh_token: refresh,
                        expires_at: 0,
                        subscription_type: None,
                    })
                }
            }
            ProviderId::Codex => {
                let access = field(1);
                if access.is_empty() {
                    None
                } else {
                    let account_id = field(2);
                    Some(Credential::CodexTokens {
                        access_token: access,
                        account_id: (!account_id.is_empty()).then_some(account_id),
                        refresh_token: None,
                    })
                }
            }
            _ => {
                let token = field(1);
                (!token.is_empty()).then_some(Credential::Token { token })
            }
        };
        if let Some(credential) = pasted {
            return Ok((credential, None));
        }
        if !path.is_empty() {
            let path = PathBuf::from(&path);
            let credential = credentials::from_vendor_file(provider, &path)
                .ok_or_else(|| format!("{} holds no usable credentials", path.display()))?;
            return Ok((credential, Some(path)));
        }
        Err(match provider {
            ProviderId::Claude => {
                "Claude login not found. Sign in with Claude Code, then retry.".into()
            }
            ProviderId::Codex => "Codex login not found. Sign in with Codex, then retry.".into(),
            ProviderId::OpenCodeGo => "Paste your OpenCode Go API key.".into(),
            ProviderId::Cursor => {
                "Cursor login not found. Sign in with Cursor agent, then retry.".into()
            }
            ProviderId::Grok => "Grok login not found. Sign in with Grok CLI, then retry.".into(),
            ProviderId::Devin => "Devin login not found. Sign in to Devin, then retry.".into(),
            ProviderId::CommandCode => {
                "Command Code login not found. Sign in to Command Code, then retry.".into()
            }
        })
    }
}

impl Field {
    fn new(label: &'static str, input: TextInput) -> Self {
        Self { label, input }
    }
}

// ------------------------------------------------------------------ drawing

pub fn draw(frame: &mut Frame, app: &App, wizard: &Wizard, area: Rect) {
    let width = (area.width.saturating_sub(4)).min(80);
    let rows = match wizard.step {
        Step::Welcome => welcome_rows(wizard) + 6,
        Step::Provider => ProviderId::ALL.len() + 5,
        Step::Connect => wizard
            .connect
            .as_ref()
            .map(|connect| connect.visible_fields().len() + 10)
            .unwrap_or(8),
        Step::Done => wizard.total() + 8,
    };
    let height = (rows as u16).min(area.height.saturating_sub(2));
    let box_area = ui::centered(area, width, height);
    let title = match wizard.step {
        Step::Welcome if !wizard.first_run => " Connect an account ",
        Step::Welcome => " Set up usagebar ",
        Step::Provider => " Connect a provider ",
        Step::Connect => " Connect ",
        Step::Done => " Finish ",
    };
    let exit_hint = match wizard.step {
        Step::Welcome if wizard.first_run => " esc to skip ",
        _ => " esc to go back ",
    };
    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(FAINT))
        .title(Line::from(Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .title_top(Line::from(Span::styled(exit_hint, Style::default().fg(FAINT))).right_aligned());
    let inner = block.inner(box_area);
    frame.render_widget(ratatui::widgets::Clear, box_area);
    frame.render_widget(block, box_area);
    if inner.height == 0 || inner.width < 24 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor: Option<(u16, u16)> = None;
    match wizard.step {
        Step::Welcome => welcome_lines(wizard, inner, &mut lines),
        Step::Provider => provider_lines(wizard, inner, &mut lines),
        Step::Connect => connect_lines(wizard, inner, &mut lines, &mut cursor),
        Step::Done => done_lines(wizard, app, inner, &mut lines),
    }
    let note = wizard
        .note
        .clone()
        .or_else(|| wizard.connect.as_ref().and_then(|c| c.error.clone()));
    let content_height = inner.height.saturating_sub(u16::from(note.is_some()));
    let anchor = match wizard.step {
        Step::Welcome => wizard.welcome_row + 2,
        Step::Provider => wizard.provider_pick + 2,
        Step::Connect => wizard
            .connect
            .as_ref()
            .map(|connect| {
                connect
                    .visible_fields()
                    .iter()
                    .position(|&index| index == connect.focus)
                    .unwrap_or(0)
                    + 4
            })
            .unwrap_or(0),
        Step::Done => 0,
    };
    let offset = if wizard.step == Step::Done {
        let offset = wizard
            .scroll
            .get()
            .min(lines.len().saturating_sub(content_height as usize));
        wizard.scroll.set(offset);
        offset
    } else {
        viewport_offset(lines.len(), content_height as usize, anchor)
    };
    let content = Rect {
        height: content_height,
        ..inner
    };
    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), content);
    if let Some(note) = note {
        let status = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                ui::clip(&note, inner.width as usize),
                Style::default().fg(ACCENT),
            )),
            status,
        );
    }
    if let Some((x, y)) = cursor {
        if content_height > 0 {
            frame.set_cursor_position((
                x.min(inner.x + inner.width - 1),
                y.saturating_sub(offset as u16)
                    .min(inner.y + content_height - 1),
            ));
        }
    }
}

fn viewport_offset(lines: usize, height: usize, selected: usize) -> usize {
    selected
        .saturating_add(1)
        .saturating_sub(height)
        .min(lines.saturating_sub(height))
}

/// What the bottom bar shows while this screen is open, one line per step.
pub fn keys(wizard: &Wizard) -> Vec<(String, String)> {
    let pair = |key: &str, action: &str| (key.to_string(), action.to_string());
    if wizard.verifying {
        return vec![pair("esc", "cancel the check")];
    }
    match wizard.step {
        Step::Welcome if wizard.first_run => vec![
            pair("↑↓", "move"),
            pair("space", "include"),
            pair("a", "connect another"),
            pair("enter", "continue"),
            pair("esc", "skip"),
        ],
        Step::Welcome => vec![
            pair("↑↓", "move"),
            pair("space", "remove"),
            pair("a", "connect another"),
            pair("enter", "continue"),
            pair("esc", "cancel"),
        ],
        Step::Provider => vec![
            pair("↑↓", "choose"),
            pair("enter", "connect"),
            pair("esc", "back"),
        ],
        Step::Connect => {
            let connect = wizard.connect.as_ref();
            let mut keys = Vec::new();
            if connect.is_some_and(|connect| connect.verified.is_some()) {
                keys.push(pair("enter", "save this account"));
            } else if connect.is_some_and(|connect| connect.error.is_some()) {
                keys.push(pair("enter", "retry"));
                keys.push(pair("ctrl+s", "save unchecked"));
            } else {
                keys.push(pair("enter", "check"));
            }
            keys.push(pair("tab/shift+tab", "field"));
            if connect.is_some_and(|connect| connect.advanced) {
                keys.push(pair("ctrl+r", "reveal"));
            } else {
                keys.push(pair("f2", "advanced"));
            }
            keys.push(pair("esc", "back"));
            keys
        }
        Step::Done => vec![
            pair("enter", "save"),
            pair("↑↓", "scroll"),
            pair("t", "sort"),
            pair("esc", "back"),
        ],
    }
}

fn welcome_lines(wizard: &Wizard, inner: Rect, lines: &mut Vec<Line>) {
    lines.push(Line::from(Span::styled(
        if !wizard.first_run {
            "Connect another account.".to_string()
        } else if wizard.detected.is_empty() {
            "No vendor credentials were found on this machine.".to_string()
        } else {
            format!(
                "{} credential{} found. Keep the ones usagebar should read.",
                wizard.detected.len(),
                if wizard.detected.len() == 1 { "" } else { "s" }
            )
        },
        Style::default().fg(TEXT),
    )));
    lines.push(Line::from(""));
    let width = inner.width as usize;
    let mut row = 0usize;
    for (index, entry) in wizard.detected.iter().enumerate() {
        let selected = row == wizard.welcome_row;
        let mark = if entry.credential.is_none() {
            "!"
        } else if wizard.picked[index] {
            "x"
        } else {
            " "
        };
        let detail = entry
            .origin
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "OpenCode key store".into());
        let detail = if entry.credential.is_none() {
            entry.error.clone().unwrap_or_default()
        } else {
            detail
        };
        lines.push(Line::from(vec![
            Span::styled(
                if selected { " ▸ " } else { "   " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("[{mark}] "),
                Style::default().fg(match mark {
                    "!" => Color::Rgb(0xD8, 0xA8, 0x57),
                    "x" => ACCENT,
                    _ => FAINT,
                }),
            ),
            Span::styled(
                ui::pad(entry.provider.display(), 14),
                Style::default().fg(TEXT).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(
                ui::clip(&detail, width.saturating_sub(22)),
                Style::default().fg(FAINT),
            ),
        ]));
        row += 1;
    }
    for pending in &wizard.pending {
        let selected = row == wizard.welcome_row;
        lines.push(Line::from(vec![
            Span::styled(
                if selected { " ▸ " } else { "   " },
                Style::default().fg(ACCENT),
            ),
            Span::styled("✓ ", Style::default().fg(Color::Rgb(0x5E, 0xB8, 0x8A))),
            Span::styled(
                ui::pad(
                    &format!(
                        "{} {}",
                        pending.account.provider.display(),
                        pending.account.label.clone().unwrap_or_default()
                    ),
                    width.saturating_sub(14),
                ),
                Style::default().fg(TEXT),
            ),
            Span::styled(ui::clip(&pending.summary, 24), Style::default().fg(FAINT)),
        ]));
        row += 1;
    }
    let selected = row == wizard.welcome_row;
    lines.push(Line::from(vec![
        Span::styled(
            if selected { " ▸ " } else { "   " },
            Style::default().fg(ACCENT),
        ),
        Span::styled(
            "+ connect another provider…",
            Style::default().fg(if selected { ACCENT } else { DIM }),
        ),
    ]));
}

fn provider_lines(wizard: &Wizard, inner: Rect, lines: &mut Vec<Line>) {
    lines.push(Line::from(Span::styled(
        "Which provider should usagebar connect?",
        Style::default().fg(TEXT),
    )));
    lines.push(Line::from(""));
    for (index, provider) in ProviderId::ALL.iter().enumerate() {
        let selected = index == wizard.provider_pick;
        lines.push(Line::from(vec![
            Span::styled(
                if selected { " ▸ " } else { "   " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                ui::pad(provider.display(), 14),
                Style::default().fg(TEXT).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(
                ui::clip(
                    login_instructions(*provider).0,
                    inner.width.saturating_sub(18) as usize,
                ),
                Style::default().fg(FAINT),
            ),
        ]));
    }
}

fn login_instructions(provider: ProviderId) -> (&'static str, &'static str) {
    match provider {
        ProviderId::Claude => (
            "Use your Claude Code login",
            "Sign in to Claude Code, then press enter. usagebar uses its saved login.",
        ),
        ProviderId::Codex => (
            "Use your Codex login",
            "Sign in to Codex, then press enter. usagebar uses its saved login.",
        ),
        ProviderId::OpenCodeGo => (
            "Use your OpenCode Go key",
            "Paste the Go API key from your OpenCode account.",
        ),
        ProviderId::Cursor => (
            "Use your Cursor agent login",
            "Sign in to Cursor agent, then press enter. usagebar uses its saved login.",
        ),
        ProviderId::Grok => (
            "Use your Grok CLI login",
            "Sign in to Grok CLI, then press enter. usagebar uses its saved login.",
        ),
        ProviderId::Devin => (
            "Use your Devin login",
            "Sign in to Devin, then press enter. usagebar uses its saved login.",
        ),
        ProviderId::CommandCode => (
            "Use your Command Code login",
            "Sign in to Command Code, then press enter. usagebar uses its saved login.",
        ),
    }
}

fn connect_lines(
    wizard: &Wizard,
    inner: Rect,
    lines: &mut Vec<Line>,
    cursor: &mut Option<(u16, u16)>,
) {
    let Some(connect) = wizard.connect.as_ref() else {
        return;
    };
    lines.push(Line::from(vec![Span::styled(
        format!("Connect {}", connect.provider.display()),
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(Span::styled(
        login_instructions(connect.provider).1,
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        if connect.provider == ProviderId::OpenCodeGo {
            "Manual connection · existing Go keys are detected during setup."
        } else if connect.advanced {
            "Advanced: pasted credentials override the login file."
        } else if connect.fields[0].input.is_blank() {
            "Sign in with the provider, or press F2 for advanced token entry."
        } else {
            "The usual login location is ready below. F2 opens advanced token entry."
        },
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(""));
    let field_width = inner.width.saturating_sub(20).max(10) as usize;
    for index in connect.visible_fields() {
        let field = &connect.fields[index];
        let focused = index == connect.focus;
        let (shown, column) = field.input.display(field_width);
        let y = inner.y + lines.len() as u16;
        lines.push(Line::from(vec![
            Span::styled(
                if focused { " ▸ " } else { "   " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                ui::pad(field.label, 17),
                Style::default().fg(if focused { TEXT } else { DIM }),
            ),
            Span::styled(
                if shown.is_empty() && focused {
                    "…".into()
                } else {
                    ui::clip(&shown, field_width)
                },
                Style::default().fg(if focused { TEXT } else { DIM }),
            ),
        ]));
        if focused {
            *cursor = Some((inner.x + 20 + column as u16, y));
        }
    }
    lines.push(Line::from(""));
    let status = match (&connect.verified, &connect.error) {
        (Some(summary), _) => Line::from(Span::styled(
            format!("✓ {summary}"),
            Style::default().fg(Color::Rgb(0x5E, 0xB8, 0x8A)),
        )),
        (None, Some(why)) => Line::from(Span::styled(
            format!("✗ {why}"),
            Style::default().fg(Color::Rgb(0xC9, 0x7B, 0x7B)),
        )),
        (None, None) => Line::from(Span::styled("not checked yet", Style::default().fg(FAINT))),
    };
    lines.push(status);
}

fn done_lines(wizard: &Wizard, app: &App, inner: Rect, lines: &mut Vec<Line>) {
    let total = wizard.total();
    lines.push(Line::from(Span::styled(
        format!(
            "{total} account{} will be saved.",
            if total == 1 { "" } else { "s" }
        ),
        Style::default().fg(TEXT),
    )));
    lines.push(Line::from(""));
    let mut shown = 0;
    for entry in wizard.selected_detected() {
        lines.push(Line::from(Span::styled(
            format!("   ✓ {}", entry.provider.display()),
            Style::default().fg(DIM),
        )));
        shown += 1;
    }
    for pending in &wizard.pending {
        let unchecked = pending.summary.starts_with("saved unchecked");
        lines.push(Line::from(vec![
            Span::styled(
                if unchecked { "   ! " } else { "   ✓ " },
                Style::default().fg(if unchecked {
                    Color::Rgb(0xD8, 0xA8, 0x57)
                } else {
                    Color::Rgb(0x5E, 0xB8, 0x8A)
                }),
            ),
            Span::styled(
                format!(
                    "{} {}",
                    pending.account.provider.display(),
                    pending.account.label.clone().unwrap_or_default()
                ),
                Style::default().fg(DIM),
            ),
            Span::styled(
                if unchecked {
                    format!("  {}", ui::clip(&pending.summary, 40))
                } else {
                    String::new()
                },
                Style::default().fg(Color::Rgb(0xD8, 0xA8, 0x57)),
            ),
        ]));
        shown += 1;
    }
    if shown == 0 {
        lines.push(Line::from(Span::styled(
            "   nothing selected — usagebar will scan the machine each run",
            Style::default().fg(FAINT),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("   Sort      ", Style::default().fg(DIM)),
        Span::styled(
            match wizard.sort {
                SortMode::Smart => "smart · worst first",
                SortMode::Manual => "manual · in the order shown",
            },
            Style::default().fg(TEXT),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("   Interval  ", Style::default().fg(DIM)),
        Span::styled(
            format!("{}s (change later in setup)", app.interval_secs),
            Style::default().fg(TEXT),
        ),
    ]));
    let _ = inner;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::FileStore;

    fn account(id: &str, provider: ProviderId) -> AccountRef {
        AccountRef::new(id, provider)
    }

    fn app_with(dir: &std::path::Path) -> App {
        use std::sync::Arc;
        let mut app = App::new(
            60,
            Arc::new(FileStore::load(dir.join("credentials.json"))),
            SortMode::Manual,
            Arc::new(FileStore::load(dir.join("credentials.json"))),
        );
        app.config_path = crate::config::config_path(dir);
        app.file_store = Arc::new(FileStore::load(dir.join("credentials.json")));
        app
    }

    #[test]
    fn failed_check_retries_on_enter_and_requires_explicit_unchecked_save() {
        let dir = std::env::temp_dir().join(format!("usagebar-wiz-retry-{}", std::process::id()));
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        wizard.step = Step::Connect;
        let mut form = Connect::new(ProviderId::Cursor);
        form.fields[0].input.set("");
        form.error = Some("previous failure".into());
        wizard.connect = Some(form);

        // An empty form fails locally; retrying never sends a network request.
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(wizard.step, Step::Connect);
        assert!(wizard.pending.is_empty());
        assert_eq!(
            wizard.connect.as_ref().unwrap().error.as_deref(),
            Some("Cursor login not found. Sign in with Cursor agent, then retry.")
        );

        wizard.connect.as_mut().unwrap().fields[1]
            .input
            .set("test-key");
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );
        assert_eq!(wizard.step, Step::Welcome);
        assert_eq!(wizard.pending.len(), 1);
        assert!(wizard.pending[0].summary.starts_with("saved unchecked:"));
        assert!(!app.config_path.exists());
    }

    #[test]
    fn login_form_hides_raw_tokens_until_advanced_and_supports_backtab() {
        let dir =
            std::env::temp_dir().join(format!("usagebar-wiz-navigation-{}", std::process::id()));
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        wizard.step = Step::Connect;
        wizard.connect = Some(Connect::new(ProviderId::Claude));
        assert_eq!(wizard.connect.as_ref().unwrap().visible_fields(), vec![3]);
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
        assert_eq!(wizard.connect.as_ref().unwrap().focus, 3);
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        );
        assert_eq!(wizard.connect.as_ref().unwrap().focus, 3);
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
        );
        assert_eq!(
            wizard.connect.as_ref().unwrap().visible_fields(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(wizard.connect.as_ref().unwrap().focus, 0);
    }

    #[test]
    fn provider_picker_keeps_last_selection_visible_in_short_pane() {
        use ratatui::{backend::TestBackend, Terminal};
        let dir = std::env::temp_dir().join(format!("usagebar-wiz-scroll-{}", std::process::id()));
        let app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        wizard.step = Step::Provider;
        wizard.provider_pick = ProviderId::ALL.len() - 1;
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &wizard, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("▸ Command Code"), "{text}");
    }

    #[test]
    fn go_manual_setup_offers_a_key_instead_of_an_unsupported_file_import() {
        let form = Connect::new(ProviderId::OpenCodeGo);
        assert_eq!(form.visible_fields(), vec![1, 2]);
        assert_eq!(form.fields[form.focus].label, "Go API key");
    }

    #[test]
    fn finish_screen_scrolls_immediately_and_clamps_at_the_last_line() {
        use ratatui::{backend::TestBackend, Terminal};
        let dir =
            std::env::temp_dir().join(format!("usagebar-wiz-finish-scroll-{}", std::process::id()));
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new(
            (0..12)
                .map(|_| Detected {
                    provider: ProviderId::Cursor,
                    credential: Some(Credential::Token {
                        token: "test-key".into(),
                    }),
                    origin: None,
                    error: None,
                })
                .collect(),
            SortMode::Manual,
        );
        wizard.step = Step::Done;
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &wizard, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!text.contains("12 accounts will be saved"));
        wizard.scroll.set(usize::MAX);
        terminal
            .draw(|frame| draw(frame, &app, &wizard, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Interval"));
        assert!(wizard.scroll.get() < 17);
    }

    #[test]
    fn building_from_fields_prefers_a_paste_then_a_file() {
        let dir = std::env::temp_dir().join(format!("usagebar-wiz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("auth.json");
        std::fs::write(&file, r#"{"accessToken":"from-file"}"#).unwrap();

        let mut connect = Connect::new(ProviderId::Cursor);
        connect.fields[0].input.set(file.display().to_string());
        let (credential, origin) = connect.build(ProviderId::Cursor).unwrap();
        assert_eq!(
            credential,
            Credential::Token {
                token: "from-file".into()
            }
        );
        assert_eq!(origin.as_deref(), Some(file.as_path()));

        // A pasted token wins over the prefilled path, so nothing has to be cleared.
        connect.fields[1].input.set("pasted");
        let (credential, origin) = connect.build(ProviderId::Cursor).unwrap();
        assert_eq!(
            credential,
            Credential::Token {
                token: "pasted".into()
            }
        );
        assert!(origin.is_none());

        connect.fields[0].input.set("");
        connect.fields[1].input.set("");
        assert!(connect.build(ProviderId::Cursor).is_err());

        // Claude needs the pair, not just half of it.
        let mut connect = Connect::new(ProviderId::Claude);
        connect.fields[0].input.set("");
        connect.fields[1].input.set("access-only");
        assert!(connect.build(ProviderId::Claude).is_err());
        connect.fields[2].input.set("refresh");
        assert!(connect.build(ProviderId::Claude).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finishing_writes_config_and_credentials() {
        let dir = std::env::temp_dir().join(format!("usagebar-wiz2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = app_with(&dir);
        let detected = vec![
            Detected {
                provider: ProviderId::Cursor,
                credential: Some(Credential::Token { token: "t1".into() }),
                origin: None,
                error: None,
            },
            Detected {
                provider: ProviderId::Grok,
                credential: None,
                origin: None,
                error: Some("nothing usable".into()),
            },
        ];
        let mut wizard = Wizard::new(detected, SortMode::Smart);
        // The unreadable entry starts unchecked and stays out.
        assert_eq!(wizard.total(), 1);
        finish(&mut wizard, &mut app).unwrap();

        let saved = crate::config::load(&app.config_path).unwrap().unwrap();
        assert_eq!(saved.accounts.len(), 1);
        assert_eq!(saved.accounts[0].provider, ProviderId::Cursor);
        assert_eq!(saved.sort, SortMode::Smart);
        assert!(app
            .file_store
            .get("cursor")
            .is_some_and(|stored| stored.secret == Credential::Token { token: "t1".into() }));
        assert!(app.persisted);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kept_accounts_and_new_ones_both_land_in_the_config() {
        let dir = std::env::temp_dir().join(format!("usagebar-wiz3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = app_with(&dir);
        app.config.accounts = vec![account("claude", ProviderId::Claude)];
        app.file_store
            .put(
                "claude",
                StoredCredential::new(Credential::Token {
                    token: "old".into(),
                }),
            )
            .unwrap();
        let mut wizard = Wizard::new(vec![], SortMode::Manual);
        wizard.pending.push(Pending {
            account: account("codex", ProviderId::Codex),
            credential: Credential::CodexTokens {
                access_token: "at".into(),
                account_id: None,
                refresh_token: None,
            },
            origin: None,
            summary: "checked".into(),
        });
        finish(&mut wizard, &mut app).unwrap();
        let saved = crate::config::load(&app.config_path).unwrap().unwrap();
        assert_eq!(
            saved
                .accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude", "codex"]
        );
        assert!(app.file_store.get("codex").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
