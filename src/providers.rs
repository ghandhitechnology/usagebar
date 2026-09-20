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
use crate::oauth;

const UA: &str = concat!("usagebar/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(25);
const MAX_FETCH_WORKERS: usize = 4;

pub type Result<T> = std::result::Result<T, String>;

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<(u16, Value)> {
    let mut req = ureq::get(url)
        .config()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
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
    let body = match res.body_mut().read_json::<Value>() {
        Ok(body) => body,
        Err(_) if status >= 400 => Value::Null,
        Err(e) => return Err(format!("{url}: unreadable response: {e}")),
    };
    Ok((status, body))
}

fn post_json(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<(u16, Value)> {
    let mut req = ureq::post(url)
        .config()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
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

/// Some vendors only accept the OAuth form encoding, so the sign-in's token requests go
/// through here rather than as JSON.
fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", urlencode(key), urlencode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A token POST that keeps the vendor's refusal. ureq folds a non-2xx into an error and
/// the body goes with it, but a sign-in is where the vendor explains itself — "could not
/// validate your token", "the code expired" — and that is what the user needs to read.
fn post_token(url: &str, headers: &[(&str, &str)], body: &str) -> Result<(u16, Value)> {
    let mut req = ureq::post(url)
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .header("User-Agent", UA)
        .header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut res = req
        .send(body)
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

fn require_success(status: u16) -> Result<()> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("request failed (HTTP {status})"))
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
pub(crate) fn opencode_db() -> PathBuf {
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
/// Claude's redirect is a hosted page that shows the code for the user to carry back:
/// the registered client has no loopback URL, so there is nothing here to listen on.
const CLAUDE_OAUTH_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Codex's redirect is a loopback URL, and the client only redirects to the exact one it
/// was registered with, port included.
const CODEX_OAUTH_PORT: u16 = 1455;

/// The browser sign-in a provider offers, with the same client its own CLI signs in
/// with. Both are taken from the installed CLIs: a client id or redirect this file
/// invented would be refused in the browser, where nothing can be done about it.
pub fn oauth_spec(provider: ProviderId) -> Option<oauth::Spec> {
    match provider {
        ProviderId::Codex => Some(oauth::Spec {
            authorize: "https://auth.openai.com/oauth/authorize",
            token: "https://auth.openai.com/oauth/token",
            client_id: CODEX_OAUTH_CLIENT_ID,
            scopes: "openid profile email offline_access",
            redirect: "http://localhost:1455/auth/callback",
            extra: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "codex_cli_rs"),
            ],
            callback_port: Some(CODEX_OAUTH_PORT),
        }),
        ProviderId::Claude => Some(oauth::Spec {
            authorize: "https://platform.claude.com/oauth/authorize",
            // The endpoint this tool already rotates Claude tokens against.
            token: "https://console.anthropic.com/v1/oauth/token",
            client_id: CLAUDE_OAUTH_CLIENT_ID,
            scopes: "org:create_api_key user:profile user:inference",
            redirect: CLAUDE_OAUTH_REDIRECT,
            extra: &[],
            callback_port: None,
        }),
        _ => None,
    }
}

/// Trade an authorization code for the credential usagebar stores. The two vendors
/// differ: Anthropic takes JSON and names the subscription in the response, while
/// OpenAI takes a form and hides the account id inside the id_token.
pub fn oauth_exchange(provider: ProviderId, code: &str, verifier: &str) -> Result<Credential> {
    let spec = oauth_spec(provider).ok_or("this provider has no browser sign-in")?;
    let (status, body) = match provider {
        ProviderId::Claude => post_token(
            spec.token,
            &[
                ("Content-Type", "application/json"),
                ("anthropic-beta", "oauth-2025-04-20"),
            ],
            &serde_json::json!({
                "grant_type": "authorization_code",
                "code": code,
                "redirect_uri": spec.redirect,
                "client_id": spec.client_id,
                "code_verifier": verifier,
            })
            .to_string(),
        )?,
        _ => post_token(
            spec.token,
            &[("Content-Type", "application/x-www-form-urlencoded")],
            &form_body(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", spec.redirect),
                ("client_id", spec.client_id),
                ("code_verifier", verifier),
            ]),
        )?,
    };

    let access_token = match s(&body, "access_token") {
        Some(token) => token,
        None => return Err(refused(status, &body)),
    };
    let refresh_token = s(&body, "refresh_token").unwrap_or_default();
    let expires_at = Utc::now().timestamp_millis() + expires_in_ms(&body);
    Ok(match provider {
        ProviderId::Claude => Credential::ClaudeOauth {
            access_token,
            refresh_token,
            expires_at,
            // Claude Code reads the plan off the token response, so a signed-in account
            // can show it as readily as an imported one.
            subscription_type: s(&body, "subscriptionType"),
        },
        _ => Credential::CodexTokens {
            access_token,
            account_id: body
                .get("id_token")
                .and_then(Value::as_str)
                .and_then(codex_account_id),
            refresh_token: (!refresh_token.is_empty()).then_some(refresh_token),
            expires_at,
        },
    })
}

/// The account a Codex token set belongs to, which every usage call has to name. The
/// claim key is a URL, so it is read by name rather than with a JSON pointer, which
/// would take its slashes for path separators.
fn codex_account_id(id_token: &str) -> Option<String> {
    jwt_payload(id_token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

fn expires_in_ms(body: &Value) -> i64 {
    // A floor of a minute, so a vendor that reports a token as already gone still leaves
    // one round trip to notice, rather than a refresh on every poll.
    (f(body, "expires_in").unwrap_or(3600.0) as i64).max(60) * 1000
}

/// A token response with no token in it: the vendor's own words when it has any.
fn refused(status: u16, body: &Value) -> String {
    format!("the sign-in was refused: {}", refusal_reason(status, body))
}

/// Why a token response carried no tokens, as the vendor put it.
fn refusal_reason(status: u16, body: &Value) -> String {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| s(body, "error_description"))
        .or_else(|| s(body, "error"))
        .unwrap_or_else(|| format!("HTTP {status}"))
}

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
    require_success(status)?;

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
    let (status, body) = post_token(
        "https://console.anthropic.com/v1/oauth/token",
        &[
            ("Content-Type", "application/json"),
            ("anthropic-beta", "oauth-2025-04-20"),
        ],
        &serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": client_id,
        })
        .to_string(),
    )?;
    let Some(access) = body.get("access_token").and_then(Value::as_str) else {
        // The vendor says why when it will, and the status is all there is otherwise.
        return Err(format!(
            "refresh failed ({}); run any Claude Code command once",
            refusal_reason(status, &body)
        ));
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

/// Keep a signed-in Codex account alive. Only the credentials this tool signed in for are
/// refreshed: a token imported from the CLI's file belongs to the CLI, which rotates its
/// own, and spending its refresh token here would sign the user out of codex.
fn rotate_codex(
    account: &AccountRef,
    store: &dyn CredentialStore,
    stored: &StoredCredential,
) -> Result<StoredCredential> {
    let Credential::CodexTokens {
        refresh_token,
        account_id,
        expires_at,
        ..
    } = &stored.secret
    else {
        return Err("stored credential is not a Codex token set".into());
    };
    let now = Utc::now().timestamp_millis();
    if *expires_at > now + 60_000 {
        return Ok(stored.clone());
    }
    // No expiry on record means the vendor never gave one, so this is not a sign-in of
    // ours: a pasted token lives as long as it lives, and there is nothing to refresh it
    // with either.
    if *expires_at == 0 {
        return Ok(stored.clone());
    }
    let Some(refresh) = refresh_token.as_deref().filter(|token| !token.is_empty()) else {
        // A sign-in that carried no refresh token has run out: say so rather than
        // reporting the same expired token as a network failure.
        return Err("the signed-in Codex token expired; sign in again from setup".into());
    };
    let spec = oauth_spec(ProviderId::Codex).ok_or("no Codex sign-in is known")?;
    let (status, body) = post_token(
        spec.token,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        &form_body(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", spec.client_id),
            ("scope", "openid profile email"),
        ]),
    )?;
    let Some(access) = body.get("access_token").and_then(Value::as_str) else {
        return Err(refused(status, &body));
    };
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or(refresh);
    credentials::record(
        account,
        store,
        stored,
        Credential::CodexTokens {
            access_token: access.to_string(),
            // A refresh answer carries a fresh id_token, and with it the account.
            account_id: body
                .get("id_token")
                .and_then(Value::as_str)
                .and_then(codex_account_id)
                .or_else(|| account_id.clone()),
            refresh_token: Some(refresh.to_string()),
            expires_at: now + expires_in_ms(&body),
        },
    )
}

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
            let fallback = origin.and_then(Path::parent).map(codex_rollout).transpose();
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
        return Err(
            "token rejected by chatgpt.com; run codex once, usagebar will pick it up".into(),
        );
    }
    require_success(status)?;
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
                report.facts.push(Fact::quiet(label, numbers.join(" / ")));
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
            report
                .facts
                .push(Fact::new("banked resets", count.to_string()));
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
    let age = std::fs::metadata(&newest)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .unwrap_or_default();
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

    let now = std::time::Instant::now();
    let mut report = Report::new(ProviderId::Codex).source(Source::LocalFile);
    report.health = Health::Stale {
        why: "live usage unavailable; showing the cached local reading".into(),
        since: now.checked_sub(age).unwrap_or(now),
    };
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
    report.notes.push("cached local Codex reading".into());
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

/// Keys owned by OpenCode's own integration. Other providers use the same table,
/// and their secrets must never be sent to the Go endpoint.
fn opencode_keys(db: &Path) -> Result<Vec<(String, String)>> {
    let connection = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("{}: {e}", db.display()))?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|e| format!("{}: {e}", db.display()))?;

    let columns = {
        let mut statement = connection
            .prepare("pragma table_info(credential)")
            .map_err(|e| format!("{}: {e}", db.display()))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| format!("{}: {e}", db.display()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("{}: {e}", db.display()))?
    };
    if !columns.iter().any(|column| column == "value") {
        return Err("OpenCode credential table has no value column".into());
    }

    // OpenCode renamed connector_id to integration_id. The final branch supports
    // the short-lived schema where the provider name itself was the row ID.
    let query = if columns.iter().any(|column| column == "integration_id") {
        "select id, value from credential where integration_id = ?1"
    } else if columns.iter().any(|column| column == "connector_id") {
        "select id, value from credential where connector_id = ?1"
    } else if columns.iter().any(|column| column == "id") {
        "select id, value from credential where id in (?1, 'opencode-go')"
    } else {
        return Err("OpenCode credential table has no ownership column".into());
    };
    let mut statement = connection
        .prepare(query)
        .map_err(|e| format!("{}: {e}", db.display()))?;
    let raw = statement
        .query_map(["opencode"], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| format!("{}: {e}", db.display()))?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()
        .map_err(|e| format!("{}: {e}", db.display()))?;

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
    require_success(status)?;

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
    jwt_payload(token)?.get(claim)?.as_str().map(str::to_string)
}

/// The claims of a JWT, without checking its signature: these are tokens the vendor just
/// handed us over TLS, read for the account they name rather than trusted for anything.
fn jwt_payload(token: &str) -> Option<Value> {
    let bytes = base64url_decode(token.split('.').nth(1)?)?;
    serde_json::from_slice(&bytes).ok()
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

/// Percent-encode everything outside the unreserved set. Shared with the sign-in, whose
/// scopes and redirects carry characters a query string cannot.
pub(crate) fn urlencode(input: &str) -> String {
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
    require_success(status)?;

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

    let mut report = Report::new(ProviderId::Devin)
        .account(s(status_obj, "email"))
        .plan(
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
    require_success(status)?;

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
    let stored = if mode == Mode::Poll {
        // A credential the CLI owns is the CLI's to rotate; one this tool signed in for
        // is ours to keep alive.
        match account.provider {
            ProviderId::Claude => match rotate_claude(account, store, &stored) {
                Ok(stored) => stored,
                Err(why) => return failed(why),
            },
            ProviderId::Codex if stored.origin.is_none() => {
                match rotate_codex(account, store, &stored) {
                    Ok(stored) => stored,
                    Err(why) => return failed(why),
                }
            }
            _ => stored,
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
        let mut account =
            AccountRef::new(next_account_id(&accounts, entry.provider), entry.provider);
        let Some(credential) = &entry.credential else {
            // A file that exists but holds nothing usable still deserves a panel that
            // says so, instead of the provider quietly disappearing.
            accounts.push(
                account.problem(
                    entry
                        .error
                        .clone()
                        .unwrap_or_else(|| "no usable credentials".into()),
                ),
            );
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
    fetch_all_progressive(accounts, store, sort, |_| {})
}

/// Fetch concurrently and publish sorted snapshots as each account finishes. The
/// final return value is the complete batch used by non-interactive commands.
pub fn fetch_all_progressive(
    accounts: &[AccountRef],
    store: &Arc<dyn CredentialStore>,
    sort: SortMode,
    mut progress: impl FnMut(&[Report]),
) -> Vec<Report> {
    let visible: Vec<AccountRef> = accounts
        .iter()
        .filter(|account| !account.hidden)
        .cloned()
        .collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let queue = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
        visible.clone(),
    )));
    let handles: Vec<_> = (0..visible.len().min(MAX_FETCH_WORKERS))
        .map(|_| {
            let queue = Arc::clone(&queue);
            let store = Arc::clone(store);
            let tx = tx.clone();
            std::thread::spawn(move || loop {
                let Some(account) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fetch_one(&account, store.as_ref())
                }))
                .unwrap_or_else(|_| {
                    Report::failed(account.provider, "adapter panicked".into())
                        .key(account.id.clone())
                        .label(account.label.clone())
                });
                if tx.send(report).is_err() {
                    break;
                }
            })
        })
        .collect();
    drop(tx);

    let mut reports = Vec::with_capacity(visible.len());
    for report in rx {
        reports.push(report);
        sort_reports(&mut reports, accounts, sort);
        progress(&reports);
    }
    for handle in handles {
        let _ = handle.join();
    }
    reports
}

/// Check a credential before the user commits to it. One live call, and the vendor's
/// own answer decides whether the account is usable. Nothing is written: a check must
/// not rotate a token or touch a vendor file, because the user may walk away from it.
pub fn verify(
    provider: ProviderId,
    credential: &Credential,
    origin: Option<&Path>,
) -> Result<Report> {
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

    fn temporary_db(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "usagebar-{name}-{}-{unique}.db",
            std::process::id()
        ))
    }

    #[test]
    fn opencode_keys_only_returns_owned_key_credentials() {
        let path = temporary_db("opencode-credentials");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            r#"
            create table credential (
                id text primary key,
                integration_id text,
                label text not null,
                value text not null
            );
            insert into credential values
                ('cred_go', 'opencode', 'Go', '{"type":"key","key":"go-secret"}'),
                ('cred_other', 'anthropic', 'Claude', '{"type":"key","key":"other-secret"}'),
                ('cred_oauth', 'opencode', 'OAuth', '{"type":"oauth","access":"access-secret"}'),
                ('cred_bad', 'opencode', 'Broken', 'not json');
            "#,
        )
        .unwrap();
        drop(db);

        let keys = opencode_keys(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(keys, vec![("cred_go".into(), "go-secret".into())]);
    }

    #[test]
    fn get_json_returns_http_errors_to_the_adapter() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 256];
            loop {
                let read = socket.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..read]);
                if read == 0 || request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            let response = b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"error\":\"denied\"}";
            socket.write_all(response).unwrap();
        });

        let (status, body) = get_json(&format!("http://{address}"), &[]).unwrap();
        server.join().unwrap();

        assert_eq!(status, 401);
        assert_eq!(body["error"], "denied");
    }

    #[test]
    fn codex_local_fallback_is_marked_stale() {
        let root = temporary_db("codex-fallback").with_extension("");
        let sessions = root.join("sessions/2026/09/20");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout.jsonl"),
            r#"{"payload":{"rate_limits":{"primary":{"used_percent":42,"window_minutes":300}}}}"#,
        )
        .unwrap();

        let report = codex_rollout(&root).unwrap();
        std::fs::remove_dir_all(root).unwrap();

        assert_eq!(report.source, Source::LocalFile);
        assert_eq!(report.windows[0].used_percent, 42.0);
        assert!(matches!(report.health, Health::Stale { .. }));
    }

    #[test]
    fn progressive_fetch_publishes_each_finished_account() {
        let accounts = vec![
            AccountRef::new("claude", ProviderId::Claude).problem("offline"),
            AccountRef::new("codex", ProviderId::Codex).problem("offline"),
        ];
        let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::new());
        let mut counts = Vec::new();

        let reports = fetch_all_progressive(&accounts, &store, SortMode::Manual, |reports| {
            counts.push(reports.len());
        });

        assert_eq!(counts, vec![1, 2]);
        assert_eq!(reports.len(), 2);
    }

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

    /// The sign-in is only as good as these two constants: a client id or redirect this
    /// file invented would be refused in the browser, where nothing can be done about it.
    #[test]
    fn the_sign_in_specs_match_the_clients_they_belong_to() {
        let codex = oauth_spec(ProviderId::Codex).unwrap();
        assert_eq!(codex.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(codex.redirect, "http://localhost:1455/auth/callback");
        assert_eq!(codex.callback_port, Some(1455));
        assert!(
            codex.scopes.contains("offline_access"),
            "a refresh needs it"
        );
        assert!(codex
            .extra
            .iter()
            .any(|(k, _)| *k == "codex_cli_simplified_flow"));

        let claude = oauth_spec(ProviderId::Claude).unwrap();
        assert_eq!(claude.client_id, CLAUDE_OAUTH_CLIENT_ID);
        assert_eq!(claude.redirect, CLAUDE_OAUTH_REDIRECT);
        // Claude's page shows the code instead of calling back, so nothing listens.
        assert_eq!(claude.callback_port, None);
        // The refresh endpoint the rest of this file already uses, so a signed-in pair
        // and an imported one rotate the same way.
        assert_eq!(claude.token, "https://console.anthropic.com/v1/oauth/token");

        // The providers that have no sign-in say so rather than offering a dead key.
        assert!(oauth_spec(ProviderId::Cursor).is_none());
        assert!(oauth_spec(ProviderId::Grok).is_none());
    }

    /// A signed-in Codex pair names its account in the id_token, which is the only place
    /// the vendor puts it, and which every usage call has to send back.
    #[test]
    fn the_codex_account_comes_out_of_the_id_token() {
        let claims = serde_json::json!({
            "sub": "user-1",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc_9", "chatgpt_plan_type": "plus"}
        });
        let payload = oauth::base64url(&serde_json::to_vec(&claims).unwrap());
        assert_eq!(
            codex_account_id(&format!("header.{payload}.sig")).as_deref(),
            Some("acc_9")
        );
        // A token without the claim, or without a payload at all, names no account.
        assert!(codex_account_id("header.e30.sig").is_none());
        assert!(codex_account_id("not-a-jwt").is_none());
    }

    /// A token response with no token in it reports the vendor's reason, not a panic or
    /// a blank error.
    #[test]
    fn a_refused_exchange_keeps_the_vendors_words() {
        assert_eq!(
            refused(
                400,
                &serde_json::json!({"error": {"type": "invalid_grant", "message": "code expired"}})
            ),
            "the sign-in was refused: code expired"
        );
        assert_eq!(
            refused(400, &serde_json::json!({"error": "invalid_client"})),
            "the sign-in was refused: invalid_client"
        );
        assert_eq!(
            refused(500, &serde_json::json!({})),
            "the sign-in was refused: HTTP 500"
        );
        // The refresh path says the same thing about the same answer.
        assert_eq!(
            refusal_reason(
                401,
                &serde_json::json!({"error": {"message": "token expired"}})
            ),
            "token expired"
        );
        assert_eq!(refusal_reason(429, &serde_json::json!({})), "HTTP 429");
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

    /// Only a credential the vendor gave an expiry for is refreshed, and only a sign-in
    /// of ours is ever spent on a refresh token.
    #[test]
    fn only_a_signed_in_codex_pair_is_refreshed() {
        let account = AccountRef::new("codex", ProviderId::Codex);
        let store = MemoryStore::new();

        // A pasted token: usable, with no expiry to judge and no refresh token to spend.
        let pasted = StoredCredential::new(Credential::CodexTokens {
            access_token: "pasted".into(),
            account_id: None,
            refresh_token: None,
            expires_at: 0,
        });
        store.put("codex", pasted.clone()).unwrap();
        assert_eq!(
            rotate_codex(&account, &store, &pasted).unwrap().secret,
            pasted.secret,
            "a pasted token must be left alone"
        );

        // A sign-in that still has time on it is left alone too.
        let fresh = StoredCredential::new(Credential::CodexTokens {
            access_token: "fresh".into(),
            account_id: Some("acc".into()),
            refresh_token: Some("rt".into()),
            expires_at: Utc::now().timestamp_millis() + 3_600_000,
        });
        store.put("codex", fresh.clone()).unwrap();
        assert_eq!(
            rotate_codex(&account, &store, &fresh).unwrap().secret,
            fresh.secret
        );

        // An expired sign-in with no refresh token says what to do about it, rather than
        // failing the account for good with no explanation.
        let spent = StoredCredential::new(Credential::CodexTokens {
            access_token: "spent".into(),
            account_id: None,
            refresh_token: None,
            expires_at: Utc::now().timestamp_millis() - 1000,
        });
        assert!(rotate_codex(&account, &store, &spent)
            .unwrap_err()
            .contains("sign in again"));
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
