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
    /// A browser sign-in in flight, if the user started one.
    pub signin: Option<SignIn>,
    /// The credential that sign-in produced. It was already read back successfully, so
    /// saving it needs no further check.
    pub signed_in: Option<Credential>,
}

/// A sign-in running in the background: what to open, where the code goes, and the
/// worker that turns the answer into a credential.
pub struct SignIn {
    inner: crate::oauth::SignIn,
    /// Where a vendor that shows the code instead of calling back puts it.
    pub code: TextInput,
    /// True while only the browser can move this along.
    pub waiting: bool,
    rx: Option<Receiver<Result<(Credential, Report), String>>>,
}

impl SignIn {
    /// What the screen says while this is running.
    pub fn status(&self) -> String {
        if self.waiting {
            return "waiting for the browser to come back…".into();
        }
        if self.rx.is_some() {
            return "exchanging the code…".into();
        }
        "approve in the browser, then paste the code it shows".into()
    }
}

/// What a connect field means. The form is assembled per provider, so everything else
/// asks for a role instead of an index: OpenCode Go has no vendor file to point at, which
/// makes its form shorter than the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Not a field at all: the row that starts the browser sign-in, for the providers
    /// that have one.
    SignIn,
    /// A vendor file to import from, for the providers that keep one.
    Config,
    Access,
    Refresh,
    AccountId,
    Token,
    Name,
}

pub struct Field {
    pub role: Role,
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

    /// Called every tick so a background check or sign-in can land without blocking the
    /// UI.
    pub fn poll(&mut self) {
        self.poll_check();
        self.poll_signin();
    }

    fn poll_check(&mut self) {
        let outcome = match &self.verify_rx {
            Some(rx) => match rx.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err("the check stopped before it answered".into()))
                }
            },
            None => None,
        };
        let Some(outcome) = outcome else {
            return;
        };
        self.verifying = false;
        self.verify_rx = None;
        match outcome {
            Ok(report) => {
                let summary = match self.connect.as_mut() {
                    Some(connect) => adopt(connect, &report),
                    None => String::new(),
                };
                self.note = Some(format!("checked: {summary}"));
            }
            Err(why) => {
                self.note = None;
                if let Some(connect) = self.connect.as_mut() {
                    connect.error = Some(why);
                    connect.verified = None;
                }
            }
        }
    }

    /// A sign-in that has run its course: the credential it produced, and the account it
    /// read back.
    fn poll_signin(&mut self) {
        let outcome = {
            let Some(rx) = self
                .connect
                .as_ref()
                .and_then(|connect| connect.signin.as_ref())
                .and_then(|signin| signin.rx.as_ref())
            else {
                return;
            };
            match rx.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    Err("the sign-in stopped before it answered".into())
                }
            }
        };
        let Some(connect) = self.connect.as_mut() else {
            return;
        };
        connect.signin = None;
        match outcome {
            Ok((credential, report)) => {
                connect.signed_in = Some(credential);
                let summary = adopt(connect, &report);
                self.note = Some(format!("signed in: {summary}"));
            }
            Err(why) => {
                connect.signed_in = None;
                connect.verified = None;
                connect.error = Some(why);
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(connect) = self.connect.as_mut() {
            // While a sign-in is open, the pasted text is the code from the browser.
            if let Some(signin) = connect.signin.as_mut() {
                signin.code.paste(text);
                return;
            }
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

/// The provider of the selected scan row, when the scan found the file but no usable
/// credential in it. Enter connects that provider by hand rather than continuing.
fn unusable_provider(wizard: &Wizard) -> Option<ProviderId> {
    wizard
        .detected
        .get(wizard.welcome_row)
        .filter(|entry| entry.credential.is_none())
        .map(|entry| entry.provider)
}

/// Put the vendor's answer on the connect form: the line that says what was found, and
/// the account's own name when the form has not been given one.
fn adopt(connect: &mut Connect, report: &Report) -> String {
    let who = report.account.clone().or_else(|| report.label.clone());
    let plan = report.plan.clone().unwrap_or_default();
    let summary = match (who, plan.is_empty()) {
        (Some(who), false) if who == plan => plan,
        (Some(who), false) => format!("{who} · {plan}"),
        (Some(who), true) => who,
        (None, false) => plan,
        (None, true) => "connected".into(),
    };
    connect.verified = Some(summary.clone());
    connect.error = None;
    if let Some(name) = connect
        .fields
        .iter_mut()
        .find(|field| field.role == Role::Name)
        .filter(|field| field.input.is_blank())
    {
        if let Some(account) = &report.account {
            name.input.set(account.clone());
        }
    }
    summary
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
            if let Some(provider) = unusable_provider(wizard) {
                // Nothing to include here, but this row is the whole reason the provider
                // is missing: open its form so the credential can be typed in.
                wizard.connect = Some(Connect::new(provider));
                wizard.step = Step::Connect;
                wizard.note = None;
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
    // A sign-in owns the keys while it is open: it is the only thing on this screen that
    // can still change, and the field it fills is not one of the form's own.
    if connect.signin.is_some() {
        return signin_keys(wizard, key);
    }
    match key.code {
        KeyCode::Esc => {
            wizard.step = Step::Provider;
            wizard.connect = None;
            wizard.note = None;
        }
        KeyCode::F(2) => {
            connect.advanced = true;
            // The advanced form opens on its first typable field, past the sign-in row.
            connect.focus = connect
                .fields
                .iter()
                .position(|field| field.role != Role::SignIn)
                .unwrap_or(0);
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
            // The sign-in row is the one row on the form that answers enter with an
            // action rather than a check.
            if connect
                .fields
                .get(connect.focus)
                .is_some_and(|field| field.role == Role::SignIn)
            {
                return start_signin(wizard);
            }
            // A failed check retries on enter; ctrl+s is the explicit unchecked save.
            if connect.verified.is_some() {
                return save_connect(wizard, app);
            }
            return check(wizard);
        }
        _ => {
            let Some(field) = connect.fields.get_mut(connect.focus) else {
                return Action::Keep;
            };
            // The sign-in row holds nothing to type into, like the setting rows that are
            // not fields: it answers to the arrow keys and to enter, and to nothing else.
            if field.role == Role::SignIn {
                return Action::Keep;
            }
            if field.input.handle_key(key) {
                // Typing in the name does not invalidate the check; changing a
                // credential does, and a credential that came from a sign-in too.
                if field.role != Role::Name {
                    connect.verified = None;
                    connect.signed_in = None;
                }
                connect.error = None;
                wizard.note = None;
            }
        }
    }
    Action::Keep
}

/// Open the vendor's own sign-in page and listen for what it sends back.
fn start_signin(wizard: &mut Wizard) -> Action {
    let Some(connect) = wizard.connect.as_mut() else {
        return Action::Keep;
    };
    let Some(spec) = providers::oauth_spec(connect.provider) else {
        connect.error = Some(format!(
            "{} has no browser sign-in",
            connect.provider.display()
        ));
        return Action::Keep;
    };
    let signin = match crate::oauth::SignIn::start(spec) {
        Ok(signin) => signin,
        Err(why) => {
            connect.error = Some(why);
            return Action::Keep;
        }
    };
    // The port is taken before the browser opens, so a sign-in that cannot be received
    // fails here rather than in the browser.
    let (rx, waiting) = if signin.calls_back() {
        let provider = connect.provider;
        let verifier = signin.verifier().to_string();
        match signin.listen() {
            Ok(code_rx) => {
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let outcome = match code_rx.recv() {
                        Ok(Ok(code)) => finish_signin(provider, &code, &verifier),
                        Ok(Err(why)) => Err(why),
                        // The listener only goes away when the user stopped waiting,
                        // which the wizard has already done by then.
                        Err(_) => Err("the sign-in stopped before the browser answered".into()),
                    };
                    let _ = tx.send(outcome);
                });
                (Some(rx), true)
            }
            Err(why) => {
                connect.error = Some(why);
                return Action::Keep;
            }
        }
    } else {
        (None, false)
    };
    let url = signin.url.clone();
    connect.signed_in = None;
    connect.verified = None;
    connect.error = None;
    connect.signin = Some(SignIn {
        inner: signin,
        code: TextInput::new(),
        waiting,
        rx,
    });
    crate::oauth::open_browser(&url);
    wizard.note = Some(format!("signing in · {url}"));
    Action::Keep
}

/// While a sign-in is open, every key belongs to it.
fn signin_keys(wizard: &mut Wizard, key: KeyEvent) -> Action {
    let Some(connect) = wizard.connect.as_mut() else {
        return Action::Keep;
    };
    match key.code {
        KeyCode::Esc => {
            if let Some(signin) = connect.signin.take() {
                signin.inner.cancel();
            }
            connect.signed_in = None;
            wizard.note = None;
        }
        KeyCode::Enter => {
            let Some(signin) = connect.signin.as_mut() else {
                return Action::Keep;
            };
            if signin.waiting || signin.code.is_blank() {
                return Action::Keep;
            }
            // The code the browser showed, which is all this flow can hand back.
            let code = pasted_code(signin.code.value());
            let verifier = signin.inner.verifier().to_string();
            let provider = connect.provider;
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(finish_signin(provider, &code, &verifier));
            });
            signin.rx = Some(rx);
        }
        _ => {
            if let Some(signin) = connect.signin.as_mut() {
                signin.code.handle_key(key);
            }
        }
    }
    Action::Keep
}

/// Trade the code for a credential and read the account once, so what the form shows is
/// what the vendor will actually answer with.
fn finish_signin(
    provider: ProviderId,
    code: &str,
    verifier: &str,
) -> Result<(Credential, Report), String> {
    let credential = providers::oauth_exchange(provider, code, verifier)?;
    let report = providers::verify(provider, &credential, None)?;
    Ok((credential, report))
}

/// The code the browser showed, as the token exchange wants it. Claude's page hands back
/// "code#state", and the code is the part before the hash: the state there is the one the
/// page echoed, not the one this sign-in generated.
fn pasted_code(value: &str) -> String {
    value
        .split('#')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
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
        let (credential, origin) = match connect.signed_in.clone() {
            // A signed-in credential has no file behind it, and needs no check: it has
            // already been read back once.
            Some(credential) => (credential, None),
            None => match connect.build(connect.provider) {
                Ok(built) => built,
                Err(why) => {
                    if let Some(connect) = wizard.connect.as_mut() {
                        connect.error = Some(why);
                    }
                    return Action::Keep;
                }
            },
        };
        let label = match connect.value(Role::Name) {
            name if name.is_empty() => None,
            name => Some(name),
        };
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
        let mut fields = Vec::new();
        // A sign-in comes first on the screen, since it is the one way in that does not
        // need anything on this machine.
        if providers::oauth_spec(provider).is_some() {
            fields.push(Field::new(Role::SignIn, "Sign in", TextInput::new()));
        }
        // The usual login location stays prefilled even before the file exists, so
        // signing in with the vendor's CLI and pressing enter is all it takes.
        if let Some(path) = credentials::vendor_file(provider) {
            fields.push(Field::new(
                Role::Config,
                "Login file",
                TextInput::with_value(path.display().to_string()),
            ));
        }
        match provider {
            ProviderId::Claude => {
                fields.push(Field::new(
                    Role::Access,
                    "Access token",
                    TextInput::new().secret(),
                ));
                fields.push(Field::new(
                    Role::Refresh,
                    "Refresh token",
                    TextInput::new().secret(),
                ));
            }
            ProviderId::Codex => {
                fields.push(Field::new(
                    Role::Access,
                    "Access token",
                    TextInput::new().secret(),
                ));
                fields.push(Field::new(Role::AccountId, "Account id", TextInput::new()));
            }
            // A Go key is not a file: the scan reads the keys the OpenCode store holds,
            // and a key that is anywhere else is simply pasted in. One key, one account.
            ProviderId::OpenCodeGo => {
                fields.push(Field::new(
                    Role::Token,
                    "Go API key",
                    TextInput::new().secret(),
                ));
            }
            _ => {
                let label = match provider {
                    ProviderId::Cursor => "Access token",
                    ProviderId::Grok => "Session key",
                    _ => "API key",
                };
                fields.push(Field::new(Role::Token, label, TextInput::new().secret()));
            }
        }
        fields.push(Field::new(Role::Name, "Name (optional)", TextInput::new()));
        let mut connect = Self {
            provider,
            fields,
            focus: 0,
            advanced: provider == ProviderId::OpenCodeGo,
            verified: None,
            error: None,
            signin: None,
            signed_in: None,
        };
        // The focus opens on the first visible field that can be typed into.
        connect.focus = connect
            .visible_fields()
            .into_iter()
            .find(|&index| connect.fields[index].role != Role::SignIn)
            .unwrap_or(0);
        connect
    }

    /// The rows the form shows. Raw credentials wait behind F2, so the plain form is the
    /// browser sign-in (where there is one) and the name; OpenCode Go has no login file,
    /// so its key is always shown.
    fn visible_fields(&self) -> Vec<usize> {
        (0..self.fields.len())
            .filter(|&index| {
                self.advanced || matches!(self.fields[index].role, Role::SignIn | Role::Name)
            })
            .collect()
    }

    /// What a field holds, trimmed. Roles rather than positions, so dropping a field for
    /// one provider cannot shift what another provider's field means.
    fn value(&self, role: Role) -> String {
        self.fields
            .iter()
            .find(|field| field.role == role)
            .map(|field| field.input.value().trim().to_string())
            .unwrap_or_default()
    }

    /// A pasted secret wins over the file field, so the prefilled path stays a
    /// convenience rather than something to clear out first.
    pub fn build(&self, provider: ProviderId) -> Result<(Credential, Option<PathBuf>), String> {
        let path = self.value(Role::Config);
        let pasted = match provider {
            ProviderId::Claude => {
                let (access, refresh) = (self.value(Role::Access), self.value(Role::Refresh));
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
                let access = self.value(Role::Access);
                if access.is_empty() {
                    None
                } else {
                    let account_id = self.value(Role::AccountId);
                    Some(Credential::CodexTokens {
                        access_token: access,
                        account_id: (!account_id.is_empty()).then_some(account_id),
                        refresh_token: None,
                        expires_at: 0,
                    })
                }
            }
            _ => {
                let token = self.value(Role::Token);
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
    fn new(role: Role, label: &'static str, input: TextInput) -> Self {
        Self { role, label, input }
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
            .map(|connect| {
                connect.visible_fields().len()
                    + 10
                    + usize::from(
                        connect
                            .signin
                            .as_ref()
                            .is_some_and(|signin| !signin.waiting),
                    )
            })
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
    // On a row the scan could not make a credential out of, enter opens that provider's
    // form instead of moving on, and the bar says so.
    let enter = match unusable_provider(wizard) {
        Some(_) => "connect it",
        None => "continue",
    };
    match wizard.step {
        Step::Welcome if wizard.first_run => vec![
            pair("↑↓", "move"),
            pair("space", "include"),
            pair("a", "connect another"),
            pair("enter", enter),
            pair("esc", "skip"),
        ],
        Step::Welcome => vec![
            pair("↑↓", "move"),
            pair("space", "remove"),
            pair("a", "connect another"),
            pair("enter", enter),
            pair("esc", "cancel"),
        ],
        Step::Provider => vec![
            pair("↑↓", "choose"),
            pair("enter", "connect"),
            pair("esc", "back"),
        ],
        Step::Connect => {
            let connect = wizard.connect.as_ref();
            if let Some(signin) = connect.and_then(|connect| connect.signin.as_ref()) {
                let mut keys = vec![pair("esc", "stop waiting")];
                if !signin.waiting {
                    keys.insert(0, pair("enter", "use the code"));
                    keys.push(pair("ctrl+r", "reveal"));
                }
                return keys;
            }
            let mut keys = Vec::new();
            if connect.is_some_and(|connect| {
                connect
                    .fields
                    .get(connect.focus)
                    .is_some_and(|field| field.role == Role::SignIn)
            }) {
                keys.push(pair("enter", "sign in with a browser"));
            } else if connect.is_some_and(|connect| connect.verified.is_some()) {
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
                ui::pad(&pending.account.name(), width.saturating_sub(14)),
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
            "Sign in with a browser, or use your Claude Code login",
            "Sign in with a browser, or sign in to Claude Code and press enter.",
        ),
        ProviderId::Codex => (
            "Sign in with a browser, or use your Codex login",
            "Sign in with a browser, or sign in to Codex and press enter.",
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
        } else if connect.value(Role::Config).is_empty() {
            "Sign in with the provider, or press F2 for advanced token entry."
        } else {
            "The usual login location is ready below. F2 opens advanced token entry."
        },
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(""));
    let field_width = inner.width.saturating_sub(20).max(10) as usize;
    // A sign-in puts the code it needs under the form, where the form's own fields end.
    let signing_in = connect.signin.as_ref().filter(|signin| !signin.waiting);
    for index in connect.visible_fields() {
        let field = &connect.fields[index];
        let focused = index == connect.focus && connect.signin.is_none();
        // The sign-in row is an action, not a field: it reads like the rows that add
        // things rather than the ones that hold something.
        if field.role == Role::SignIn {
            lines.push(Line::from(vec![
                Span::styled(
                    if focused { " ▸ " } else { "   " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    "+ sign in with a browser…",
                    Style::default().fg(if focused { ACCENT } else { DIM }),
                ),
            ]));
            continue;
        }
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
    if let Some(signin) = signing_in {
        let (shown, column) = signin.code.display(field_width);
        let y = inner.y + lines.len() as u16;
        lines.push(Line::from(vec![
            Span::styled(" ▸ ", Style::default().fg(ACCENT)),
            Span::styled(ui::pad("Sign-in code", 17), Style::default().fg(TEXT)),
            Span::styled(
                if shown.is_empty() {
                    "…".into()
                } else {
                    ui::clip(&shown, field_width)
                },
                Style::default().fg(TEXT),
            ),
        ]));
        *cursor = Some((inner.x + 20 + column as u16, y));
    }
    lines.push(Line::from(""));
    let status = match (&connect.signin, &connect.verified, &connect.error) {
        (Some(signin), _, _) => Line::from(Span::styled(
            format!("◌ {}", signin.status()),
            Style::default().fg(ACCENT),
        )),
        (None, Some(summary), _) => Line::from(Span::styled(
            format!("✓ {summary}"),
            Style::default().fg(Color::Rgb(0x5E, 0xB8, 0x8A)),
        )),
        (None, None, Some(why)) => Line::from(Span::styled(
            format!("✗ {why}"),
            Style::default().fg(Color::Rgb(0xC9, 0x7B, 0x7B)),
        )),
        (None, None, None) => {
            Line::from(Span::styled("not checked yet", Style::default().fg(FAINT)))
        }
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
            Span::styled(pending.account.name(), Style::default().fg(DIM)),
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
    use crossterm::event::KeyModifiers;

    fn account(id: &str, provider: ProviderId) -> AccountRef {
        AccountRef::new(id, provider)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("usagebar-wiz-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Type into the field that means this, whichever position it ended up in.
    fn set(connect: &mut Connect, role: Role, value: &str) {
        connect
            .fields
            .iter_mut()
            .find(|field| field.role == role)
            .expect("the form has that field")
            .input
            .set(value);
    }

    fn press(wizard: &mut Wizard, app: &mut App, code: KeyCode) -> Action {
        handle(wizard, app, KeyEvent::new(code, KeyModifiers::NONE))
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
        // The plain form is the browser sign-in row and the name.
        assert_eq!(
            wizard.connect.as_ref().unwrap().visible_fields(),
            vec![0, 4]
        );
        assert_eq!(wizard.connect.as_ref().unwrap().focus, 4);
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
        assert_eq!(wizard.connect.as_ref().unwrap().focus, 4);
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        );
        assert_eq!(wizard.connect.as_ref().unwrap().focus, 0);
        handle(
            &mut wizard,
            &mut app,
            KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
        );
        assert_eq!(
            wizard.connect.as_ref().unwrap().visible_fields(),
            vec![0, 1, 2, 3, 4]
        );
        // Advanced opens on the login file, past the sign-in row.
        let connect = wizard.connect.as_ref().unwrap();
        assert_eq!(connect.fields[connect.focus].role, Role::Config);
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
        assert_eq!(form.visible_fields(), vec![0, 1]);
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
        let dir = scratch("build");
        let file = dir.join("auth.json");
        std::fs::write(&file, r#"{"accessToken":"from-file"}"#).unwrap();

        let mut connect = Connect::new(ProviderId::Cursor);
        set(&mut connect, Role::Config, &file.display().to_string());
        let (credential, origin) = connect.build(ProviderId::Cursor).unwrap();
        assert_eq!(
            credential,
            Credential::Token {
                token: "from-file".into()
            }
        );
        assert_eq!(origin.as_deref(), Some(file.as_path()));

        // A pasted token wins over the prefilled path, so nothing has to be cleared.
        set(&mut connect, Role::Token, "pasted");
        let (credential, origin) = connect.build(ProviderId::Cursor).unwrap();
        assert_eq!(
            credential,
            Credential::Token {
                token: "pasted".into()
            }
        );
        assert!(origin.is_none());

        set(&mut connect, Role::Config, "");
        set(&mut connect, Role::Token, "");
        assert!(connect.build(ProviderId::Cursor).is_err());

        // Claude needs the pair, not just half of it.
        let mut connect = Connect::new(ProviderId::Claude);
        set(&mut connect, Role::Config, "");
        set(&mut connect, Role::Access, "access-only");
        assert!(connect.build(ProviderId::Claude).is_err());
        set(&mut connect, Role::Refresh, "refresh");
        assert!(connect.build(ProviderId::Claude).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// OpenCode Go keeps no credential file of its own, so its form is the key itself:
    /// no path to point at, and a key that no store on the machine has to know about.
    #[test]
    fn an_opencode_go_key_is_typed_in_rather_than_imported() {
        let mut connect = Connect::new(ProviderId::OpenCodeGo);
        assert!(!connect
            .fields
            .iter()
            .any(|field| field.role == Role::Config));
        assert_eq!(connect.fields[0].role, Role::Token);
        assert_eq!(connect.fields[0].label, "Go API key");
        assert_eq!(
            connect.build(ProviderId::OpenCodeGo).unwrap_err(),
            "Paste your OpenCode Go API key."
        );

        set(&mut connect, Role::Token, "  sk-go-somewhere-else  ");
        let (credential, origin) = connect.build(ProviderId::OpenCodeGo).unwrap();
        assert_eq!(
            credential,
            Credential::Token {
                token: "sk-go-somewhere-else".into()
            }
        );
        // Nothing to follow it back to: the key is the whole credential.
        assert!(origin.is_none());
    }

    /// The scan can find OpenCode installed without finding a Go key it can use. That
    /// row is where the key gets typed in, so enter on it opens the provider's form.
    #[test]
    fn a_row_with_no_credential_opens_its_connect_screen() {
        let dir = scratch("unusable-row");
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new(
            vec![Detected {
                provider: ProviderId::OpenCodeGo,
                credential: None,
                origin: None,
                error: Some("no working OpenCode Go key in the local store".into()),
            }],
            SortMode::Manual,
        );
        // The row cannot be included, and the guide says what enter does with it.
        assert_eq!(wizard.total(), 0);
        assert!(keys(&wizard).contains(&("enter".to_string(), "connect it".to_string())));

        assert!(matches!(
            press(&mut wizard, &mut app, KeyCode::Enter),
            Action::Keep
        ));
        assert_eq!(wizard.step, Step::Connect);
        let connect = wizard.connect.as_ref().unwrap();
        assert_eq!(connect.provider, ProviderId::OpenCodeGo);
        assert_eq!(connect.fields[0].role, Role::Token);

        // Backing out returns to the list rather than saving anything.
        press(&mut wizard, &mut app, KeyCode::Esc);
        assert_eq!(wizard.step, Step::Provider);
        assert_eq!(wizard.total(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A key typed in by hand becomes an account of its own, named as the user named it.
    #[test]
    fn a_typed_go_key_becomes_an_account() {
        let dir = scratch("go-account");
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        let mut connect = Connect::new(ProviderId::OpenCodeGo);
        set(&mut connect, Role::Token, "sk-go-1");
        set(&mut connect, Role::Name, " shared key ");
        connect.verified = Some("connected".into());
        wizard.connect = Some(connect);

        assert!(matches!(save_connect(&mut wizard, &mut app), Action::Keep));
        assert_eq!(wizard.step, Step::Welcome);
        assert_eq!(wizard.pending.len(), 1);
        let pending = &wizard.pending[0];
        assert_eq!(pending.account.provider, ProviderId::OpenCodeGo);
        assert_eq!(pending.account.id, "opencode-go");
        assert_eq!(pending.account.label.as_deref(), Some("shared key"));
        assert_eq!(
            pending.credential,
            Credential::Token {
                token: "sk-go-1".into()
            }
        );
        assert!(pending.origin.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two keys typed in by hand are two accounts, not one that replaces the other.
    #[test]
    fn each_typed_go_key_gets_its_own_account() {
        let dir = scratch("go-accounts");
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        for key in ["sk-go-1", "sk-go-2"] {
            let mut connect = Connect::new(ProviderId::OpenCodeGo);
            set(&mut connect, Role::Token, key);
            connect.verified = Some("connected".into());
            wizard.connect = Some(connect);
            save_connect(&mut wizard, &mut app);
        }
        assert_eq!(
            wizard
                .pending
                .iter()
                .map(|pending| pending.account.id.as_str())
                .collect::<Vec<_>>(),
            vec!["opencode-go", "opencode-go-2"]
        );
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
                expires_at: 0,
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

    /// The Go form draws the key where the other providers draw their file field, and
    /// says what to do with it.
    #[test]
    fn the_go_form_draws_a_key_field_and_no_path() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let dir = scratch("go-draw");
        let app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        wizard.step = Step::Connect;
        wizard.connect = Some(Connect::new(ProviderId::OpenCodeGo));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &wizard, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(text.contains("Connect OpenCode Go"), "{text}");
        assert!(text.contains("API key"), "{text}");
        assert!(!text.contains("Login file"), "{text}");
        // And the connect form is what the bottom bar is describing.
        assert!(keys(&wizard).contains(&("enter".to_string(), "check".to_string())));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The guide follows the row the focus is on: the sign-in row answers enter with a
    /// sign-in, and a field answers it with a check.
    #[test]
    fn the_guide_follows_the_focused_row() {
        let dir = scratch("signin-guide");
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        wizard.step = Step::Connect;
        let mut connect = Connect::new(ProviderId::Codex);
        connect.focus = connect
            .fields
            .iter()
            .position(|field| field.role == Role::SignIn)
            .unwrap();
        wizard.connect = Some(connect);
        assert!(
            keys(&wizard).contains(&("enter".to_string(), "sign in with a browser".to_string()))
        );

        press(&mut wizard, &mut app, KeyCode::Down);
        let connect = wizard.connect.as_ref().unwrap();
        assert_eq!(connect.fields[connect.focus].role, Role::Name);
        let guide = keys(&wizard);
        assert!(guide.contains(&("enter".to_string(), "check".to_string())));
        assert!(!guide.iter().any(|(key, _)| key == "o"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The sign-in is a row of the form, not only a key in the guide: at 60 columns the
    /// guide has already dropped entries, and an option that disappears with the width
    /// is an option nobody finds.
    #[test]
    fn the_sign_in_row_is_on_the_form_whatever_the_width() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        for provider in ProviderId::ALL {
            let dir = scratch(&format!("signin-row-{}", provider.slug()));
            let app = app_with(&dir);
            let mut wizard = Wizard::new_add(SortMode::Manual);
            wizard.step = Step::Connect;
            let connect = Connect::new(provider);
            let row = connect
                .fields
                .iter()
                .position(|field| field.role == Role::SignIn);
            assert_eq!(
                row.is_some(),
                matches!(provider, ProviderId::Codex | ProviderId::Claude),
                "{} rows",
                provider.slug()
            );
            if row.is_some() {
                // It leads the screen, and the focus starts past it on something typable.
                assert_eq!(row, Some(0));
                assert_ne!(connect.focus, 0);
                assert_ne!(connect.fields[connect.focus].role, Role::SignIn);
            }
            wizard.connect = Some(connect);

            let mut terminal = Terminal::new(TestBackend::new(60, 26)).unwrap();
            terminal
                .draw(|frame| draw(frame, &app, &wizard, frame.area()))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
                .collect();
            assert_eq!(
                text.contains("sign in with a browser"),
                row.is_some(),
                "{} drew: {text}",
                provider.slug()
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// Signing in does not go through the field check: the credential that comes back is
    /// the one that gets saved, with no file behind it.
    #[test]
    fn a_signed_in_credential_is_what_gets_saved() {
        let dir = scratch("signed-in");
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        let mut connect = Connect::new(ProviderId::Claude);
        connect.signed_in = Some(Credential::ClaudeOauth {
            access_token: "oat".into(),
            refresh_token: "ort".into(),
            expires_at: 4_000_000_000_000,
            subscription_type: Some("max".into()),
        });
        connect.verified = Some("signed in as a@b.c · max".into());
        set(&mut connect, Role::Name, "work");
        wizard.connect = Some(connect);

        assert!(matches!(save_connect(&mut wizard, &mut app), Action::Keep));
        let pending = &wizard.pending[0];
        assert_eq!(pending.account.id, "claude");
        assert_eq!(pending.account.label.as_deref(), Some("work"));
        assert!(
            pending.origin.is_none(),
            "a sign-in has no file to point at"
        );
        assert_eq!(
            pending.credential,
            Credential::ClaudeOauth {
                access_token: "oat".into(),
                refresh_token: "ort".into(),
                expires_at: 4_000_000_000_000,
                subscription_type: Some("max".into()),
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Typing over a credential after signing in drops the signed-in pair, so what is
    /// saved is what the form shows.
    #[test]
    fn editing_a_credential_field_forgets_the_sign_in() {
        let dir = scratch("signin-edit");
        let mut app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        let mut connect = Connect::new(ProviderId::Claude);
        connect.signed_in = Some(Credential::ClaudeOauth {
            access_token: "oat".into(),
            refresh_token: "ort".into(),
            expires_at: 0,
            subscription_type: None,
        });
        connect.focus = connect
            .fields
            .iter()
            .position(|field| field.role == Role::Access)
            .unwrap();
        wizard.connect = Some(connect);
        wizard.step = Step::Connect;

        press(&mut wizard, &mut app, KeyCode::Char('x'));
        assert!(wizard.connect.as_ref().unwrap().signed_in.is_none());
        // And the form is back to being the source of the credential.
        assert!(matches!(save_connect(&mut wizard, &mut app), Action::Keep));
        assert!(wizard.pending.is_empty(), "an unfinished pair is not saved");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The code a browser shows is carried back by hand for Claude, and a pasted
    /// "code#state" is split the way the vendor's own CLI splits it.
    #[test]
    fn a_pasted_code_is_split_at_the_hash() {
        assert_eq!(pasted_code("abc-123#state-from-the-page"), "abc-123");
        assert_eq!(pasted_code("  abc-123  "), "abc-123");
        assert_eq!(pasted_code(""), "");
        assert_eq!(pasted_code("#only-state"), "");
    }

    /// While a sign-in is open the screen says what it is waiting for, offers the field
    /// the browser's code goes in, and takes the keys for itself.
    #[test]
    fn a_sign_in_in_flight_is_what_the_screen_shows() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let dir = scratch("signin-draw");
        let app = app_with(&dir);
        let mut wizard = Wizard::new_add(SortMode::Manual);
        wizard.step = Step::Connect;
        let mut connect = Connect::new(ProviderId::Claude);
        connect.signin = Some(SignIn {
            inner: crate::oauth::SignIn::start(providers::oauth_spec(ProviderId::Claude).unwrap())
                .unwrap(),
            code: TextInput::new(),
            waiting: false,
            rx: None,
        });
        wizard.connect = Some(connect);
        let mut terminal = Terminal::new(TestBackend::new(96, 26)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, &wizard, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(text.contains("Sign-in code"), "{text}");
        assert!(text.contains("approve in the browser"), "{text}");
        // The guide is the sign-in's, not the form's.
        let guide = keys(&wizard);
        assert!(guide.contains(&("esc".to_string(), "stop waiting".to_string())));
        assert!(guide.contains(&("enter".to_string(), "use the code".to_string())));
        // Typing goes to the code, not to the form underneath it.
        let mut wizard = wizard;
        let mut app = app;
        press(&mut wizard, &mut app, KeyCode::Char('c'));
        let signin = wizard.connect.as_ref().unwrap().signin.as_ref().unwrap();
        assert_eq!(signin.code.value(), "c");
        // And one esc leaves the form as it was, with nothing signed in.
        press(&mut wizard, &mut app, KeyCode::Esc);
        let connect = wizard.connect.as_ref().unwrap();
        assert!(connect.signin.is_none());
        assert!(connect.signed_in.is_none());
        assert_eq!(wizard.step, Step::Connect);
        std::fs::remove_dir_all(&dir).ok();
    }
}
