mod model;
mod providers;
mod ui;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use serde_json::{json, Value};

use model::{Health, Report};
use ui::App;

const DEFAULT_INTERVAL: u64 = 60;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "usagebar — live subscription usage, one panel per provider\n\n\
             USAGE: usagebar [OPTIONS]\n\n\
             OPTIONS:\n\
             \x20 --interval <SECS>   refresh cadence in the TUI (default {DEFAULT_INTERVAL})\n\
             \x20 --json             print one snapshot as JSON and exit\n\
             \x20 --once             print one snapshot as a table and exit\n\
             \x20 -h, --help         show this text\n"
        );
        return Ok(());
    }

    let interval = args
        .iter()
        .position(|a| a == "--interval")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL)
        .max(5);

    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&snapshot())?);
        return Ok(());
    }
    if args.iter().any(|a| a == "--once") {
        print_table(&providers::fetch_all());
        return Ok(());
    }
    run_tui(interval)
}

fn snapshot() -> Value {
    let reports = providers::fetch_all();
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
        "provider": report.provider,
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
        "notes": report.notes,
    })
}

fn print_table(reports: &[Report]) {
    for report in reports {
        let plan = report.plan.clone().unwrap_or_default();
        println!("{} {}", report.provider, plan);
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
        for note in &report.notes {
            println!("  {note}");
        }
    }
}

fn run_tui(interval: u64) -> std::io::Result<()> {
    let mut app = App::new(interval);
    let (tx, rx) = mpsc::channel::<Vec<Report>>();
    trigger(&tx);
    app.refreshing = true;

    let mut terminal = ratatui::init();
    let tick = Duration::from_millis(120);
    let mut last_trigger = Instant::now();
    let result = loop {
        if let Ok(reports) = rx.try_recv() {
            app.absorb(reports);
        }
        let due = !app.paused && last_trigger.elapsed() >= Duration::from_secs(interval);
        if due {
            trigger(&tx);
            app.refreshing = true;
            last_trigger = Instant::now();
        }

        if let Err(error) = terminal.draw(|frame| ui::draw(frame, &app)) {
            break Err(error);
        }

        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('r') => {
                        trigger(&tx);
                        app.refreshing = true;
                        last_trigger = Instant::now();
                    }
                    KeyCode::Char(' ') => app.paused = !app.paused,
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    };

    ratatui::restore();
    result
}

fn trigger(tx: &mpsc::Sender<Vec<Report>>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(providers::fetch_all());
    });
}
