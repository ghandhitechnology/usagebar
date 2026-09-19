//! The vocabulary every other module shares: providers, accounts, readings.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a number came from. Every value shown is vendor-reported; this says which
/// surface reported it so a wrong number can be traced back to its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Api,
    LocalFile,
}

impl Source {
    pub fn glyph(self) -> &'static str {
        match self {
            Source::Api => "api",
            Source::LocalFile => "local",
        }
    }
}

/// The vendors this tool knows how to read. The set is fixed at compile time; a
/// configured account always names one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProviderId {
    #[serde(rename = "claude")]
    Claude,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "opencode-go")]
    OpenCodeGo,
    #[serde(rename = "cursor")]
    Cursor,
    #[serde(rename = "grok")]
    Grok,
    #[serde(rename = "devin")]
    Devin,
    #[serde(rename = "command-code")]
    CommandCode,
}

impl ProviderId {
    pub const ALL: [ProviderId; 7] = [
        ProviderId::Claude,
        ProviderId::Codex,
        ProviderId::OpenCodeGo,
        ProviderId::Cursor,
        ProviderId::Grok,
        ProviderId::Devin,
        ProviderId::CommandCode,
    ];

    /// The stable name used in the config file and in account ids.
    pub fn slug(self) -> &'static str {
        match self {
            ProviderId::Claude => "claude",
            ProviderId::Codex => "codex",
            ProviderId::OpenCodeGo => "opencode-go",
            ProviderId::Cursor => "cursor",
            ProviderId::Grok => "grok",
            ProviderId::Devin => "devin",
            ProviderId::CommandCode => "command-code",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            ProviderId::Claude => "Claude",
            ProviderId::Codex => "Codex",
            ProviderId::OpenCodeGo => "OpenCode Go",
            ProviderId::Cursor => "Cursor",
            ProviderId::Grok => "Grok",
            ProviderId::Devin => "Devin",
            ProviderId::CommandCode => "Command Code",
        }
    }

    /// One line on how this provider is connected, shown while picking a provider.
    pub fn connect_hint(self) -> &'static str {
        match self {
            ProviderId::Claude => "OAuth pair from Claude Code, or an import of its credentials file",
            ProviderId::Codex => "tokens from the Codex CLI's auth.json",
            ProviderId::OpenCodeGo => "a Go API key from the OpenCode credential store",
            ProviderId::Cursor => "the access token from cursor-agent's auth.json",
            ProviderId::Grok => "the session key from the Grok CLI's auth.json",
            ProviderId::Devin => "the windsurf_api_key from Devin's credentials.toml",
            ProviderId::CommandCode => "the API key from the Command Code CLI's auth.json",
        }
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display())
    }
}

/// One entry in the display list. Accounts exist in the config; their secrets live in
/// the credential store under the same id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRef {
    pub id: String,
    pub provider: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl AccountRef {
    pub fn new(id: impl Into<String>, provider: ProviderId) -> Self {
        Self {
            id: id.into(),
            provider,
            label: None,
            hidden: false,
        }
    }

    /// What the panel title shows after the provider name.
    pub fn display_name(&self, vendor_account: Option<&str>) -> Option<String> {
        self.label
            .clone()
            .or_else(|| vendor_account.map(str::to_string))
    }
}

#[derive(Debug, Clone)]
pub struct Window {
    pub label: String,
    /// Percent used, 0-100, exactly as the vendor reports it. Never derived.
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    /// Verifiable extra from the same response, e.g. "$15.14 / $35 cap".
    pub detail: Option<String>,
}

impl Window {
    pub fn new(label: impl Into<String>, used_percent: f64) -> Self {
        Self {
            label: label.into(),
            used_percent,
            resets_at: None,
            detail: None,
        }
    }

    pub fn reset_at(mut self, at: Option<DateTime<Utc>>) -> Self {
        self.resets_at = at;
        self
    }

    pub fn detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }
}

/// A vendor number that is not a quota window: a balance, a reset credit count, a
/// per-model availability. Panels flatten these into one faint line; the detail view
/// gives each its own row.
#[derive(Debug, Clone)]
pub struct Fact {
    pub label: String,
    pub value: String,
    /// Whether the panel's one-line summary shows it. A quiet fact only appears in the
    /// detail view, so panels stay short while nothing is lost.
    pub panel: bool,
}

impl Fact {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            panel: true,
        }
    }

    pub fn quiet(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            panel: false,
            ..Self::new(label, value)
        }
    }
}

#[derive(Debug, Clone)]
pub enum Health {
    Ok,
    /// Responded, but the vendor did not report a quota for this account.
    NoQuota(String),
    /// Failed, and the windows on the report are the last good reading, not a fresh one.
    Stale {
        why: String,
        since: std::time::Instant,
    },
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct Report {
    /// The account this reading belongs to. Doubles as the history key, so two
    /// accounts of the same provider never share a sparkline.
    pub key: String,
    pub provider: ProviderId,
    /// Configured display label, when the account has one.
    pub label: Option<String>,
    pub account: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    /// Flat vendor numbers shown below the windows.
    pub facts: Vec<Fact>,
    /// Prose caveats, e.g. "from last local rollout".
    pub notes: Vec<String>,
    pub source: Source,
    pub health: Health,
}

impl Report {
    pub fn new(provider: ProviderId) -> Self {
        Self {
            key: provider.slug().to_string(),
            provider,
            label: None,
            account: None,
            plan: None,
            windows: Vec::new(),
            facts: Vec::new(),
            notes: Vec::new(),
            source: Source::Api,
            health: Health::Ok,
        }
    }

    pub fn failed(provider: ProviderId, why: String) -> Self {
        Self {
            health: Health::Unavailable(why),
            ..Self::new(provider)
        }
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    pub fn label(mut self, label: Option<String>) -> Self {
        self.label = label;
        self
    }

    pub fn account(mut self, account: Option<String>) -> Self {
        self.account = account;
        self
    }

    pub fn plan(mut self, plan: Option<String>) -> Self {
        self.plan = plan;
        self
    }

    pub fn source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }

    pub fn fact(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts.push(Fact::new(label, value));
        self
    }

    pub fn quiet_fact(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts.push(Fact::quiet(label, value));
        self
    }

    /// Highest used percent in this report, used for sorting and for the header summary.
    pub fn peak(&self) -> Option<f64> {
        self.windows
            .iter()
            .map(|w| w.used_percent)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
    }

    pub fn health_is_ok(&self) -> bool {
        matches!(self.health, Health::Ok)
    }
}

/// Version of the account label that is safe to render: emails get shortened so a
/// wide card does not turn into a wall of text.
pub fn short_account(label: &str) -> String {
    match label.split_once('@') {
        Some((name, domain)) if name.len() > 12 => format!("{}…@{}", &name[..11], domain),
        _ => label.to_string(),
    }
}
