//! One adapter per provider. Every number that reaches the UI is read straight from a
//! vendor surface; nothing is estimated or extrapolated.
//!
//! Adapters take a credential and return a report. Deciding which credential an
//! account uses, refreshing it, and writing it back are the credential module's job.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::config::SortMode;
use crate::credentials::{self, Credential, CredentialStore, MemoryStore, StoredCredential};
use crate::fsutil;
use crate::model::{AccountRef, Fact, Health, ProviderId, Report, Source, Window};

const UA: &str = concat!("usagebar/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(25);

pub type Result<T> = std::result::Result<T, String>;

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

// ---------------------------------------------------------------- detection

/// A credential found on this machine, before the user decides to keep it. Detection
/// only reads; it never writes a config or a store.
#[derive(Debug, Clone)]
pub struct Detected {
    pub provider: ProviderId,
    pub credential: Option<Credential>,
    pub origin: Option<PathBuf>,
    /// Set when a vendor file exists but holds nothing usable.
    pub error: Option<String>,
}

/// Scan the machine for the vendor credentials already in place.
pub fn detect() -> Vec<Detected> {
    let mut found = Vec::new();
    for provider in ProviderId::ALL {
        if provider == ProviderId::OpenCodeGo {
            found.extend(detect_opencode_go());
            continue;
        }
        let Some(path) = credentials::vendor_file(provider) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        match credentials::from_vendor_file(provider, &path) {
            Some(credential) => found.push(Detected {
                provider,
                credential: Some(credential),
                origin: Some(path),
                error: None,
            }),
            None => found.push(Detected {
                provider,
                credential: None,
                origin: Some(path.clone()),
                error: Some(format!("{} holds no usable credentials", path.display())),
            }),
        }
    }
    found
}

/// The OpenCode Go keys live in the OpenCode database, not in a vendor auth file.
fn opencode_db() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENCODE_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("opencode.db");
        }
    }
    fsutil::xdg_dir("XDG_DATA_HOME", "opencode", ".local/share/opencode").join("opencode.db")
}

/// The OpenCode store holds keys for several services, so a key counts as a Go
/// credential only when the Go endpoint answers for it.
fn detect_opencode_go() -> Vec<Detected> {
    let db = opencode_db();
    if !db.exists() {
        // OpenCode is not installed; that is not an error worth showing.
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut errors = Vec::new();
    match opencode_keys(&db) {
        Ok(keys) => {
            for (_id, key) in keys {
                match go_usage(&key) {
                    Ok(_) => found.push(Detected {
                        provider: ProviderId::OpenCodeGo,
                        credential: Some(Credential::Token { token: key }),
                        origin: None,
                        error: None,
                    }),
                    Err(why) => errors.push(why),
                }
            }
        }
        Err(why) => errors.push(why),
    }
    if found.is_empty() {
        found.push(Detected {
            provider: ProviderId::OpenCodeGo,
            credential: None,
            origin: Some(db),
            error: Some(
                errors
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "no working OpenCode Go key in the local store".into()),
            ),
        });
    }
    found
}

// ---------------------------------------------------------------- claude

const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

fn claude(credential: &Credential) -> Result<Report> {
    let Credential::ClaudeOauth {
        access_token,
        subscription_type,
        ..
    } = credential
    else {
        return Err("stored credential is not a Claude OAuth pair".into());
    };
    let (status, body) = get_json(
        "https://api.anthropic.com/api/oauth/usage",
        &[
            ("Authorization", &format!("Bearer {access_token}")),
            ("anthropic-beta", "oauth-2025-04-20"),
        ],
    )?;
    if status == 401 || status == 403 {
        return Err("token rejected; run any Claude Code command, usagebar will pick it up".into());
    }

    let mut report = Report::new(ProviderId::Claude).plan(subscription_type.clone());

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
            report = report.fact(
                "extra usage",
                match cap {
                    Some(cap) => format!("${used:.2} / ${cap:.2}"),
                    None => format!("${used:.2}"),
                },
            );
        }
    }

    if report.windows.is_empty() {
        report.health = Health::NoQuota("no windows reported".into());
    }
    Ok(report)
}

/// Refresh an expired Claude pair. Returns the pair to use now, and persists a rotated
/// one both here and (when it still matches) in the vendor file.
fn rotate_claude(
    account: &AccountRef,
    store: &dyn CredentialStore,
    stored: &StoredCredential,
) -> Result<StoredCredential> {
    let Credential::ClaudeOauth {
        refresh_token,
        expires_at,
        subscription_type,
        ..
    } = &stored.secret
    else {
        return Err("stored credential is not a Claude OAuth pair".into());
    };
    let now = Utc::now().timestamp_millis();
    if *expires_at > now + 60_000 {
        return Ok(stored.clone());
    }
    if refresh_token.is_empty() {
        return Err("token expired and no refresh token; run any Claude Code command".into());
    }
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
            "refresh_token": refresh_token,
            "client_id": client_id,
        }),
    )?;
    let Some(access) = body.get("access_token").and_then(Value::as_str) else {
        return Err(match status {
            200 => "refresh returned no access_token".to_string(),
            _ => format!("refresh failed (HTTP {status}); run any Claude Code command once"),
        });
    };
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or(refresh_token);
    let expires_in = f(&body, "expires_in").unwrap_or(3600.0) as i64;
    credentials::record(
        account,
        store,
        stored,
        Credential::ClaudeOauth {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at: now + expires_in * 1000,
            subscription_type: subscription_type.clone(),
        },
    )
}

// ---------------------------------------------------------------- codex

fn codex(credential: &Credential, origin: Option<&Path>, live_only: bool) -> Result<Report> {
    let Credential::CodexTokens {
        access_token,
        account_id,
        ..
    } = credential
    else {
        return Err("stored credential is not a Codex token set".into());
    };
    match codex_live(access_token, account_id.as_deref()) {
        Ok(report) if report.health_is_ok() => Ok(report),
        live => {
            if live_only {
                // Checking a credential must prove the token works; the local rollout
                // says nothing about that.
                return live;
            }
            // The newest local rollout is a fallback, not a source: it needs a Codex
            // home directory, which only exists when the credential came from a file.
            let fallback = origin
                .and_then(Path::parent)
                .map(codex_rollout)
                .transpose();
            match fallback {
                Ok(Some(report)) => Ok(report),
                _ => live,
            }
        }
    }
}

fn codex_live(access_token: &str, account_id: Option<&str>) -> Result<Report> {
    let mut headers = vec![("Authorization", format!("Bearer {access_token}"))];
    if let Some(account) = account_id {
        headers.push(("ChatGPT-Account-Id", account.to_string()));
    }
    let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let (status, body) = get_json("https://chatgpt.com/backend-api/wham/usage", &borrowed)?;
    if status == 401 || status == 403 {
        return Err("token rejected by chatgpt.com; run codex once, usagebar will pick it up".into());
    }
    Ok(codex_from_wham(&body))
}

fn codex_from_wham(body: &Value) -> Report {
    let mut report = Report::new(ProviderId::Codex)
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
        let balance = credits
            .get("balance")
            .map(|b| {
                b.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| b.to_string())
            })
            .unwrap_or_else(|| "0".into());
        let has = credits
            .get("has_credits")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if has || balance != "0" {
            report.facts.push(Fact::new("credits", balance.clone()));
        }
        for (key, label) in [
            ("unlimited", "unlimited"),
            ("overage_limit_reached", "overage limit reached"),
        ] {
            if credits.get(key).and_then(Value::as_bool).unwrap_or(false) {
                report.facts.push(Fact::quiet(label, "yes"));
            }
        }
    }
    for (key, label) in [
        ("approx_local_messages", "approx local messages"),
        ("approx_cloud_messages", "approx cloud messages"),
    ] {
        if let Some(values) = body
            .pointer(&format!("/credits/{key}"))
            .and_then(Value::as_array)
        {
            let numbers: Vec<String> = values
                .iter()
                .map(|v| {
                    v.as_f64()
                        .map(|n| format!("{n:.0}"))
                        .unwrap_or_else(|| "?".into())
                })
                .collect();
            if !numbers.is_empty() {
                report
                    .facts
                    .push(Fact::quiet(label, numbers.join(" / ")));
            }
        }
    }
    if let Some(count) = body
        .pointer("/rate_limit_reset_credits/available_count")
        .and_then(Value::as_i64)
    {
        let applicable = body
            .pointer("/rate_limit_reset_credits/applicable_available_count")
            .and_then(Value::as_i64);
        if count > 0 {
            report.facts.push(Fact::new("banked resets", count.to_string()));
        } else {
            report.facts.push(Fact::quiet("banked resets", "0"));
        }
        if let Some(applicable) = applicable {
            report
                .facts
                .push(Fact::quiet("resets usable now", applicable.to_string()));
        }
    }
    if let Some(models) = body.get("model_usage").and_then(Value::as_object) {
        for (name, entry) in models {
            let available = entry
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let value = match (available, dt(entry, "available_at")) {
                (true, _) => "available".to_string(),
                (false, Some(at)) => format!("unavailable until {}", at.to_rfc3339()),
                (false, None) => "unavailable".to_string(),
            };
            report.facts.push(Fact::quiet(name.clone(), value));
        }
    }
    if report.windows.is_empty() {
        report.health = Health::NoQuota("rate_limit empty".into());
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

    let mut report = Report::new(ProviderId::Codex).source(Source::LocalFile);
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

/// Keys in the OpenCode credential table that could belong to the Go endpoint.
fn opencode_keys(db: &Path) -> Result<Vec<(String, String)>> {
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

fn opencode_go(credential: &Credential) -> Result<Report> {
    let Credential::Token { token } = credential else {
        return Err("stored credential is not a Go key".into());
    };
    go_usage(token)
}

fn go_usage(key: &str) -> Result<Report> {
    let (status, body) = get_json(
        "https://opencode.ai/zen/go/v1/usage",
        &[("Authorization", &format!("Bearer {key}"))],
    )?;
    if status != 200 || body.get("usage").is_none() {
        return Err(format!("key rejected (HTTP {status})"));
    }
    let mut report = Report::new(ProviderId::OpenCodeGo);
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

fn cursor(credential: &Credential) -> Result<Report> {
    let Credential::Token { token } = credential else {
        return Err("stored credential is not a Cursor access token".into());
    };
    let subject = jwt_claim(token, "sub").ok_or("access token is not a JWT with a sub claim")?;
    let cookie = format!(
        "WorkosCursorSessionToken={}",
        urlencode(&format!("{subject}::{token}"))
    );

    let (status, body) = get_json(
        "https://cursor.com/api/usage-summary",
        &[("Cookie", &cookie)],
    )?;
    if status == 401 || status == 403 {
        return Err("session expired; sign in with cursor-agent, usagebar will pick it up".into());
    }

    let mut report = Report::new(ProviderId::Cursor)
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
            report = report.fact("on-demand", format!("{used:.0}"));
        }
    }
    if report.windows.is_empty() {
        report.health = Health::NoQuota("no individual usage in response".into());
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

fn grok(credential: &Credential) -> Result<Report> {
    let Credential::Token { token } = credential else {
        return Err("stored credential is not a Grok session key".into());
    };
    let (status, body) = get_json(
        "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("x-xai-token-auth", "xai-grok-cli"),
        ],
    )?;
    if status == 401 || status == 403 {
        return Err("session expired; run `grok login`, usagebar will pick it up".into());
    }

    let config = body.get("config").ok_or("billing response had no config")?;
    let mut report = Report::new(ProviderId::Grok);
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
            report.health = Health::NoQuota("no metered quota on this account".into());
        }
    }
    let prepaid = config
        .pointer("/prepaidBalance/val")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if prepaid > 0.0 {
        report = report.fact("prepaid", format!("${prepaid:.2}"));
    }
    let on_demand = config
        .pointer("/onDemandUsed/val")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if on_demand > 0.0 {
        report = report.fact("on-demand", format!("${on_demand:.2}"));
    }
    Ok(report)
}

// ---------------------------------------------------------------- devin

fn devin(credential: &Credential) -> Result<Report> {
    let Credential::Token { token: key } = credential else {
        return Err("stored credential is not a Devin API key".into());
    };
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
        return Err(format!("status endpoint returned HTTP {status}"));
    }
    let status_obj = body.get("userStatus").ok_or("no userStatus in response")?;
    let plan_status = status_obj.get("planStatus");

    let mut report = Report::new(ProviderId::Devin).account(s(status_obj, "email")).plan(
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
            report = report.fact("flex credits", format!("{credits:.0}"));
        }
    }
    if report.windows.is_empty() {
        report.health = Health::NoQuota("plan reports no quota windows".into());
    }
    Ok(report)
}

// ---------------------------------------------------------------- command code

fn command_code(credential: &Credential) -> Result<Report> {
    let Credential::Token { token: key } = credential else {
        return Err("stored credential is not a Command Code API key".into());
    };
    let (status, body) = get_json(
        "https://api.commandcode.ai/alpha/billing/credits",
        &[("Authorization", &format!("Bearer {key}"))],
    )?;
    if status == 401 || status == 403 {
        return Err("key rejected; run `command-code` login again".into());
    }

    let mut report = Report::new(ProviderId::CommandCode);
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
        report = report.fact(
            "credits left",
            format!("${credits:.2} (${purchased:.2} purchased)"),
        );
    }
    if report.windows.is_empty() {
        report.health = Health::NoQuota("account reports no window limits".into());
    }
    Ok(report)
}

// ---------------------------------------------------------------- fan-out

fn dispatch(
    provider: ProviderId,
    credential: &Credential,
    origin: Option<&Path>,
    mode: Mode,
) -> Result<Report> {
    match provider {
        ProviderId::Claude => claude(credential),
        ProviderId::Codex => codex(credential, origin, mode == Mode::Check),
        ProviderId::OpenCodeGo => opencode_go(credential),
        ProviderId::Cursor => cursor(credential),
        ProviderId::Grok => grok(credential),
        ProviderId::Devin => devin(credential),
        ProviderId::CommandCode => command_code(credential),
    }
}

/// How a fetch is allowed to behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The normal poll: refresh what is stale and persist the rotated credential.
    Poll,
    /// Read only. A check must never consume a refresh token or write a vendor file,
    /// because the user may walk away from it.
    Check,
}

/// One account, end to end: resolve the credential, refresh it if the provider does
/// that here, then read the vendor.
pub fn fetch_one(account: &AccountRef, store: &dyn CredentialStore) -> Report {
    fetch_with(account, store, Mode::Poll)
}

pub fn fetch_with(account: &AccountRef, store: &dyn CredentialStore, mode: Mode) -> Report {
    let failed = |why: String| {
        Report::failed(account.provider, why)
            .key(account.id.clone())
            .label(account.label.clone())
    };
    if let Some(problem) = &account.problem {
        return failed(problem.clone());
    }
    let stored = match credentials::current(account, store) {
        Ok(stored) => stored,
        Err(why) => return failed(why),
    };
    let stored = if account.provider == ProviderId::Claude && mode == Mode::Poll {
        match rotate_claude(account, store, &stored) {
            Ok(stored) => stored,
            Err(why) => return failed(why),
        }
    } else {
        stored
    };
    let origin = stored.origin.clone();
    match dispatch(account.provider, &stored.secret, origin.as_deref(), mode) {
        Ok(report) => report.key(account.id.clone()).label(account.label.clone()),
        Err(why) => failed(why),
    }
}

/// Turn a scan into the accounts a run will read, with their secrets in memory only.
/// Anything the user keeps goes through the wizard, which writes the real store.
pub fn accounts_from_detected(detected: &[Detected]) -> (Vec<AccountRef>, MemoryStore) {
    let store = MemoryStore::new();
    let mut accounts: Vec<AccountRef> = Vec::new();
    let go_total = detected
        .iter()
        .filter(|d| d.provider == ProviderId::OpenCodeGo && d.credential.is_some())
        .count();
    let mut go_seen = 0usize;
    for entry in detected {
        let mut account = AccountRef::new(next_account_id(&accounts, entry.provider), entry.provider);
        let Some(credential) = &entry.credential else {
            // A file that exists but holds nothing usable still deserves a panel that
            // says so, instead of the provider quietly disappearing.
            accounts.push(account.problem(
                entry
                    .error
                    .clone()
                    .unwrap_or_else(|| "no usable credentials".into()),
            ));
            continue;
        };
        if entry.provider == ProviderId::OpenCodeGo && go_total > 1 {
            // The credential store carries no account names; an ordinal is the only
            // honest label for the second Go key onward.
            go_seen += 1;
            account.label = Some(format!("#{go_seen}"));
        }
        store
            .put(
                &account.id,
                StoredCredential::new(credential.clone()).with_origin(entry.origin.clone()),
            )
            .expect("an in-memory store cannot fail");
        accounts.push(account);
    }
    (accounts, store)
}

fn next_account_id(accounts: &[AccountRef], provider: ProviderId) -> String {
    crate::config::free_id(provider, accounts.iter().map(|account| account.id.as_str()))
}

/// Every visible account runs on its own thread so one slow vendor cannot hold up the
/// rest. Hidden accounts are not fetched at all.
pub fn fetch_all(
    accounts: &[AccountRef],
    store: &Arc<dyn CredentialStore>,
    sort: SortMode,
) -> Vec<Report> {
    let visible: Vec<AccountRef> = accounts
        .iter()
        .filter(|account| !account.hidden)
        .cloned()
        .collect();
    let handles: Vec<_> = visible
        .iter()
        .map(|account| {
            let account = account.clone();
            let store = Arc::clone(store);
            std::thread::spawn(move || fetch_one(&account, store.as_ref()))
        })
        .collect();

    let mut reports: Vec<Report> = handles
        .into_iter()
        .zip(visible.iter())
        .map(|(handle, account)| {
            handle.join().unwrap_or_else(|_| {
                Report::failed(account.provider, "adapter panicked".into())
                    .key(account.id.clone())
                    .label(account.label.clone())
            })
        })
        .collect();
    sort_reports(&mut reports, accounts, sort);
    reports
}

/// Check a credential before the user commits to it. One live call, and the vendor's
/// own answer decides whether the account is usable. Nothing is written: a check must
/// not rotate a token or touch a vendor file, because the user may walk away from it.
pub fn verify(provider: ProviderId, credential: &Credential, origin: Option<&Path>) -> Result<Report> {
    let account = AccountRef::new("verify", provider);
    let store = MemoryStore::new();
    store.put(
        "verify",
        StoredCredential::new(credential.clone()).with_origin(origin.map(Path::to_path_buf)),
    )?;
    let report = fetch_with(&account, &store, Mode::Check);
    match report.health {
        Health::Ok | Health::NoQuota(_) => Ok(report),
        Health::Unavailable(why) | Health::Stale { why, .. } => Err(why),
    }
}

/// Manual keeps the account list order; smart is worst first, then most used, then the
/// list order again so the panels that move are the ones that have to.
pub fn sort_reports(reports: &mut [Report], accounts: &[AccountRef], mode: SortMode) {
    if mode == SortMode::Manual {
        return;
    }
    let position = |report: &Report| {
        accounts
            .iter()
            .position(|account| account.id == report.key)
            .unwrap_or(usize::MAX)
    };
    reports.sort_by(|a, b| {
        let rank = |r: &Report| match r.health {
            Health::Ok => 0,
            Health::Stale { .. } => 1,
            Health::NoQuota(_) => 2,
            Health::Unavailable(_) => 3,
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| {
                b.peak()
                    .unwrap_or(0.0)
                    .partial_cmp(&a.peak().unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| position(a).cmp(&position(b)))
            .then_with(|| a.provider.slug().cmp(b.provider.slug()))
    });
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

    /// The Codex response carries more than the bar needs: balances, reset credits and
    /// per-model availability all become facts, with the panel-worthy ones marked.
    #[test]
    fn codex_response_keeps_the_extra_numbers() {
        let body = serde_json::json!({
            "email": "a@b.c",
            "plan_type": "pro",
            "rate_limit": {"primary_window": {
                "used_percent": 97, "limit_window_seconds": 604800, "reset_at": 1789805398}},
            "credits": {
                "has_credits": false, "balance": "0", "unlimited": true,
                "approx_local_messages": [0, 3], "approx_cloud_messages": [0, 0],
                "overage_limit_reached": true},
            "rate_limit_reset_credits": {"available_count": 2, "applicable_available_count": 1},
            "model_usage": {"gpt-6-astra": {"available": false, "available_at": null}}
        });
        let report = codex_from_wham(&body);
        assert_eq!(report.plan.as_deref(), Some("pro"));
        assert_eq!(report.windows.len(), 1);
        let fact = |label: &str| {
            report
                .facts
                .iter()
                .find(|f| f.label == label)
                .map(|f| f.value.clone())
        };
        assert_eq!(fact("banked resets").as_deref(), Some("2"));
        assert_eq!(fact("approx local messages").as_deref(), Some("0 / 3"));
        assert_eq!(fact("gpt-6-astra").as_deref(), Some("unavailable"));
        assert_eq!(fact("unlimited").as_deref(), Some("yes"));
        assert_eq!(fact("overage limit reached").as_deref(), Some("yes"));
        // The panel line stays short: only facts that earn it are printed.
        let panel: Vec<&Fact> = report.facts.iter().filter(|f| f.panel).collect();
        assert_eq!(panel.len(), 1);
        assert_eq!(panel[0].label, "banked resets");
    }

    #[test]
    fn smart_sort_is_worst_first_and_stable_on_the_account_list() {
        let accounts = vec![
            AccountRef::new("claude", ProviderId::Claude),
            AccountRef::new("codex", ProviderId::Codex),
            AccountRef::new("cursor", ProviderId::Cursor),
        ];
        let mut ok = Report::new(ProviderId::Claude).key("claude");
        ok.windows.push(Window::new("W", 10.0));
        let mut worse = Report::new(ProviderId::Codex).key("codex");
        worse.windows.push(Window::new("W", 90.0));
        let broken = Report::failed(ProviderId::Cursor, "HTTP 500".into()).key("cursor");
        let mut reports = vec![ok, broken, worse];
        sort_reports(&mut reports, &accounts, SortMode::Smart);
        assert_eq!(
            reports.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["codex", "claude", "cursor"]
        );

        let mut manual = vec![
            Report::new(ProviderId::Codex).key("codex"),
            Report::new(ProviderId::Claude).key("claude"),
        ];
        sort_reports(&mut manual, &accounts, SortMode::Manual);
        assert_eq!(manual[0].key, "codex");
    }
}
