//! Rendering. A dense panel grid with vendor-accurate numbers, tuned for a dark terminal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::config::{self, Config, SortMode};
use crate::credentials::{CredentialStore, FileStore, MemoryStore};
use crate::model::{AccountRef, Health, ProviderId, Report};
use crate::settings::{self, Settings};
use crate::wizard::{self, Wizard};

pub struct App {
    pub reports: Vec<Report>,
    pub last_refresh: Option<Instant>,
    pub refreshing: bool,
    pub paused: bool,
    pub interval_secs: u64,
    /// True when --interval named a value for this run only; finishing setup must not
    /// turn that into the saved preference.
    pub interval_locked: bool,
    /// Accounts in display order, and the secrets they read from.
    pub accounts: Vec<AccountRef>,
    pub store: Arc<dyn CredentialStore>,
    /// The saved preferences. `accounts` mirrors this list whenever a config exists.
    pub config: Config,
    pub config_path: PathBuf,
    /// False until a config has been written; onboarding starts when it never was.
    pub persisted: bool,
    /// Always the on-disk credential store; writes go here.
    pub file_store: Arc<FileStore>,
    /// Credentials found by the startup scan but not yet committed to the store.
    pub detected_store: Option<Arc<MemoryStore>>,
    /// Anything the user should hear about at startup, e.g. an unreadable config.
    pub boot_note: Option<String>,
    pub overlay: Option<Overlay>,
}

/// The modal screens. One at a time; the base view owns every key when this is None.
pub enum Overlay {
    Settings(Settings),
    Wizard(Wizard),
    Detail(crate::detail::Detail),
}

impl Overlay {
    pub fn handle(&mut self, app: &mut App, key: KeyEvent) -> OverlayAction {
        match self {
            Overlay::Settings(settings) => match settings::handle(settings, app, key) {
                settings::Action::Keep => OverlayAction::Keep,
                settings::Action::Close => OverlayAction::Close,
                settings::Action::Refresh => OverlayAction::Refresh,
                settings::Action::OpenWizard => OverlayAction::OpenWizard,
            },
            Overlay::Wizard(wizard) => match wizard::handle(wizard, app, key) {
                wizard::Action::Keep => OverlayAction::Keep,
                wizard::Action::Close => OverlayAction::Close,
                wizard::Action::Saved => OverlayAction::Saved,
            },
            Overlay::Detail(detail) => match crate::detail::handle(detail, app, key) {
                crate::detail::Action::Keep => OverlayAction::Keep,
                crate::detail::Action::Close => OverlayAction::Close,
            },
        }
    }

    /// A paste goes to whatever field is focused in the open overlay.
    pub fn paste(&mut self, text: &str) {
        match self {
            Overlay::Settings(settings) => {
                if let Some(input) = settings.editing.as_mut() {
                    input.paste(text);
                }
            }
            Overlay::Wizard(wizard) => wizard.paste(text),
            Overlay::Detail(_) => {}
        }
    }

    /// Called every tick so background work can land without blocking the UI.
    pub fn poll(&mut self) {
        if let Overlay::Wizard(wizard) = self {
            wizard.poll();
        }
    }
}

pub enum OverlayAction {
    Keep,
    Close,
    /// Keep the overlay open, but re-read every account.
    Refresh,
    /// The overlay is done and closed; re-read every account.
    Saved,
    OpenWizard,
}

impl App {
    pub fn new(
        interval_secs: u64,
        store: Arc<dyn CredentialStore>,
        sort: SortMode,
        file_store: Arc<FileStore>,
    ) -> Self {
        Self {
            reports: Vec::new(),
            last_refresh: None,
            refreshing: false,
            paused: false,
            interval_secs,
            interval_locked: false,
            accounts: Vec::new(),
            store,
            config: Config {
                interval_secs,
                sort,
                ..Config::default()
            },
            config_path: config::config_path(&config::dir()),
            persisted: false,
            file_store,
            detected_store: None,
            boot_note: None,
            overlay: None,
        }
    }

    /// Write the preferences. Returns the error for the overlay to show, if any.
    pub fn save_config(&mut self) -> Option<String> {
        match config::save(&self.config_path, &self.config) {
            Ok(()) => {
                self.persisted = true;
                None
            }
            Err(why) => Some(why),
        }
    }

    /// The user changed which accounts are shown or their order. Accounts that only
    /// existed as a scan result are committed to the credential store here, because an
    /// account the user kept has to survive a restart.
    pub fn save_accounts(&mut self, accounts: Vec<AccountRef>) -> Option<String> {
        self.accounts = accounts;
        self.config.accounts = self.accounts.clone();
        // Once the list is the user's, stop scanning for new credentials each run.
        self.config.detect = Some(false);
        for account in &self.accounts {
            if self.file_store.get(&account.id).is_none() {
                if let Some(stored) = self
                    .detected_store
                    .as_ref()
                    .and_then(|store| store.get(&account.id))
                {
                    if let Err(why) = self.file_store.put(&account.id, stored) {
                        return Some(why);
                    }
                }
            }
        }
        self.store = Arc::clone(&self.file_store) as Arc<dyn CredentialStore>;
        self.save_config()
    }

    pub fn forget_credentials(&self, account_id: &str) {
        if let Err(why) = self.file_store.remove(account_id) {
            eprintln!("usagebar: could not remove credentials for {account_id}: {why}");
        }
    }

    pub fn absorb(&mut self, reports: Vec<Report>) {
        let previous: HashMap<String, Report> = self
            .reports
            .iter()
            .map(|report| (report.key.clone(), report.clone()))
            .collect();
        let last_good = self.last_refresh.unwrap_or_else(Instant::now);
        // A fetch started before the account list changed can still be in flight; its
        // readings are for accounts the user just hid or removed.
        let expected: Vec<&str> = self
            .accounts
            .iter()
            .filter(|account| !account.hidden)
            .map(|account| account.id.as_str())
            .collect();

        let reports: Vec<Report> = reports
            .into_iter()
            .filter(|report| expected.contains(&report.key.as_str()))
            .map(|mut report| {
                // A rate limit or a network blip should not erase a panel that had a real
                // vendor number a moment ago; keep the number and say how old it is.
                if report.windows.is_empty() {
                    if let Health::Unavailable(why) = &report.health {
                        if let Some(last) =
                            previous.get(&report.key).filter(|r| !r.windows.is_empty())
                        {
                            report.windows = last.windows.clone();
                            report.facts = last.facts.clone();
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

        self.reports = reports;
        self.last_refresh = Some(Instant::now());
        self.refreshing = false;
    }

    /// The report for an account id, in display order.
    pub fn report_for(&self, key: &str) -> Option<&Report> {
        self.reports.iter().find(|report| report.key == key)
    }
}

// ------------------------------------------------------------------ palette

pub(crate) const ACCENT: Color = Color::Rgb(0xD9, 0x77, 0x57);
pub(crate) const DIM: Color = Color::Rgb(0x6B, 0x6F, 0x7A);
pub(crate) const FAINT: Color = Color::Rgb(0x3C, 0x40, 0x48);
pub(crate) const TEXT: Color = Color::Rgb(0xC8, 0xCC, 0xD4);
const TRACK: Color = Color::Rgb(0x33, 0x36, 0x3D);

/// How much of a colour survives the backdrop under an open overlay, in percent.
/// opencode dims its own settings screen to this same ratio.
const BACKDROP: u16 = 41;

pub(crate) fn provider_color(provider: ProviderId) -> Color {
    match provider {
        ProviderId::Claude => Color::Rgb(0xD9, 0x77, 0x57),
        ProviderId::Codex => Color::Rgb(0x4F, 0xB8, 0x9A),
        ProviderId::OpenCodeGo => Color::Rgb(0x7A, 0xA2, 0xF7),
        ProviderId::Cursor => Color::Rgb(0xB9, 0xC2, 0xD6),
        ProviderId::Grok => Color::Rgb(0x9C, 0xA3, 0xAF),
        ProviderId::Devin => Color::Rgb(0x6E, 0x9E, 0xE8),
        ProviderId::CommandCode => Color::Rgb(0xE0, 0xA8, 0x5E),
    }
}

/// Filled-bar ramp, cool when there is headroom and hot when there is not.
pub(crate) fn ramp(percent: f64, position: f64) -> Color {
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

pub(crate) fn bar_spans(percent: f64, width: usize) -> Vec<Span<'static>> {
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

pub(crate) fn countdown(reset: DateTime<Utc>, now: DateTime<Utc>) -> String {
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

pub(crate) fn clip(text: &str, width: usize) -> String {
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

pub(crate) fn pad(text: &str, width: usize) -> String {
    let clipped = clip(text, width);
    let len = clipped.chars().count();
    format!("{clipped}{}", " ".repeat(width.saturating_sub(len)))
}

/// A box of a fixed size, centered in the given area. Used by every overlay.
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .flex(ratatui::layout::Flex::Center)
        .constraints([Constraint::Length(height)])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .flex(ratatui::layout::Flex::Center)
        .constraints([Constraint::Length(width)])
        .split(vertical[0])[0]
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
    if app.overlay.is_some() {
        backdrop(frame, chunks[2]);
    }
    match &app.overlay {
        Some(Overlay::Settings(settings)) => settings::draw(frame, app, settings, area),
        Some(Overlay::Wizard(wizard)) => wizard::draw(frame, app, wizard, area),
        Some(Overlay::Detail(detail)) => crate::detail::draw(frame, app, detail, area),
        None => {}
    }
}

/// Drops the view behind an open overlay towards the background, so the overlay reads as
/// the only live surface. Each colour keeps its hue and loses most of its brightness;
/// bold goes with it, since a bright weight would punch back through the veil. Only the
/// rows above `footer` are veiled, which keeps the key guide fully readable.
fn backdrop(frame: &mut Frame, footer: Rect) {
    let area = frame.area();
    let veiled = area.width as usize * footer.y.saturating_sub(area.y) as usize;
    for cell in frame.buffer_mut().content.iter_mut().take(veiled) {
        cell.fg = faded(cell.fg);
        cell.bg = faded(cell.bg);
        cell.modifier.remove(Modifier::BOLD);
    }
}

fn faded(color: Color) -> Color {
    let dim = |channel: u8| (u16::from(channel) * BACKDROP / 100) as u8;
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(dim(r), dim(g), dim(b)),
        // Everything else is the terminal's own colour, which is already the backdrop.
        other => other,
    }
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

/// The bottom bar: what to press here, for the screen that is open.
fn footer(frame: &mut Frame, app: &App, area: Rect) {
    let (guide, tail) = match &app.overlay {
        None => (
            vec![
                ("q".to_string(), "quit".to_string()),
                ("r".to_string(), "refresh".to_string()),
                ("space".to_string(), "pause".to_string()),
                ("s".to_string(), "setup".to_string()),
                ("d".to_string(), "details".to_string()),
            ],
            Some(match &app.boot_note {
                Some(note) => Span::styled(
                    format!(" {note}"),
                    Style::default().fg(Color::Rgb(0xD8, 0xA8, 0x57)),
                ),
                None => {
                    let ok = app
                        .reports
                        .iter()
                        .filter(|r| matches!(r.health, Health::Ok))
                        .count();
                    Span::styled(
                        format!(" {ok}/{} reporting", app.reports.len()),
                        Style::default().fg(FAINT),
                    )
                }
            }),
        ),
        Some(Overlay::Settings(_)) => (settings::keys(), None),
        Some(Overlay::Wizard(wizard)) => (wizard::keys(wizard), None),
        Some(Overlay::Detail(detail)) => (crate::detail::keys(app, detail), None),
    };

    let width = area.width as usize;
    let tail_width = tail.as_ref().map(|span| span.width()).unwrap_or(0);
    let mut spans = key_guide(&guide, width.saturating_sub(tail_width + 1));
    if let Some(tail) = tail {
        let used: usize = spans.iter().map(|span| span.width()).sum();
        let gap = width.saturating_sub(used + tail.width());
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(tail);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// " q quit · r refresh ", dropping entries that do not fit. A pane too narrow for one
/// entry still gets the keys themselves, which is the part a newcomer needs.
fn key_guide(entries: &[(String, String)], width: usize) -> Vec<Span<'static>> {
    let cost = |index: usize| {
        let (key, action) = &entries[index];
        key.chars().count() + action.chars().count() + 2 + if index > 0 { 2 } else { 0 }
    };
    let mut budget = 0usize;
    let mut keep = 0usize;
    for index in 0..entries.len() {
        let next = cost(index);
        if budget + next > width {
            break;
        }
        budget += next;
        keep += 1;
    }
    if keep == 0 {
        let bare: String = entries
            .iter()
            .map(|(key, _)| format!(" {key}"))
            .collect::<Vec<_>>()
            .join(" ");
        return vec![Span::styled(
            clip(&bare, width),
            Style::default().fg(ACCENT),
        )];
    }
    let mut spans = Vec::new();
    for (index, (key, action)) in entries.iter().take(keep).enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ·", Style::default().fg(FAINT)));
        }
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().fg(ACCENT),
        ));
        spans.push(Span::styled(action.clone(), Style::default().fg(DIM)));
    }
    spans
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
    let (density, heights, hidden) = plan(&rows, area.height, columns);

    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            heights
                .iter()
                .map(|h| Constraint::Length(*h))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (row_index, row) in rows.iter().take(heights.len()).enumerate() {
        let rect = row_rects[row_index];
        if rect.height == 0 {
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
            card(frame, report, cells[card_index], density);
        }
    }

    if hidden > 0 {
        let used = heights.iter().map(|h| *h as usize).sum::<usize>() as u16;
        let y = area.y + used;
        if y < area.y + area.height {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("+{hidden} more — enlarge the pane"),
                    Style::default().fg(FAINT),
                ))),
                Rect { y, ..area },
            );
        }
    }
}

/// Picks the densest drawing that still fits the pane, then drops whole rows that will not
/// fit rather than letting the layout shrink every panel out of usefulness. Returns the
/// density, the heights of the rows to draw, and how many panels were held back.
fn plan(rows: &[&[Report]], height: u16, columns: usize) -> (Density, Vec<u16>, usize) {
    let outcome = fit_density(rows, height, columns);
    // A panel held back without a word is worse than one fewer panel, so re-plan with a row
    // reserved for the notice whenever something had to be dropped.
    match outcome.2 {
        0 => outcome,
        _ if height >= 3 => fit_density(rows, height - 1, columns),
        _ => outcome,
    }
}

fn fit_density(rows: &[&[Report]], height: u16, columns: usize) -> (Density, Vec<u16>, usize) {
    let by_density = |density| row_heights(rows, density);
    let (density, mut heights) = if fits(&by_density(Density::Full), height) {
        (Density::Full, by_density(Density::Full))
    } else if fits(&by_density(Density::Compact), height) {
        (Density::Compact, by_density(Density::Compact))
    } else {
        (Density::Row, by_density(Density::Row))
    };

    let mut used = 0u16;
    let mut keep = 0usize;
    for row_height in &heights {
        if used + row_height <= height {
            used += row_height;
            keep += 1;
        } else {
            break;
        }
    }
    let hidden = (rows.len() - keep) * columns;
    heights.truncate(keep);
    (density, heights, hidden)
}

/// How much of each window a panel can afford to draw. Picked by what actually fits, so a
/// short side panel still shows every provider instead of squeezing each one to nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Density {
    /// Bar plus a reset line per window.
    Full,
    /// One line per window, countdown inline.
    Compact,
    /// One line per provider, tightest window only.
    Row,
}

fn fits(heights: &[u16], budget: u16) -> bool {
    heights.iter().map(|h| *h as usize).sum::<usize>() <= budget as usize
}

fn row_heights(rows: &[&[Report]], density: Density) -> Vec<u16> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|r| card_height(r, density))
                .max()
                .unwrap_or(1)
        })
        .collect()
}

fn card_height(report: &Report, density: Density) -> u16 {
    if density == Density::Row {
        return 1;
    }
    let per_window = match density {
        Density::Full => 2,
        _ => 1,
    };
    let summary =
        usize::from(!report.notes.is_empty() || report.facts.iter().any(|fact| fact.panel));
    let content = match &report.health {
        Health::Unavailable(_) => 2,
        // Stale keeps the windows and the summary from the last good reading and adds
        // one line saying why it is old.
        Health::Stale { .. } => report.windows.len() * per_window + 1 + summary,
        Health::Ok | Health::NoQuota(_) => report.windows.len() * per_window + summary,
    };
    (content + 2) as u16
}

/// One provider per line, showing whichever window is closest to its limit.
fn row_line(frame: &mut Frame, report: &Report, area: Rect) {
    let width = area.width as usize;
    if width < 16 {
        return;
    }
    let accent = provider_color(report.provider);
    let name_width = 13.min(width / 4);

    let tightest = report.windows.iter().max_by(|a, b| {
        a.used_percent
            .partial_cmp(&b.used_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut spans = vec![Span::styled(
        pad(report.provider.display(), name_width),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )];

    match tightest {
        Some(window) => {
            let label_width = 9.min(width / 5);
            let reserved = name_width + label_width + 1 + 4 + 1;
            let bar_width = width.saturating_sub(reserved).max(4);
            spans.push(Span::styled(
                pad(&window.label, label_width),
                Style::default().fg(DIM),
            ));
            spans.extend(bar_spans(window.used_percent, bar_width));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{:>3.0}%", window.used_percent),
                Style::default()
                    .fg(ramp(window.used_percent, 0.8))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        None => {
            let note = match &report.health {
                Health::Unavailable(why) => why.clone(),
                Health::NoQuota(why) => why.clone(),
                _ => "no windows".into(),
            };
            spans.push(Span::styled(
                clip(&note, width.saturating_sub(name_width + 1)),
                Style::default().fg(FAINT),
            ));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn card(frame: &mut Frame, report: &Report, area: Rect, density: Density) {
    if density == Density::Row {
        row_line(frame, report, area);
        return;
    }
    let compact = density == Density::Compact;
    let accent = provider_color(report.provider);
    let healthy = matches!(report.health, Health::Ok | Health::Stale { .. });

    let mut title = vec![Span::styled(
        format!(" {} ", report.provider.display()),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )];
    if let Some(name) = report.label.clone().or_else(|| report.account.clone()) {
        title.push(Span::styled(
            format!("{} ", crate::model::short_account(&name)),
            Style::default().fg(FAINT),
        ));
    }
    let badge = match (&report.plan, &report.health) {
        (Some(plan), _) => Span::styled(format!(" {} ", plan), Style::default().fg(DIM)),
        (None, Health::Stale { .. }) => {
            Span::styled(" stale ", Style::default().fg(Color::Rgb(0xD8, 0xA8, 0x57)))
        }
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
        let label_width = 10.min(width / 4);

        // Windows that share a billing cycle would otherwise repeat the same countdown on
        // every line of the card.
        let reset = match window.resets_at {
            Some(at) if Some(at) == shown_reset => String::new(),
            Some(at) => {
                shown_reset = Some(at);
                countdown(at, now)
            }
            None if window.used_percent <= 0.0 => "idle".into(),
            None => String::new(),
        };

        // In a narrow pane the countdown rides on the same line as the bar, which buys a
        // whole line per window and keeps every provider visible.
        let reset_width = if compact {
            reset.chars().count().min(8) + 1
        } else {
            0
        };
        // The bar takes every column the label, percentage, and countdown do not, so its
        // right edge meets the panel edge and the detail text below it, instead of
        // stopping short and leaving the numbers floating in the middle of the panel.
        let reserved = label_width + 1 + 4 + 1 + reset_width;
        let bar_width = width.saturating_sub(reserved).max(6);

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
        if compact {
            let used = label_width + 1 + bar_width + 1 + 4;
            let gap = width.saturating_sub(used + reset.chars().count());
            if !reset.is_empty() && gap > 0 {
                row.push(Span::raw(" ".repeat(gap)));
                row.push(Span::styled(reset.clone(), Style::default().fg(DIM)));
            }
        }
        lines.push(Line::from(row));
        if compact {
            continue;
        }

        let mut sub = vec![
            Span::raw(" ".repeat(label_width + 1)),
            Span::styled(
                if reset.is_empty() {
                    String::new()
                } else {
                    format!("resets in {reset}")
                },
                Style::default().fg(DIM),
            ),
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

    if !report.notes.is_empty() || report.facts.iter().any(|fact| fact.panel) {
        let mut summary: Vec<String> = report
            .facts
            .iter()
            .filter(|fact| fact.panel)
            .map(|fact| format!("{} {}", fact.label, fact.value))
            .collect();
        summary.extend(report.notes.iter().cloned());
        lines.push(Line::from(Span::styled(
            clip(&summary.join(" · "), width),
            Style::default().fg(FAINT),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;
    use crate::model::Window;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_app() -> App {
        let dir = std::env::temp_dir().join(format!("usagebar-ui-{}", std::process::id()));
        App::new(
            60,
            Arc::new(MemoryStore::new()),
            SortMode::Manual,
            Arc::new(crate::credentials::FileStore::load(
                dir.join("credentials.json"),
            )),
        )
    }

    /// A failed poll must keep the previous vendor number, marked stale, rather than
    /// blanking a panel that was reporting fine a minute ago.
    #[test]
    fn failure_keeps_last_good_reading() {
        let mut app = test_app();
        app.accounts = vec![AccountRef::new("claude", ProviderId::Claude)];
        let mut good = Report::new(ProviderId::Claude).key("claude");
        good.windows.push(Window::new("Weekly", 42.0));
        app.absorb(vec![good]);

        app.absorb(vec![
            Report::failed(ProviderId::Claude, "HTTP 429".into()).key("claude")
        ]);

        let report = &app.reports[0];
        assert_eq!(report.windows.len(), 1);
        assert_eq!(report.windows[0].used_percent, 42.0);
        match &report.health {
            Health::Stale { why, .. } => assert_eq!(why, "HTTP 429"),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    /// Two accounts of one provider are two panels, so one account's reading must not
    /// overwrite the other's.
    #[test]
    fn accounts_of_one_provider_keep_separate_reports() {
        let mut app = test_app();
        app.accounts = vec![
            AccountRef::new("claude-a", ProviderId::Claude),
            AccountRef::new("claude-b", ProviderId::Claude),
        ];
        let mut first = Report::new(ProviderId::Claude).key("claude-a");
        first.windows.push(Window::new("Weekly", 10.0));
        let mut second = Report::new(ProviderId::Claude).key("claude-b");
        second.windows.push(Window::new("Weekly", 80.0));
        app.absorb(vec![first, second]);

        assert_eq!(app.reports[0].windows[0].used_percent, 10.0);
        assert_eq!(app.reports[1].windows[0].used_percent, 80.0);
    }

    /// Without a previous reading there is nothing honest to show, so the error stands.
    #[test]
    fn failure_without_history_stays_unavailable() {
        let mut app = test_app();
        app.accounts = vec![AccountRef::new("claude", ProviderId::Claude)];
        app.absorb(vec![
            Report::failed(ProviderId::Claude, "HTTP 429".into()).key("claude")
        ]);
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

    fn sample_report(provider: ProviderId, windows: usize) -> Report {
        let mut report = Report::new(provider);
        for index in 0..windows {
            report.windows.push(Window::new(format!("W{index}"), 10.0));
        }
        report
    }

    /// A side panel is the whole point of the tool, so the plan must never squeeze panels
    /// down: it drops to a lighter drawing or holds panels back, visibly.
    #[test]
    fn plan_falls_back_to_a_density_that_fits() {
        let reports: Vec<Report> = ProviderId::ALL[..3]
            .iter()
            .map(|p| sample_report(*p, 3))
            .collect();
        let rows: Vec<&[Report]> = reports.chunks(1).collect();

        // Roomy: full drawing, three windows at two lines each plus borders.
        let (density, heights, hidden) = plan(&rows, 40, 1);
        assert_eq!(density, Density::Full);
        assert_eq!(heights, vec![8, 8, 8]);
        assert_eq!(hidden, 0);

        // Tight: one line per window.
        let (density, heights, hidden) = plan(&rows, 15, 1);
        assert_eq!(density, Density::Compact);
        assert_eq!(heights, vec![5, 5, 5]);
        assert_eq!(hidden, 0);

        // Tighter still: one line per provider.
        let (density, heights, hidden) = plan(&rows, 6, 1);
        assert_eq!(density, Density::Row);
        assert_eq!(heights, vec![1, 1, 1]);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn plan_reports_panels_it_could_not_fit() {
        let reports: Vec<Report> = ProviderId::ALL[..4]
            .iter()
            .map(|p| sample_report(*p, 3))
            .collect();
        let rows: Vec<&[Report]> = reports.chunks(1).collect();

        // Three rows of room for four panels: one row goes to the notice, the rest to
        // panels, and nothing disappears without saying so.
        let (density, heights, hidden) = plan(&rows, 3, 1);
        assert_eq!(density, Density::Row);
        assert_eq!(heights.len(), 2);
        assert_eq!(hidden, 2);
    }

    /// An open overlay drops the view behind it, and the key guide stays out of the veil.
    #[test]
    fn the_setup_overlay_dims_the_view_behind_it_but_not_the_key_guide() {
        let mut app = test_app();
        app.accounts = vec![AccountRef::new("claude", ProviderId::Claude)];
        app.absorb(vec![sample_report(ProviderId::Claude, 2)]);

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let plain = terminal.backend().buffer().clone();

        app.overlay = Some(Overlay::Settings(Settings::default()));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let veiled = terminal.backend().buffer().clone();

        // Everything above the key guide keeps its place and loses its brightness.
        let mut compared = 0;
        for y in [0, 1, 38] {
            for x in 0..120 {
                let (before, after) = (plain[(x, y)].clone(), veiled[(x, y)].clone());
                assert_eq!(after.symbol(), before.symbol());
                if let Color::Rgb(..) = before.fg {
                    assert_eq!(after.fg, faded(before.fg));
                    compared += 1;
                }
            }
        }
        assert!(compared > 4, "the header should carry colour to compare");

        // The navigation guide on the last row is drawn as-is: its keys keep the accent
        // colour that everything above has just given up.
        let guide = 39;
        let keys = (0..120)
            .filter(|&x| veiled[(x, guide)].fg == ACCENT)
            .count();
        assert!(keys > 0, "the key guide should carry the accent colour");
        assert!(
            (0..120).all(|x| veiled[(x, guide)].fg != faded(ACCENT)),
            "the key guide should stay out of the veil"
        );

        // The panel is drawn after the backdrop, so its accent survives untouched.
        assert!(veiled.content().iter().any(|cell| cell.fg == ACCENT));
    }
}
