mod config;
mod credentials;
mod fsutil;
mod input;
mod model;
mod providers;
mod render;
mod settings;
mod ui;

use std::io::IsTerminal;
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

const DEFAULT_INTERVAL: u64 = config::DEFAULT_INTERVAL;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{}", help());
        return Ok(());
    }

    let dir = config::dir();
    let config_path = config::config_path(&dir);
    let file_store = Arc::new(FileStore::load(config::credentials_path(&dir)));
    let (config, config_error) = match config::load(&config_path) {
        Ok(Some(config)) => (Some(config), None),
        Ok(None) => (None, None),
        Err(why) => (
            None,
            Some(format!("{why}; starting setup instead of using it")),
        ),
    };

    let interval = arg_value(&args, "--interval")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| secs.max(config::MIN_INTERVAL))
        .unwrap_or_else(|| {
            config
                .as_ref()
                .map(Config::interval)
                .unwrap_or(DEFAULT_INTERVAL)
        });

    if args.iter().any(|a| a == "--json") {
        let (accounts, store) = boot(&config, &file_store);
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot(&accounts, &store, sort_mode(&config)))?
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--render") {
        let width = flag(&args, "--width").unwrap_or(80).clamp(20, 400) as u16;
        let height = flag(&args, "--height").unwrap_or(24).clamp(6, 200) as u16;
        let sizes = match arg_value(&args, "--sizes") {
            Some(list) => parse_sizes(&list).unwrap_or_else(|| vec![(width, height)]),
            None => vec![(width, height)],
        };
        let (accounts, store) = boot(&config, &file_store);
        let mut app = App::new(interval, store, sort_mode(&config));
        app.accounts = accounts;
        app.absorb(providers::fetch_all(
            &app.accounts,
            &app.store,
            app.config.sort,
        ));
        for (w, h) in sizes {
            println!("--- {w}x{h}");
            print!("{}", render::to_ansi(w, h, &app).unwrap());
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--once") {
        let (accounts, store) = boot(&config, &file_store);
        print_table(&providers::fetch_all(&accounts, &store, sort_mode(&config)));
        return Ok(());
    }
    run_tui(interval, config, config_error, file_store)
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
         KEYS: q quit · r refresh · space pause · s setup\n\
         Config: ~/.config/usagebar/config.json (override with USAGEBAR_CONFIG_DIR)\n"
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

fn print_table(reports: &[Report]) {
    for report in reports {
        let plan = report.plan.clone().unwrap_or_default();
        let name = match &report.label {
            Some(label) => format!("{} · {label}", report.provider.display()),
            None => report.provider.display().to_string(),
        };
        println!("{} {}", name, plan);
        match &report.health {
            Health::Ok => {}
            Health::NoQuota(why) => println!("  no quota reported: {why}"),
            Health::Stale { why, since } => {
                println!("  stale ({}s ago): {why}", since.elapsed().as_secs())
            }
            Health::Unavailable(why) => println!("  unavailable: {why}"),
        }
        for window in &report.windows {
            println!(
                "  {:<16} {:>5.1}%  {}",
                window.label,
                window.used_percent,
                window
                    .resets_at
                    .map(|t| format!("resets {}", t.to_rfc3339()))
                    .unwrap_or_default()
            );
        }
        for fact in report.facts.iter().filter(|fact| fact.panel) {
            println!("  {} {}", fact.label, fact.value);
        }
        for note in &report.notes {
            println!("  {note}");
        }
    }
}

fn run_tui(
    interval: u64,
    config: Option<Config>,
    config_error: Option<String>,
    file_store: Arc<FileStore>,
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
    let mut app = App::new(interval, store, sort_mode(&config));
    app.accounts = saved_accounts;
    app.config = config.unwrap_or(Config {
        interval_secs: interval,
        sort: SortMode::Smart,
        ..Config::default()
    });
    app.config_path = config::config_path(&config::dir());
    app.persisted = persisted;
    app.file_store = Arc::clone(&file_store);
    app.boot_note = config_error;

    let (tx, rx) = mpsc::channel::<Vec<Report>>();
    let (scan_tx, scan_rx) = mpsc::channel::<Vec<providers::Detected>>();
    let scanning = if app.accounts.is_empty() {
        std::thread::spawn(move || {
            let _ = scan_tx.send(providers::detect());
        });
        true
    } else {
        trigger(&tx, &app.accounts, &app.store, app.config.sort);
        app.refreshing = true;
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
                trigger(&tx, &app.accounts, &app.store, app.config.sort);
                app.refreshing = true;
                last_trigger = Instant::now();
            }
        }
        if let Ok(reports) = rx.try_recv() {
            app.absorb(reports);
        }
        let due = !app.paused && last_trigger.elapsed() >= Duration::from_secs(app.interval_secs);
        if due && !app.accounts.is_empty() {
            trigger(&tx, &app.accounts, &app.store, app.config.sort);
            app.refreshing = true;
            last_trigger = Instant::now();
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
                            OverlayAction::Close => {}
                            OverlayAction::Keep => app.overlay = Some(overlay),
                            OverlayAction::Refresh => {
                                app.overlay = Some(overlay);
                                trigger(&tx, &app.accounts, &app.store, app.config.sort);
                                app.refreshing = true;
                                last_trigger = Instant::now();
                            }
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                            KeyCode::Char('r') => {
                                trigger(&tx, &app.accounts, &app.store, app.config.sort);
                                app.refreshing = true;
                                last_trigger = Instant::now();
                            }
                            KeyCode::Char(' ') => app.paused = !app.paused,
                            KeyCode::Char('s') => {
                                app.overlay = Some(Overlay::Settings(Settings::default()))
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

fn trigger(
    tx: &mpsc::Sender<Vec<Report>>,
    accounts: &[AccountRef],
    store: &Arc<dyn CredentialStore>,
    sort: SortMode,
) {
    let tx = tx.clone();
    let accounts = accounts.to_vec();
    let store = Arc::clone(store);
    std::thread::spawn(move || {
        let _ = tx.send(providers::fetch_all(&accounts, &store, sort));
    });
}
