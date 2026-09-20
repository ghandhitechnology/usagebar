mod config;
mod credentials;
mod detail;
mod fsutil;
mod input;
mod model;
mod oauth;
mod providers;
mod render;
mod settings;
mod ui;
mod usage;
mod wizard;

use std::io::{IsTerminal, Write};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
};
use crossterm::execute;
use serde_json::{json, Value};

use config::{Config, SortMode};
use credentials::{CredentialStore, FileStore, MemoryStore};
use model::{AccountRef, Health, Report};
use settings::Settings;
use ui::{App, Overlay, OverlayAction};
use wizard::Wizard;

const DEFAULT_INTERVAL: u64 = config::DEFAULT_INTERVAL;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return out(&help());
    }

    let dir = config::dir();
    let config_path = config::config_path(&dir);
    let file_store = Arc::new(FileStore::load(config::credentials_path(&dir)));
    let (config, config_error) = match config::load(&config_path) {
        Ok(Some(config)) => (Some(config), None),
        Ok(None) => (None, None),
        Err(why) => {
            // The overlay footer is narrow; the full error goes to stderr.
            eprintln!("usagebar: {why}");
            (
                None,
                Some("config.json is unreadable; setup will start fresh".into()),
            )
        }
    };

    let cli_interval = arg_value(&args, "--interval")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| secs.max(config::MIN_INTERVAL));
    let interval = cli_interval.unwrap_or_else(|| {
        config
            .as_ref()
            .map(Config::interval)
            .unwrap_or(DEFAULT_INTERVAL)
    });

    if args.iter().any(|a| a == "--json") {
        let (accounts, store) = boot(&config, &file_store);
        let snapshot = snapshot(&accounts, &store, sort_mode(&config));
        return out(&format!("{}\n", serde_json::to_string_pretty(&snapshot)?));
    }
    if args.iter().any(|a| a == "--render") {
        let width = flag(&args, "--width").unwrap_or(80).clamp(20, 400) as u16;
        let height = flag(&args, "--height").unwrap_or(24).clamp(6, 200) as u16;
        let sizes = match arg_value(&args, "--sizes") {
            Some(list) => parse_sizes(&list).unwrap_or_else(|| vec![(width, height)]),
            None => vec![(width, height)],
        };
        let (accounts, store) = boot(&config, &file_store);
        let mut app = App::new(interval, store, sort_mode(&config), Arc::clone(&file_store));
        app.accounts = accounts;
        app.usage = Some(usage::scan(Utc::now()));
        app.absorb(providers::fetch_all(
            &app.accounts,
            &app.store,
            app.config.sort,
        ));
        let mut frames = String::new();
        for (w, h) in sizes {
            frames.push_str(&format!("--- {w}x{h}\n"));
            frames.push_str(&render::to_ansi(w, h, &app).unwrap());
        }
        return out(&frames);
    }
    if args.iter().any(|a| a == "--once") {
        let (accounts, store) = boot(&config, &file_store);
        let table = table(&providers::fetch_all(&accounts, &store, sort_mode(&config)));
        return out(&table);
    }
    run_tui(
        interval,
        config,
        config_error,
        file_store,
        cli_interval.is_some(),
    )
}

fn help() -> String {
    format!(
        "usagebar — live subscription usage, one panel per provider\n\n\
         USAGE: usagebar [OPTIONS]\n\n\
         OPTIONS:\n\
         \x20 --interval <SECS>   refresh cadence, overriding the saved setting (default {DEFAULT_INTERVAL})\n\
         \x20 --json             print one snapshot as JSON and exit\n\
         \x20 --once             print one snapshot as a table and exit\n\
         \x20 --render           draw frames to stdout, for narrow panes and screenshots\n\
         \x20 --width <COLS>     frame width for --render (default 80)\n\
         \x20 --height <ROWS>    frame height for --render (default 24)\n\
         \x20 --sizes <LIST>     several frames at once, e.g. 80x24,140x45\n\
         \x20 -h, --help         show this text\n\n\
         KEYS: q quit · r refresh · space pause · s setup · d details\n\
         Config: {config}/config.json (override with USAGEBAR_CONFIG_DIR)\n",
        config = config::dir().display(),
    )
}

fn sort_mode(config: &Option<Config>) -> SortMode {
    config.as_ref().map(|c| c.sort).unwrap_or(SortMode::Smart)
}

/// Accounts to read this run. A config with accounts is authoritative; anything else
/// (no config, an empty list, an unreadable file) falls back to scanning the machine,
/// which is also the behavior of every version before accounts existed.
fn boot(
    config: &Option<Config>,
    file_store: &Arc<FileStore>,
) -> (Vec<AccountRef>, Arc<dyn CredentialStore>) {
    match config {
        Some(config) if !config.auto_detect() => (
            config.accounts.clone(),
            Arc::clone(file_store) as Arc<dyn CredentialStore>,
        ),
        _ => {
            let (accounts, store) = providers::accounts_from_detected(&providers::detect());
            (accounts, Arc::new(store) as Arc<dyn CredentialStore>)
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<u64> {
    arg_value(args, name).and_then(|v| v.parse().ok())
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// "80x24,100x50" into frame sizes, so one fetch can show several pane shapes.
fn parse_sizes(list: &str) -> Option<Vec<(u16, u16)>> {
    list.split(',')
        .map(|item| {
            let (w, h) = item.trim().split_once(['x', 'X'])?;
            Some((
                w.trim().parse::<u16>().ok()?.clamp(20, 400),
                h.trim().parse::<u16>().ok()?.clamp(6, 200),
            ))
        })
        .collect()
}

fn snapshot(accounts: &[AccountRef], store: &Arc<dyn CredentialStore>, sort: SortMode) -> Value {
    let reports = providers::fetch_all(accounts, store, sort);
    json!({
        "captured_at": Utc::now().to_rfc3339(),
        "reports": reports.iter().map(report_json).collect::<Vec<_>>(),
    })
}

fn report_json(report: &Report) -> Value {
    let health = match &report.health {
        Health::Ok => json!({"state": "ok"}),
        Health::NoQuota(why) => json!({"state": "no_quota", "detail": why}),
        Health::Stale { why, since } => json!({
            "state": "stale",
            "detail": why,
            "age_seconds": since.elapsed().as_secs(),
        }),
        Health::Unavailable(why) => json!({"state": "unavailable", "detail": why}),
    };
    json!({
        "account_id": report.key,
        "provider": report.provider.display(),
        "label": report.label,
        "account": report.account,
        "plan": report.plan,
        "source": report.source.glyph(),
        "health": health,
        "windows": report.windows.iter().map(|w| json!({
            "label": w.label,
            "used_percent": w.used_percent,
            "resets_at": w.resets_at.map(|t| t.to_rfc3339()),
            "detail": w.detail,
        })).collect::<Vec<_>>(),
        "facts": report.facts.iter().map(|f| json!({
            "label": f.label,
            "value": f.value,
        })).collect::<Vec<_>>(),
        "notes": report.notes,
    })
}

fn table(reports: &[Report]) -> String {
    let mut out = String::new();
    for report in reports {
        let plan = report.plan.clone().unwrap_or_default();
        let name = report.name();
        out.push_str(&format!("{} {}\n", name, plan));
        match &report.health {
            Health::Ok => {}
            Health::NoQuota(why) => out.push_str(&format!("  no quota reported: {why}\n")),
            Health::Stale { why, since } => out.push_str(&format!(
                "  stale ({}s ago): {why}\n",
                since.elapsed().as_secs()
            )),
            Health::Unavailable(why) => out.push_str(&format!("  unavailable: {why}\n")),
        }
        for window in &report.windows {
            out.push_str(&format!(
                "  {:<16} {:>5.1}%  {}\n",
                window.label,
                window.used_percent,
                window
                    .resets_at
                    .map(|t| format!("resets {}", t.to_rfc3339()))
                    .unwrap_or_default()
            ));
        }
        for fact in report.facts.iter().filter(|fact| fact.panel) {
            out.push_str(&format!("  {} {}\n", fact.label, fact.value));
        }
        for note in &report.notes {
            out.push_str(&format!("  {note}\n"));
        }
    }
    out
}

/// A reader that goes away (`usge --once | head`) is not a failure.
fn out(text: &str) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(text.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn run_tui(
    interval: u64,
    config: Option<Config>,
    config_error: Option<String>,
    file_store: Arc<FileStore>,
    interval_locked: bool,
) -> std::io::Result<()> {
    // Without a terminal this would panic deep inside ratatui; say what to do instead.
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "usagebar: no terminal attached. Use --render for a frame, --once or --json for data."
        );
        std::process::exit(2);
    }

    let persisted = config.is_some();
    let saved_accounts = config
        .as_ref()
        .filter(|config| !config.auto_detect())
        .map(|config| config.accounts.clone())
        .unwrap_or_default();
    let store: Arc<dyn CredentialStore> = if persisted && !saved_accounts.is_empty() {
        Arc::clone(&file_store) as Arc<dyn CredentialStore>
    } else {
        Arc::new(MemoryStore::new())
    };
    let mut app = App::new(interval, store, sort_mode(&config), Arc::clone(&file_store));
    app.accounts = saved_accounts;
    app.interval_locked = interval_locked;
    app.config = config.unwrap_or(Config {
        interval_secs: interval,
        sort: SortMode::Smart,
        ..Config::default()
    });
    app.config_path = config::config_path(&config::dir());
    app.persisted = persisted;
    app.file_store = Arc::clone(&file_store);
    app.boot_note = config_error;

    let (tx, rx) = mpsc::channel::<RefreshEvent>();
    let (scan_tx, scan_rx) = mpsc::channel::<Vec<providers::Detected>>();
    // The token history is read off the local logs, which are big enough that it is
    // spoiling its own thread. Daily columns do not need to be fresher than this.
    let (usage_tx, usage_rx) = mpsc::channel::<usage::Usage>();
    std::thread::spawn(move || loop {
        if usage_tx.send(usage::scan(Utc::now())).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_secs(300));
    });
    let scanning = if app.accounts.is_empty() {
        std::thread::spawn(move || {
            let _ = scan_tx.send(providers::detect());
        });
        true
    } else {
        request_refresh(&mut refresh, &tx, &mut app);
        false
    };

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let tick = Duration::from_millis(120);
    let mut last_trigger = Instant::now();
    let result = loop {
        if scanning {
            if let Ok(detected) = scan_rx.try_recv() {
                let (accounts, detected_store) = providers::accounts_from_detected(&detected);
                let detected_store = Arc::new(detected_store);
                app.accounts = accounts;
                app.store = Arc::clone(&detected_store) as Arc<dyn CredentialStore>;
                app.detected_store = Some(detected_store);
                // Nothing saved yet means this is the first run; offer setup.
                if !app.persisted && app.overlay.is_none() {
                    app.overlay = Some(Overlay::Wizard(Box::new(Wizard::new(
                        detected,
                        app.config.sort,
                    ))));
                }
                request_refresh(&mut refresh, &tx, &mut app);
                last_trigger = Instant::now();
            }
        }
        if let Ok(update) = rx.try_recv() {
            match update {
                RefreshEvent::Progress(reports) => absorb_progress(&mut app, reports),
                RefreshEvent::Complete(reports) => {
                    app.absorb(reports);
                    if refresh.completed() {
                        spawn_refresh(&tx, &mut app);
                    }
                }
            }
        }
        if let Ok(usage) = usage_rx.try_recv() {
            app.usage = Some(usage);
        }
        let due = !app.paused && last_trigger.elapsed() >= Duration::from_secs(app.interval_secs);
        if due && !app.accounts.is_empty() {
            request_refresh(&mut refresh, &tx, &mut app);
            last_trigger = Instant::now();
        }

        if let Some(overlay) = app.overlay.as_mut() {
            overlay.poll();
        }
        if let Err(error) = terminal.draw(|frame| ui::draw(frame, &app)) {
            break Err(error);
        }

        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(mut overlay) = app.overlay.take() {
                        let action = overlay.handle(&mut app, key);
                        match action {
                            OverlayAction::Keep => app.overlay = Some(overlay),
                            OverlayAction::Close => {
                                // Closing the first-run wizard without saving still
                                // records the skip, so it does not open every launch.
                                if matches!(overlay, Overlay::Wizard(_)) && !app.persisted {
                                    // A file that exists but did not parse is kept aside
                                    // rather than overwritten; its accounts may still be
                                    // recoverable by hand.
                                    if app.config_path.exists() {
                                        match crate::fsutil::set_aside(
                                            &app.config_path,
                                            "unreadable",
                                        ) {
                                            Ok(aside) => eprintln!(
                                                "usagebar: kept the unreadable config at {}",
                                                aside.display()
                                            ),
                                            Err(why) => eprintln!("usagebar: {why}"),
                                        }
                                    }
                                    // An explicit "keep scanning", so an empty account
                                    // list stays unambiguous.
                                    app.config.detect = Some(true);
                                    let _ = app.save_config();
                                }
                                app.boot_note = None;
                            }
                            OverlayAction::Refresh => {
                                app.overlay = Some(overlay);
                                request_refresh(&mut refresh, &tx, &mut app);
                                last_trigger = Instant::now();
                            }
                            OverlayAction::Saved => {
                                request_refresh(&mut refresh, &tx, &mut app);
                                last_trigger = Instant::now();
                            }
                            OverlayAction::OpenWizard => {
                                app.overlay = Some(Overlay::Wizard(Box::new(Wizard::new_add(
                                    app.config.sort,
                                ))));
                            }
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                            KeyCode::Char('r') => {
                                request_refresh(&mut refresh, &tx, &mut app);
                                last_trigger = Instant::now();
                            }
                            KeyCode::Char(' ') => app.paused = !app.paused,
                            KeyCode::Char('s') => {
                                app.overlay = Some(Overlay::Settings(Settings::default()))
                            }
                            KeyCode::Char('d') => {
                                app.overlay = Some(Overlay::Detail(detail::Detail::new()))
                            }
                            _ => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    if let Some(overlay) = app.overlay.as_mut() {
                        overlay.paste(&text);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    };

    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

#[derive(Default)]
struct RefreshGate {
    in_flight: bool,
    pending: bool,
}

impl RefreshGate {
    /// Start immediately when idle; otherwise remember one follow-up refresh.
    fn request(&mut self) -> bool {
        if self.in_flight {
            self.pending = true;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    /// Finish the active batch and immediately claim a queued refresh, if any.
    fn completed(&mut self) -> bool {
        self.in_flight = false;
        if self.pending {
            self.pending = false;
            self.in_flight = true;
            true
        } else {
            false
        }
    }
}

fn request_refresh(gate: &mut RefreshGate, tx: &mpsc::Sender<RefreshEvent>, app: &mut App) {
    if !app.accounts.is_empty() && gate.request() {
        spawn_refresh(tx, app);
    }
}

fn spawn_refresh(tx: &mpsc::Sender<RefreshEvent>, app: &mut App) {
    let tx = tx.clone();
    let accounts = app.accounts.clone();
    let store = Arc::clone(&app.store);
    let sort = app.config.sort;
    app.refreshing = true;
    std::thread::spawn(move || {
        let reports = providers::fetch_all_progressive(&accounts, &store, sort, |reports| {
            let _ = tx.send(RefreshEvent::Progress(reports.to_vec()));
        });
        let _ = tx.send(RefreshEvent::Complete(reports));
    });
}

enum RefreshEvent {
    Progress(Vec<Report>),
    Complete(Vec<Report>),
}

/// Show successful accounts as soon as they finish. Failures wait for the complete
/// batch so App::absorb can preserve the previous good reading as stale.
fn absorb_progress(app: &mut App, reports: Vec<Report>) {
    let expected: Vec<&str> = app
        .accounts
        .iter()
        .filter(|account| !account.hidden)
        .map(|account| account.id.as_str())
        .collect();
    app.reports
        .retain(|report| expected.contains(&report.key.as_str()));
    for report in reports.into_iter().filter(|report| {
        !matches!(report.health, Health::Unavailable(_) | Health::Stale { .. })
            && expected.contains(&report.key.as_str())
    }) {
        match app
            .reports
            .iter()
            .position(|current| current.key == report.key)
        {
            Some(index) => app.reports[index] = report,
            None => app.reports.push(report),
        }
    }
    providers::sort_reports(&mut app.reports, &app.accounts, app.config.sort);
}

#[cfg(test)]
mod tests {
    use super::RefreshGate;

    #[test]
    fn refresh_gate_coalesces_overlapping_requests() {
        let mut gate = RefreshGate::default();

        assert!(gate.request());
        assert!(!gate.request());
        assert!(!gate.request());
        assert!(gate.completed());
        assert!(!gate.completed());
        assert!(gate.request());
    }
}
