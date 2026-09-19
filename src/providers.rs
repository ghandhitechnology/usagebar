//! One adapter per provider. Every number that reaches the UI is read straight from a
//! vendor surface; nothing is estimated or extrapolated.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::model::{Report, Source, Window};

const UA: &str = concat!("usagebar/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(25);

pub type Result<T> = std::result::Result<T, String>;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn config_dir(env_key: &str, default: &str) -> PathBuf {
    std::env::var(env_key)
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(default))
}

fn read_json(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<(u16, Value)> {
    let mut req = ureq::get(url)
        .config()
        .timeout_global(Some(TIMEOUT))
        .build()
        .header("User-Agent", UA)
        .header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut res = req
        .call()
        .map_err(|e| format!("{url}: {}", transport_error(e)))?;
    let status = res.status().as_u16();
    let body = res
        .body_mut()
        .read_json::<Value>()
        .map_err(|e| format!("{url}: unreadable response: {e}"))?;
    Ok((status, body))
}

fn post_json(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<(u16, Value)> {
    let mut req = ureq::post(url)
        .config()
        .timeout_global(Some(TIMEOUT))
        .build()
        .header("User-Agent", UA)
        .header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut res = req
        .send_json(body)
        .map_err(|e| format!("{url}: {}", transport_error(e)))?;
    let status = res.status().as_u16();
    let body = res.body_mut().read_json::<Value>().unwrap_or(Value::Null);
    Ok((status, body))
}

/// ureq folds the HTTP status into the error type; keep the vendor's own words.
fn transport_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(code) => format!("HTTP {code}"),
        other => other.to_string(),
    }
}

/// Vendors are inconsistent about encoding: Command Code and Devin both send numbers as
/// strings, so every numeric read goes through here rather than trusting the JSON type.
fn f(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(raw) => raw.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn dt(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    match v.get(key)? {
        Value::String(raw) => DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|d| d.with_timezone(&Utc))
            .or_else(|| epoch_seconds(raw.trim().parse::<f64>().ok()?)),
        Value::Number(n) => epoch_seconds(n.as_f64()?),
        _ => None,
    }
}

fn epoch_seconds(raw: f64) -> Option<DateTime<Utc>> {
    // Vendors mix seconds and milliseconds; anything past the year 2200 is ms.
    let seconds = if raw > 7_258_118_400.0 {
        raw / 1000.0
    } else {
        raw
    };
    Utc.timestamp_opt(seconds as i64, 0).single()
}

fn ms_remaining_to_seconds(remaining: f64) -> f64 {
    100.0 - remaining
}

/// Compact thousands so a credit count fits beside a bar: 187091 becomes "187k".
fn compact(value: f64) -> String {
    if value.abs() >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value.abs() >= 1_000.0 {
        format!("{:.0}k", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

// ---------------------------------------------------------------- claude

const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

pub fn claude() -> Result<Report> {
    let dir = config_dir("CLAUDE_CONFIG_DIR", ".claude");
    let path = dir.join(".credentials.json");
    let creds = read_json(&path)?;
    let oauth = creds
        .get("claudeAiOauth")
        .ok_or("no claudeAiOauth in credentials (API-key login has no quota)")?
        .clone();

    let token = match token_for(&path, &oauth) {
        Ok(t) => t,
        Err(why) => return Ok(Report::failed("Claude", why)),
    };

    let (status, body) = get_json(
        "https://api.anthropic.com/api/oauth/usage",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("anthropic-beta", "oauth-2025-04-20"),
        ],
    )?;
    if status == 401 || status == 403 {
        return Ok(Report::failed(
            "Claude",
            "token rejected; run any Claude Code command to refresh it".into(),
        ));
    }

    let mut report = Report::new("Claude").plan(
        oauth
            .get("subscriptionType")
            .and_then(Value::as_str)
            .map(str::to_string),
    );

    // Newer responses carry a self-describing limits array; older ones carry fixed buckets.
    let limits = body.get("limits").and_then(Value::as_array);
    match limits {
        Some(list) if !list.is_empty() => {
            for entry in list {
                let kind = s(entry, "kind").unwrap_or_default();
                let label = match kind.as_str() {
                    "session" => "Session".to_string(),
                    "weekly_all" => "Weekly".to_string(),
                    "weekly_scoped" => format!(
                        "Weekly · {}",
                        entry
                            .pointer("/scope/model/display_name")
                            .and_then(Value::as_str)
                            .unwrap_or("scoped")
                    ),
                    other => other.to_string(),
                };
                let Some(percent) = f(entry, "percent") else {
                    continue;
                };
                report
                    .windows
                    .push(Window::new(label, percent).reset_at(dt(entry, "resets_at")));
            }
        }
        _ => {
            for (key, label) in [
                ("five_hour", "Session"),
                ("seven_day", "Weekly"),
                ("seven_day_sonnet", "Weekly · Sonnet"),
                ("seven_day_opus", "Weekly · Opus"),
            ] {
                let Some(bucket) = body.get(key).filter(|v| !v.is_null()) else {
                    continue;
                };
                let Some(percent) = f(bucket, "utilization") else {
                    continue;
                };
                report
                    .windows
                    .push(Window::new(label, percent).reset_at(dt(bucket, "resets_at")));
            }
        }
    }

    if let Some(extra) = body.get("extra_usage").filter(|v| !v.is_null()) {
        if extra
            .get("is_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let used = f(extra, "used_credits").unwrap_or(0.0);
            let cap = f(extra, "monthly_limit");
            report.notes.push(match cap {
                Some(cap) => format!("extra usage ${used:.2} / ${cap:.2}"),
                None => format!("extra usage ${used:.2}"),
            });
        }
    }

    if report.windows.is_empty() {
        report.health = crate::model::Health::NoQuota("no windows reported".into());
    }
    Ok(report)
}

/// Returns a usable access token. Claude Code owns the refresh token, so we only refresh
/// when the stored one is expired and we write the rotated pair back the way Claude Code
/// does. The re-read guards against clobbering a refresh that happened while we worked.
fn token_for(path: &Path, oauth: &Value) -> Result<String> {
    let access = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or("no accessToken")?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let fresh_enough = expires_at > Utc::now().timestamp_millis() + 60_000;
    if fresh_enough {
        return Ok(access);
    }

    let refresh = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .ok_or("token expired and no refreshToken")?
        .to_string();
    let client_id = std::env::var("CLAUDE_CODE_OAUTH_CLIENT_ID")
        .unwrap_or_else(|_| CLAUDE_OAUTH_CLIENT_ID.to_string());
    let (status, body) = post_json(
        "https://console.anthropic.com/v1/oauth/token",
        &[
            ("Content-Type", "application/json"),
            ("anthropic-beta", "oauth-2025-04-20"),
        ],
        &serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh,
            "client_id": client_id,
        }),
    )?;
    let new_access = body.get("access_token").and_then(Value::as_str);
    let Some(new_access) = new_access else {
        return Err(match status {
            200 => "refresh returned no access_token".to_string(),
            _ => format!("refresh failed (HTTP {status}); run a Claude Code command once"),
        });
    };
    let new_refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or(&refresh);
    let expires_in = f(&body, "expires_in").unwrap_or(3600.0) as i64;
    if let Err(why) = write_back_credentials(path, &refresh, new_access, new_refresh, expires_in) {
        // Serving the token still beats failing; the next poll retries the write.
        eprintln!("usagebar: could not persist refreshed Claude credentials: {why}");
    }
    Ok(new_access.to_string())
}

fn write_back_credentials(
    path: &Path,
    expected_refresh: &str,
    access: &str,
    refresh: &str,
    expires_in_secs: i64,
) -> Result<()> {
    let mut doc = read_json(path)?;
    let stored_refresh = doc
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if stored_refresh != expected_refresh {
        return Err("another process refreshed first; leaving its token alone".into());
    }
    let slot = doc
        .pointer_mut("/claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or("credentials shape changed")?;
    slot.insert("accessToken".into(), Value::String(access.to_string()));
    slot.insert("refreshToken".into(), Value::String(refresh.to_string()));
    slot.insert(
        "expiresAt".into(),
        Value::Number((Utc::now().timestamp_millis() + expires_in_secs * 1000).into()),
    );
    let tmp = path.with_extension("json.usagebar-tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------- codex

pub fn codex() -> Result<Report> {
    let dir = config_dir("CODEX_HOME", ".codex");
    match codex_live(&dir) {
        Ok(report) if report.health_is_ok() => Ok(report),
        live => match codex_rollout(&dir) {
            Ok(fallback) => Ok(fallback),
            Err(local_why) => live.map_err(|e| format!("{e}; local rollouts: {local_why}")),
        },
    }
}

impl Report {
    fn health_is_ok(&self) -> bool {
        matches!(self.health, crate::model::Health::Ok)
    }
}

fn codex_live(dir: &Path) -> Result<Report> {
    let auth = read_json(&dir.join("auth.json"))?;
    let token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .ok_or("no tokens.access_token in auth.json")?;
    let mut headers = vec![("Authorization", format!("Bearer {token}"))];
    if let Some(account) = auth.pointer("/tokens/account_id").and_then(Value::as_str) {
        headers.push(("ChatGPT-Account-Id", account.to_string()));
    }
    let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let (status, body) = get_json("https://chatgpt.com/backend-api/wham/usage", &borrowed)?;
    if status == 401 || status == 403 {
        return Ok(Report::failed(
            "Codex",
            "token rejected by chatgpt.com".into(),
        ));
    }
    Ok(codex_from_wham(&body))
}

fn codex_from_wham(body: &Value) -> Report {
    let mut report = Report::new("Codex")
        .account(s(body, "email"))
        .plan(s(body, "plan_type"));
    if let Some(limit) = body.get("rate_limit") {
        for key in ["primary_window", "secondary_window"] {
            let Some(window) = limit.get(key).filter(|v| !v.is_null()) else {
                continue;
            };
            let Some(percent) = f(window, "used_percent") else {
                continue;
            };
            let seconds = f(window, "limit_window_seconds").unwrap_or(0.0);
            let reset =
                f(window, "reset_at").and_then(|secs| Utc.timestamp_opt(secs as i64, 0).single());
            report.windows.push(
                Window::new(window_label(seconds), percent)
                    .reset_at(reset)
                    .detail(window_label_detail(seconds)),
            );
        }
    }
    if let Some(credits) = body.get("credits") {
        if credits
            .get("has_credits")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let balance = credits
                .get("balance")
                .map(|b| {
                    b.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| b.to_string())
                })
                .unwrap_or_else(|| "0".into());
            report.notes.push(format!("credits {balance}"));
        }
    }
    if let Some(count) = body
        .pointer("/rate_limit_reset_credits/available_count")
        .and_then(Value::as_i64)
    {
        if count > 0 {
            report.notes.push(format!("{count} banked resets"));
        }
    }
    if report.windows.is_empty() {
        report.health = crate::model::Health::NoQuota("rate_limit empty".into());
    }
    report
}

fn window_label(seconds: f64) -> String {
    match seconds as i64 {
        18_000 => "Session".into(),
        604_800 => "Weekly".into(),
        other if other >= 86_400 => format!("{}d", other / 86_400),
        other if other >= 3_600 => format!("{}h", other / 3_600),
        other => format!("{other}m"),
    }
}

fn window_label_detail(seconds: f64) -> Option<String> {
    match seconds as i64 {
        18_000 => Some("5h window".into()),
        604_800 => Some("7d window".into()),
        _ => None,
    }
}

/// Last recorded rate-limit snapshot in the newest rollout, for when the live call fails.
fn codex_rollout(dir: &Path) -> Result<Report> {
    let newest = newest_file(&dir.join("sessions"), ".jsonl")?;
    let tail = tail_of(&newest, 512 * 1024)?;
    let line = tail
        .lines()
        .rev()
        .find(|line| line.contains("\"rate_limits\""))
        .ok_or("no rate_limits recorded yet")?;
    let event: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let limits = event
        .pointer("/payload/rate_limits")
        .ok_or("rate_limits missing from event")?;

    let mut report = Report::new("Codex").source(Source::LocalFile);
    for key in ["primary", "secondary"] {
        let Some(window) = limits.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let Some(percent) = f(window, "used_percent") else {
            continue;
        };
        let minutes = f(window, "window_minutes").unwrap_or(0.0);
        report.windows.push(
            Window::new(window_label(minutes * 60.0), percent).reset_at(dt(window, "resets_at")),
        );
    }
    report.notes.push("from last local rollout".into());
    Ok(report)
}

fn newest_file(root: &Path, ext: &str) -> Result<PathBuf> {
    fn walk(dir: &Path, ext: &str, best: &mut Option<(std::time::SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, ext, best);
            } else if path.to_string_lossy().ends_with(ext) {
                if let Ok(meta) = entry.metadata() {
                    let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                        *best = Some((modified, path));
                    }
                }
            }
        }
    }
    let mut best = None;
    walk(root, ext, &mut best);
    best.map(|(_, p)| p)
        .ok_or_else(|| format!("no {ext} under {}", root.display()))
}

fn tail_of(path: &Path, bytes: u64) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let start = len.saturating_sub(bytes);
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

// ---------------------------------------------------------------- opencode go

/// Credential ids that answered the Go usage call last time, so a refresh does not have to
/// re-probe every key in the store.
static OPENCODE_KEYS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn opencode_go() -> Result<Vec<Report>> {
    let db = config_dir("OPENCODE_DATA_DIR", ".local/share/opencode").join("opencode.db");
    let known: Vec<String> = OPENCODE_KEYS.lock().unwrap().clone();

    let candidates = if known.is_empty() {
        credential_keys(&db)?
    } else {
        known
            .iter()
            .map(|id| (id.clone(), key_by_id(&db, id).unwrap_or_default()))
            .collect()
    };

    let mut reports = Vec::new();
    let mut working = Vec::new();
    let mut errors = Vec::new();
    for (id, key) in candidates {
        if key.is_empty() {
            continue;
        }
        match go_usage(&key) {
            Ok(report) => {
                working.push(id.clone());
                reports.push(report);
            }
            Err(why) => errors.push(why),
        }
    }

    let mut learned = OPENCODE_KEYS.lock().unwrap();
    if !working.is_empty() {
        // A probe run replaces the cache; a cached run keeps it unless something broke.
        if known.is_empty() {
            *learned = working;
        }
    } else {
        learned.clear();
    }

    if reports.is_empty() {
        return Err(errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no OpenCode Go credentials in the local store".into()));
    }

    // The credential store carries no account names, so when several keys are live the
    // only honest label is an ordinal, ordered by how loaded each one is.
    reports.sort_by(|a, b| {
        b.peak()
            .unwrap_or(0.0)
            .partial_cmp(&a.peak().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if reports.len() > 1 {
        for (index, report) in reports.iter_mut().enumerate() {
            report.account = Some(format!("#{}", index + 1));
        }
    }
    Ok(reports)
}

fn credential_keys(db: &Path) -> Result<Vec<(String, String)>> {
    let raw = sqlite_rows(db, "select id, value from credential")?;
    let mut out = Vec::new();
    for (id, value) in raw {
        let Ok(parsed) = serde_json::from_str::<Value>(&value) else {
            continue;
        };
        if parsed.get("type").and_then(Value::as_str) != Some("key") {
            continue;
        }
        if let Some(key) = parsed.get("key").and_then(Value::as_str) {
            out.push((id, key.to_string()));
        }
    }
    Ok(out)
}

fn key_by_id(db: &Path, id: &str) -> Result<String> {
    let rows = sqlite_rows(
        db,
        &format!("select id, value from credential where id = '{id}'"),
    )?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|(_, value)| {
            serde_json::from_str::<Value>(&value)
                .ok()?
                .get("key")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_default())
}

/// Reads through the system sqlite3 binary; the store is a plain table and this keeps the
/// dependency list short.
fn sqlite_rows(db: &Path, query: &str) -> Result<Vec<(String, String)>> {
    let out = std::process::Command::new("sqlite3")
        .arg("-separator")
        .arg("\u{1f}")
        .arg(db)
        .arg(query)
        .output()
        .map_err(|e| format!("sqlite3: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sqlite3: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once('\u{1f}'))
        .map(|(id, value)| (id.to_string(), value.to_string()))
        .collect())
}

fn go_usage(key: &str) -> Result<Report> {
    let (status, body) = get_json(
        "https://opencode.ai/zen/go/v1/usage",
        &[("Authorization", &format!("Bearer {key}"))],
    )?;
    if status != 200 || body.get("usage").is_none() {
        return Err(format!("key rejected (HTTP {status})"));
    }
    let mut report = Report::new("OpenCode Go");
    for (key, label) in [
        ("rolling", "Session"),
        ("weekly", "Weekly"),
        ("monthly", "Monthly"),
    ] {
        let Some(window) = body
            .pointer(&format!("/usage/{key}"))
            .filter(|v| !v.is_null())
        else {
            continue;
        };
        let Some(percent) = f(window, "percent") else {
            continue;
        };
        report
            .windows
            .push(Window::new(label, percent).reset_at(dt(window, "resetsAt")));
    }
    Ok(report)
}

// ---------------------------------------------------------------- cursor

pub fn cursor() -> Result<Report> {
    let dir = config_dir("CURSOR_CONFIG_DIR", ".cursor");
    let auth = read_json(&dir.join("auth.json"))?;
    let token = auth
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or("no accessToken in ~/.cursor/auth.json")?;
    let subject = jwt_claim(token, "sub").ok_or("accessToken is not a JWT with a sub claim")?;
    let cookie = format!(
        "WorkosCursorSessionToken={}",
        urlencode(&format!("{subject}::{token}"))
    );

    let (status, body) = get_json(
        "https://cursor.com/api/usage-summary",
        &[("Cookie", &cookie)],
    )?;
    if status == 401 || status == 403 {
        return Ok(Report::failed(
            "Cursor",
            "session expired; sign in with cursor-agent to refresh it".into(),
        ));
    }

    let mut report = Report::new("Cursor")
        .plan(s(&body, "membershipType"))
        .source(Source::Api);
    let plan = body.pointer("/individualUsage/plan");
    if let Some(plan) = plan {
        let breakdown = plan.get("breakdown");
        let detail = breakdown.and_then(|b| {
            let included = f(b, "included")?;
            let bonus = f(b, "bonus").unwrap_or(0.0);
            Some(format!(
                "{} + {} bonus credits",
                compact(included),
                compact(bonus)
            ))
        });
        for (key, label) in [
            ("totalPercentUsed", "Total"),
            ("autoPercentUsed", "Auto"),
            ("apiPercentUsed", "API"),
        ] {
            if let Some(percent) = f(plan, key) {
                report.windows.push(
                    Window::new(label, percent)
                        .reset_at(dt(&body, "billingCycleEnd"))
                        .detail(if key == "totalPercentUsed" {
                            detail.clone()
                        } else {
                            None
                        }),
                );
            }
        }
    }
    if let Some(on_demand) = body.pointer("/individualUsage/onDemand") {
        if on_demand
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let used = f(on_demand, "used").unwrap_or(0.0);
            report.notes.push(format!("on-demand: {used:.0}"));
        }
    }
    if report.windows.is_empty() {
        report.health = crate::model::Health::NoQuota("no individual usage in response".into());
    }
    Ok(report)
}

/// The dashboard cookie is `sub::jwt`, and `cursor-auth.json` only ships the jwt.
fn jwt_claim(token: &str, claim: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    let doc: Value = serde_json::from_slice(&bytes).ok()?;
    doc.get(claim)?.as_str().map(str::to_string)
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut lookup = [255u8; 256];
    for (i, byte) in TABLE.iter().enumerate() {
        lookup[*byte as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------- grok

pub fn grok() -> Result<Report> {
    let dir = config_dir("GROK_HOME", ".grok");
    let auth = read_json(&dir.join("auth.json"))?;
    let token = auth
        .as_object()
        .and_then(|map| map.values().find_map(|entry| s(entry, "key")))
        .ok_or("no session key in ~/.grok/auth.json")?;

    let (status, body) = get_json(
        "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("x-xai-token-auth", "xai-grok-cli"),
        ],
    )?;
    if status == 401 || status == 403 {
        return Ok(Report::failed(
            "Grok",
            "session expired; run `grok login`".into(),
        ));
    }

    let config = body.get("config").ok_or("billing response had no config")?;
    let mut report = Report::new("Grok");
    match f(config, "creditUsagePercent") {
        Some(percent) => {
            let end = dt(config, "billingPeriodEnd").or_else(|| {
                config
                    .pointer("/currentPeriod/end")
                    .and_then(|_| dt(config, "billingPeriodEnd"))
            });
            report
                .windows
                .push(Window::new("Weekly", percent).reset_at(end));
        }
        None => {
            report.health =
                crate::model::Health::NoQuota("no metered quota on this account".into());
        }
    }
    let prepaid = config
        .pointer("/prepaidBalance/val")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if prepaid > 0.0 {
        report.notes.push(format!("prepaid ${prepaid:.2}"));
    }
    let on_demand = config
        .pointer("/onDemandUsed/val")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if on_demand > 0.0 {
        report.notes.push(format!("on-demand ${on_demand:.2}"));
    }
    Ok(report)
}

// ---------------------------------------------------------------- devin

pub fn devin() -> Result<Report> {
    let path = config_dir("XDG_DATA_HOME", ".local/share").join("devin/credentials.toml");
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let key = raw
        .lines()
        .find_map(|line| line.strip_prefix("windsurf_api_key"))
        .and_then(|rest| rest.split('"').nth(1))
        .ok_or("no windsurf_api_key in devin credentials")?;

    let (status, body) = post_json(
        "https://server.codeium.com/exa.api_server_pb.ApiServerService/GetUserStatus",
        &[
            ("Content-Type", "application/json"),
            ("Connect-Protocol-Version", "1"),
        ],
        &serde_json::json!({
            "metadata": {
                "apiKey": key,
                "ideName": "windsurf",
                "ideVersion": "1.0.0",
                "extensionVersion": "1.0.0",
                "locale": "en",
                "os": std::env::consts::OS,
            }
        }),
    )?;
    if status != 200 {
        return Ok(Report::failed(
            "Devin",
            format!("status endpoint returned HTTP {status}"),
        ));
    }
    let status_obj = body.get("userStatus").ok_or("no userStatus in response")?;
    let plan_status = status_obj.get("planStatus");

    let mut report = Report::new("Devin").account(s(status_obj, "email")).plan(
        status_obj
            .pointer("/planStatus/planInfo/planName")
            .and_then(Value::as_str)
            .map(str::to_string),
    );

    if let Some(plan_status) = plan_status {
        // Devin reports what is *remaining*; the UI shows used, so flip it here.
        for (key, label, reset_key) in [
            (
                "dailyQuotaRemainingPercent",
                "Daily",
                "dailyQuotaResetAtUnix",
            ),
            (
                "weeklyQuotaRemainingPercent",
                "Weekly",
                "weeklyQuotaResetAtUnix",
            ),
        ] {
            if let Some(remaining) = f(plan_status, key) {
                report.windows.push(
                    Window::new(label, ms_remaining_to_seconds(remaining))
                        .reset_at(dt(plan_status, reset_key)),
                );
            }
        }
        if let Some(credits) = f(plan_status, "availableFlexCredits") {
            report.notes.push(format!("{credits:.0} flex credits"));
        }
    }
    if report.windows.is_empty() {
        report.health = crate::model::Health::NoQuota("plan reports no quota windows".into());
    }
    Ok(report)
}

// ---------------------------------------------------------------- command code

pub fn command_code() -> Result<Report> {
    let dir = config_dir("COMMANDCODE_HOME", ".commandcode");
    let auth = read_json(&dir.join("auth.json"))?;
    let key = auth
        .as_object()
        .and_then(|map| {
            map.get("apiKey").and_then(Value::as_str).or_else(|| {
                map.values().find_map(|entry| {
                    (entry.get("type").and_then(Value::as_str) == Some("key"))
                        .then(|| entry.get("key").and_then(Value::as_str))
                        .flatten()
                })
            })
        })
        .ok_or("no apiKey in ~/.commandcode/auth.json")?;

    let (status, body) = get_json(
        "https://api.commandcode.ai/alpha/billing/credits",
        &[("Authorization", &format!("Bearer {key}"))],
    )?;
    if status == 401 || status == 403 {
        return Ok(Report::failed(
            "Command Code",
            "key rejected; run `command-code` login again".into(),
        ));
    }

    let mut report = Report::new("Command Code");
    if let Some(limits) = body.get("windowLimits") {
        for (key, label) in [("fiveHour", "Session"), ("weekly", "Weekly")] {
            let Some(window) = limits.get(key).filter(|v| !v.is_null()) else {
                continue;
            };
            let (Some(used), Some(cap)) = (f(window, "used"), f(window, "cap")) else {
                continue;
            };
            if cap <= 0.0 {
                continue;
            }
            report.windows.push(
                Window::new(label, used / cap * 100.0)
                    .reset_at(dt(window, "resetAt"))
                    .detail(Some(format!("${used:.2} / ${cap:.2}"))),
            );
        }
    }
    if let Some(credits) = f(&body, "credits").or_else(|| {
        body.pointer("/credits/monthlyCredits")
            .and_then(Value::as_f64)
    }) {
        let purchased = body
            .pointer("/credits/purchasedCredits")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        report.notes.push(format!(
            "${credits:.2} credits left (${purchased:.2} purchased)"
        ));
    }
    if report.windows.is_empty() {
        report.health = crate::model::Health::NoQuota("account reports no window limits".into());
    }
    Ok(report)
}

// ---------------------------------------------------------------- fan-out

/// Every adapter runs on its own thread so one slow vendor cannot hold up the rest.
pub fn fetch_all() -> Vec<Report> {
    let handles: Vec<_> = vec![
        spawn("Claude", || vec![claude()]),
        spawn("Codex", || vec![codex()]),
        spawn("OpenCode Go", || match opencode_go() {
            Ok(reports) => reports.into_iter().map(Ok).collect(),
            Err(why) => vec![Err(why)],
        }),
        spawn("Cursor", || vec![cursor()]),
        spawn("Grok", || vec![grok()]),
        spawn("Devin", || vec![devin()]),
        spawn("Command Code", || vec![command_code()]),
    ];

    let mut reports: Vec<Report> = handles
        .into_iter()
        .flat_map(|handle| {
            handle
                .join()
                .unwrap_or_else(|_| vec![Report::failed("Provider", "adapter panicked".into())])
        })
        .collect();
    reports.sort_by(|a, b| {
        let rank = |r: &Report| match r.health {
            crate::model::Health::Ok => 0,
            crate::model::Health::Stale { .. } => 1,
            crate::model::Health::NoQuota(_) => 2,
            crate::model::Health::Unavailable(_) => 3,
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| {
                b.peak()
                    .unwrap_or(0.0)
                    .partial_cmp(&a.peak().unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.provider.cmp(b.provider))
    });
    reports
}

/// Turns a single-report adapter into the multi-report shape the fan-out wants, naming the
/// provider in any failure so the card is still identifiable.
fn spawn(
    name: &'static str,
    adapter: impl FnOnce() -> Vec<Result<Report>> + Send + 'static,
) -> std::thread::JoinHandle<Vec<Report>> {
    std::thread::spawn(move || {
        adapter()
            .into_iter()
            .map(|result| result.unwrap_or_else(|why| Report::failed(name, why)))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vendors disagree about units and encodings; these are the shapes seen in the wild.
    #[test]
    fn epoch_handles_seconds_millis_and_strings() {
        assert_eq!(
            dt(&serde_json::json!({"t": 1789804800}), "t")
                .unwrap()
                .timestamp(),
            1789804800
        );
        assert_eq!(
            dt(&serde_json::json!({"t": "1789804800"}), "t")
                .unwrap()
                .timestamp(),
            1789804800
        );
        assert_eq!(
            dt(&serde_json::json!({"t": 1789804800000i64}), "t")
                .unwrap()
                .timestamp(),
            1789804800
        );
        assert!(dt(&serde_json::json!({"t": "not a time"}), "t").is_none());
    }

    #[test]
    fn numeric_fields_accept_string_encodings() {
        assert_eq!(f(&serde_json::json!({"p": 12.5}), "p"), Some(12.5));
        assert_eq!(f(&serde_json::json!({"p": "12.5"}), "p"), Some(12.5));
        assert_eq!(f(&serde_json::json!({"p": null}), "p"), None);
    }

    #[test]
    fn jwt_payload_decodes_the_cursor_subject() {
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJnb29nbGUtb2F1dGgyfHVzZXJfMDEifQ.sig";
        assert_eq!(
            jwt_claim(token, "sub").as_deref(),
            Some("google-oauth2|user_01")
        );
        assert_eq!(jwt_claim("not-a-jwt", "sub"), None);
    }

    #[test]
    fn credit_counts_stay_readable() {
        assert_eq!(compact(187091.0), "187k");
        assert_eq!(compact(40000.0), "40k");
        assert_eq!(compact(1200000.0), "1.2M");
        assert_eq!(compact(420.0), "420");
    }

    /// Devin reports remaining quota; the UI shows used.
    #[test]
    fn remaining_percent_is_flipped() {
        assert_eq!(ms_remaining_to_seconds(40.0), 60.0);
        assert_eq!(ms_remaining_to_seconds(0.0), 100.0);
    }
}
