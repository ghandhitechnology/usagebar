use chrono::{DateTime, Utc};

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

#[derive(Debug, Clone)]
pub enum Health {
    Ok,
    /// Responded, but the vendor did not report a quota for this account.
    NoQuota(String),
    /// Failed, and the windows on the report are the last good reading, not a fresh one.
    Stale { why: String, since: std::time::Instant },
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct Report {
    pub provider: &'static str,
    pub account: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    /// Balances and other flat vendor numbers shown below the windows.
    pub notes: Vec<String>,
    pub source: Source,
    pub health: Health,
}

impl Report {
    pub fn new(provider: &'static str) -> Self {
        Self {
            provider,
            account: None,
            plan: None,
            windows: Vec::new(),
            notes: Vec::new(),
            source: Source::Api,
            health: Health::Ok,
        }
    }

    pub fn failed(provider: &'static str, why: String) -> Self {
        Self {
            health: Health::Unavailable(why),
            ..Self::new(provider)
        }
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

    /// Highest used percent in this report, used for sorting and for the header summary.
    pub fn peak(&self) -> Option<f64> {
        self.windows
            .iter()
            .map(|w| w.used_percent)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
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
