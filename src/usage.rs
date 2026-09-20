//! Tokens per day per provider, read from the CLIs' own session logs. Nothing here
//! talks to a vendor: these are the numbers the local tools already recorded, grouped
//! by provider so two accounts of one vendor draw as one series.

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde_json::Value;

use crate::fsutil;
use crate::model::ProviderId;
use crate::providers;

/// How far back the chart looks.
pub const WINDOW_DAYS: u64 = 21;

/// One day of the chart: what every provider spent that day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub day: NaiveDate,
    pub total: u64,
    /// Per provider, biggest share first, so a stack is drawn the same way twice.
    pub parts: Vec<(ProviderId, u64)>,
}

/// Tokens per provider per day. Days with no activity are absent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Usage {
    days: BTreeMap<NaiveDate, BTreeMap<ProviderId, u64>>,
}

impl Usage {
    pub fn add(&mut self, day: NaiveDate, provider: ProviderId, tokens: u64) {
        if tokens == 0 {
            return;
        }
        *self
            .days
            .entry(day)
            .or_default()
            .entry(provider)
            .or_default() += tokens;
    }

    /// Provider totals across the whole window, biggest first.
    pub fn totals(&self) -> Vec<(ProviderId, u64)> {
        let mut totals: BTreeMap<ProviderId, u64> = BTreeMap::new();
        for day in self.days.values() {
            for (provider, tokens) in day {
                *totals.entry(*provider).or_default() += tokens;
            }
        }
        let mut out: Vec<(ProviderId, u64)> = totals.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// The last `days` days ending today, oldest first, including the quiet ones: a
    /// chart needs a column for every day, not only the days that had traffic.
    pub fn columns(&self, today: NaiveDate, days: u64) -> Vec<Column> {
        let oldest = today
            .checked_sub_days(Days::new(days.saturating_sub(1)))
            .unwrap_or(today);
        (0..days)
            .filter_map(|offset| oldest.checked_add_days(Days::new(offset)))
            .filter(|day| *day <= today)
            .map(|day| {
                let empty = BTreeMap::new();
                let spent = self.days.get(&day).unwrap_or(&empty);
                let mut parts: Vec<(ProviderId, u64)> = spent
                    .iter()
                    .map(|(provider, tokens)| (*provider, *tokens))
                    .collect();
                parts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                Column {
                    day,
                    total: parts.iter().map(|(_, tokens)| *tokens).sum(),
                    parts,
                }
            })
            .collect()
    }
}

/// Read every log the machine keeps. Runs on its own thread: the Codex rollouts alone
/// are several gigabytes, so this is done off the UI and only as often as it is asked.
pub fn scan(now: DateTime<Utc>) -> Usage {
    let mut usage = Usage::default();
    let oldest = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(WINDOW_DAYS.saturating_sub(1)))
        .unwrap_or_else(|| now.with_timezone(&Local).date_naive());
    let since = SystemTime::now() - Duration::from_secs(WINDOW_DAYS * 86_400);

    let claude_root = fsutil::dir_from("CLAUDE_CONFIG_DIR", ".claude").join("projects");
    scan_claude(&mut usage, &claude_root, oldest, since);

    let codex_root = fsutil::dir_from("CODEX_HOME", ".codex").join("sessions");
    scan_codex(&mut usage, &codex_root, oldest, since);

    scan_opencode(&mut usage, &providers::opencode_db(), oldest);

    usage
}

/// Every `.jsonl` under `root` written since `since`. A file untouched since then
/// cannot hold an event inside the window, so it is never opened.
fn logs(root: &Path, since: SystemTime) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.to_string_lossy().ends_with(".jsonl") {
                continue;
            }
            if meta.modified().map(|at| at >= since).unwrap_or(true) {
                found.push(path);
            }
        }
    }
    found
}

/// Every line of a log. Stops at the first unreadable one: a log being appended to can
/// end mid-line, and a persistently failing reader must not spin.
fn each_line(path: &Path, mut visit: impl FnMut(&str)) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        visit(&line);
    }
}

fn day_of(stamp: Option<&Value>) -> Option<NaiveDate> {
    let raw = stamp?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&Local).date_naive())
}

/// Claude Code writes one transcript per session; every assistant message carries the
/// usage of the turn that produced it.
fn scan_claude(usage: &mut Usage, root: &Path, oldest: NaiveDate, since: SystemTime) {
    for path in logs(root, since) {
        each_line(&path, |line| {
            // Cheap reject first: most lines are conversation, not usage.
            if !line.contains("\"usage\"") {
                return;
            }
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                return;
            };
            let Some(day) = day_of(event.get("timestamp")) else {
                return;
            };
            if day < oldest {
                return;
            }
            let Some(spent) = event.pointer("/message/usage") else {
                return;
            };
            let tokens: u64 = [
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
                "output_tokens",
            ]
            .iter()
            .filter_map(|key| spent.get(key).and_then(Value::as_u64))
            .sum();
            usage.add(day, ProviderId::Claude, tokens);
        });
    }
}

/// Codex rollouts record a running total per session plus the delta since the last
/// event; the deltas are what belong to a day.
fn scan_codex(usage: &mut Usage, root: &Path, oldest: NaiveDate, since: SystemTime) {
    for path in logs(root, since) {
        each_line(&path, |line| {
            if !line.contains("\"total_token_usage\"") {
                return;
            }
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                return;
            };
            let Some(day) = day_of(event.get("timestamp")) else {
                return;
            };
            if day < oldest {
                return;
            }
            let Some(delta) = event.pointer("/payload/info/last_token_usage") else {
                return;
            };
            let total = delta
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| {
                    ["input_tokens", "output_tokens"]
                        .iter()
                        .filter_map(|key| delta.get(key).and_then(Value::as_u64))
                        .sum()
                });
            usage.add(day, ProviderId::Codex, total);
        });
    }
}

/// The OpenCode database holds messages for several providers; only the Go
/// subscription is this tool's business. Current OpenCode versions write to
/// `session_message`; `message` remains for older versions and migrated rows.
fn scan_opencode(usage: &mut Usage, db: &Path, oldest: NaiveDate) {
    if !db.exists() {
        return;
    }

    // Query one extra UTC day because `oldest` is a local date. Rust applies the
    // machine timezone below, after SQLite has cheaply discarded older history.
    let cutoff = oldest
        .and_hms_opt(0, 0, 0)
        .map(|at| at.and_utc().timestamp_millis() - 86_400_000)
        .unwrap_or(0);
    let current = format!(
        "select id, time_created, \
         coalesce(json_extract(data, '$.tokens.input'), 0) + \
         coalesce(json_extract(data, '$.tokens.output'), 0) + \
         coalesce(json_extract(data, '$.tokens.reasoning'), 0) + \
         coalesce(json_extract(data, '$.tokens.cache.read'), 0) + \
         coalesce(json_extract(data, '$.tokens.cache.write'), 0) \
         from session_message \
         where type = 'assistant' \
         and json_extract(data, '$.model.providerID') = 'opencode-go' \
         and time_created >= {cutoff}"
    );
    let legacy = format!(
        "select id, time_created, \
         coalesce(json_extract(data, '$.tokens.total'), \
           coalesce(json_extract(data, '$.tokens.input'), 0) + \
           coalesce(json_extract(data, '$.tokens.output'), 0) + \
           coalesce(json_extract(data, '$.tokens.reasoning'), 0) + \
           coalesce(json_extract(data, '$.tokens.cache.read'), 0) + \
           coalesce(json_extract(data, '$.tokens.cache.write'), 0)) \
         from message \
         where json_extract(data, '$.providerID') = 'opencode-go' \
         and time_created >= {cutoff}"
    );

    let mut seen = HashSet::new();
    add_opencode_rows(usage, db, &current, oldest, &mut seen);
    add_opencode_rows(usage, db, &legacy, oldest, &mut seen);
}

/// Add the compact three-column result of an OpenCode query. Message IDs survive
/// OpenCode's schema migration, so reading the current table first prevents migrated
/// rows from being counted twice.
fn add_opencode_rows(
    usage: &mut Usage,
    db: &Path,
    query: &str,
    oldest: NaiveDate,
    seen: &mut HashSet<String>,
) {
    let Ok(out) = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg("-separator")
        .arg("\u{1f}")
        .arg(db)
        .arg(query)
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.splitn(3, '\u{1f}');
        let (Some(id), Some(created), Some(tokens)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(ms) = created.trim().parse::<i64>() else {
            continue;
        };
        let Some(day) =
            DateTime::from_timestamp_millis(ms).map(|at| at.with_timezone(&Local).date_naive())
        else {
            continue;
        };
        if day < oldest {
            continue;
        }
        let Ok(tokens) = tokens.trim().parse::<u64>() else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        usage.add(day, ProviderId::OpenCodeGo, tokens);
    }
}

/// Compact token counts the way a chart legend wants them: 187091 reads as "187k".
pub fn compact(tokens: u64) -> String {
    let value = tokens as f64;
    if value >= 1_000_000_000.0 {
        format!("{:.1}B", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.0}k", value / 1_000.0)
    } else {
        format!("{tokens}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("usagebar-usage-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A day that saw nothing still gets a column, or the chart would lie about where
    /// the quiet days are.
    #[test]
    fn a_quiet_day_keeps_its_column() {
        let mut usage = Usage::default();
        usage.add(day(2026, 9, 17), ProviderId::Codex, 500);
        usage.add(day(2026, 9, 19), ProviderId::Codex, 100);
        let columns = usage.columns(day(2026, 9, 19), 3);
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].day, day(2026, 9, 17));
        assert_eq!(columns[1].total, 0);
        assert!(columns[1].parts.is_empty());
        assert_eq!(columns[2].total, 100);
    }

    /// Two accounts of one provider are one series: the chart groups by vendor.
    #[test]
    fn a_provider_sums_all_of_its_accounts() {
        let mut usage = Usage::default();
        usage.add(day(2026, 9, 19), ProviderId::Claude, 300);
        usage.add(day(2026, 9, 19), ProviderId::Claude, 200);
        usage.add(day(2026, 9, 19), ProviderId::Codex, 100);
        let columns = usage.columns(day(2026, 9, 19), 1);
        assert_eq!(columns[0].total, 600);
        assert_eq!(
            columns[0].parts,
            vec![(ProviderId::Claude, 500), (ProviderId::Codex, 100)]
        );
        assert_eq!(usage.totals()[0], (ProviderId::Claude, 500));
    }

    /// Claude counts every token the vendor processed, cached reads included.
    #[test]
    fn claude_transcripts_sum_their_usage() {
        let dir = scratch("claude");
        let projects = dir.join("projects/-some-project");
        std::fs::create_dir_all(&projects).unwrap();
        let log = projects.join("session.jsonl");
        std::fs::write(
            &log,
            concat!(
                r#"{"type":"user","timestamp":"2026-09-19T10:00:00Z","message":{"content":"hi"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-09-19T10:00:05Z","message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":30,"cache_read_input_tokens":40,"output_tokens":8}}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-09-18T10:00:05Z","message":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                "\n",
                "not json at all\n",
            ),
        )
        .unwrap();
        let mut usage = Usage::default();
        scan_claude(
            &mut usage,
            &dir.join("projects"),
            day(2026, 9, 17),
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(
            usage.columns(day(2026, 9, 19), 3)[2].total,
            80,
            "input, cache creation, cache read and output all count"
        );
        assert_eq!(usage.columns(day(2026, 9, 19), 3)[1].total, 2);
    }

    /// Codex totals are cumulative per session, so the deltas are what land in a day.
    #[test]
    fn codex_events_count_their_delta_once() {
        let dir = scratch("codex");
        let sessions = dir.join("sessions/2026/09/19");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout.jsonl"),
            concat!(
                r#"{"timestamp":"2026-09-19T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110},"total_token_usage":{"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-09-19T10:01:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200,"output_tokens":20,"total_tokens":220},"total_token_usage":{"total_tokens":330}}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut usage = Usage::default();
        scan_codex(
            &mut usage,
            &dir.join("sessions"),
            day(2026, 9, 17),
            SystemTime::UNIX_EPOCH,
        );
        let columns = usage.columns(day(2026, 9, 19), 3);
        assert_eq!(
            columns[2].total, 330,
            "110 + 220, not the cumulative 330 twice"
        );
    }

    /// OpenCode's current schema nests the provider under `model` and splits the token
    /// total into components. Migrated messages also remain in the legacy table.
    #[test]
    fn opencode_reads_both_schemas_without_counting_migrations_twice() {
        let dir = scratch("opencode-schemas");
        let db = dir.join("opencode.db");
        let stamp = Local
            .with_ymd_and_hms(2026, 9, 19, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let sql = format!(
            r#"
            create table session_message (
                id text primary key, type text, time_created integer, data text
            );
            create table message (
                id text primary key, time_created integer, data text
            );
            insert into session_message values (
                'shared', 'assistant', {stamp},
                '{{"model":{{"providerID":"opencode-go"}},"tokens":{{"input":40,"output":10,"reasoning":5,"cache":{{"read":40,"write":5}}}}}}'
            );
            insert into session_message values (
                'current-only', 'assistant', {stamp},
                '{{"model":{{"providerID":"opencode-go"}},"tokens":{{"input":120,"output":20,"reasoning":10,"cache":{{"read":40,"write":10}}}}}}'
            );
            insert into message values (
                'shared', {stamp},
                '{{"providerID":"opencode-go","tokens":{{"total":999}}}}'
            );
            insert into message values (
                'legacy-only', {stamp},
                '{{"providerID":"opencode-go","tokens":{{"total":50}}}}'
            );
            "#
        );
        let status = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg(sql)
            .status()
            .unwrap();
        assert!(status.success());

        let mut usage = Usage::default();
        scan_opencode(&mut usage, &db, day(2026, 9, 19));
        assert_eq!(usage.columns(day(2026, 9, 19), 1)[0].total, 350);

        let status = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg("drop table session_message")
            .status()
            .unwrap();
        assert!(status.success());
        let mut legacy_usage = Usage::default();
        scan_opencode(&mut legacy_usage, &db, day(2026, 9, 19));
        assert_eq!(legacy_usage.columns(day(2026, 9, 19), 1)[0].total, 1049);
    }

    /// A log older than the window is never opened, however much is in it.
    #[test]
    fn logs_outside_the_window_are_skipped() {
        let dir = scratch("stale");
        let nested = dir.join("sessions/2026/08/01");
        std::fs::create_dir_all(&nested).unwrap();
        let log = nested.join("rollout.jsonl");
        std::fs::write(
            &log,
            r#"{"timestamp":"2026-08-01T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":9999}}}}"#,
        )
        .unwrap();
        let mut usage = Usage::default();
        // A cutoff after the file was written stands in for a mtime outside the window.
        let future = SystemTime::now() + Duration::from_secs(86_400);
        scan_codex(&mut usage, &dir.join("sessions"), day(2026, 8, 1), future);
        assert_eq!(usage.columns(day(2026, 8, 1), 1)[0].total, 0);
    }

    #[test]
    fn counts_stay_readable() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(12_400), "12k");
        assert_eq!(compact(2_450_000), "2.5M");
        assert_eq!(compact(3_100_000_000), "3.1B");
    }

    /// The chart's window runs up to and including today, and never into the future.
    #[test]
    fn the_window_ends_today() {
        let usage = Usage::default();
        let columns = usage.columns(day(2026, 9, 19), WINDOW_DAYS);
        assert_eq!(columns.len(), WINDOW_DAYS as usize);
        assert_eq!(columns.last().unwrap().day, day(2026, 9, 19));
        assert_eq!(columns.first().unwrap().day, day(2026, 8, 30));
    }
}
