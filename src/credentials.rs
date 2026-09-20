//! Where secrets live. Accounts in the config reference credentials by id; this module
//! reads vendor files, keeps usagebar's own copy in sync with them, and writes the
//! store with owner-only permissions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::model::{AccountRef, ProviderId};

/// The shapes of secret the adapters know how to use. One variant per shape, not one
/// per provider: a bearer-ish token covers five vendors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    ClaudeOauth {
        access_token: String,
        refresh_token: String,
        /// Milliseconds since the epoch, the way Claude Code stores it.
        #[serde(default)]
        expires_at: i64,
        /// Carried through so the panel can still show the plan badge.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_type: Option<String>,
    },
    CodexTokens {
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        /// Milliseconds since the epoch, when the vendor said so. Absent for a pair
        /// imported from the CLI, which refreshes its own.
        #[serde(default)]
        expires_at: i64,
    },
    Token {
        token: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCredential {
    pub secret: Credential,
    /// The vendor file this was imported from, if any. Kept so a rotated token can be
    /// written back and so a refresh made elsewhere is picked up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PathBuf>,
    pub updated_at: DateTime<Utc>,
}

impl StoredCredential {
    pub fn new(secret: Credential) -> Self {
        Self {
            secret,
            origin: None,
            updated_at: Utc::now(),
        }
    }

    pub fn with_origin(mut self, origin: Option<PathBuf>) -> Self {
        self.origin = origin;
        self
    }
}

pub trait CredentialStore: Send + Sync {
    fn get(&self, account_id: &str) -> Option<StoredCredential>;
    fn put(&self, account_id: &str, credential: StoredCredential) -> Result<(), String>;
    fn remove(&self, account_id: &str) -> Result<(), String>;
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreDoc {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    credentials: BTreeMap<String, StoredCredential>,
}

fn default_version() -> u32 {
    1
}

/// The on-disk store. Every write goes through the mutex and lands atomically with
/// mode 0600, so a crash or a concurrent fetch cannot tear the file.
pub struct FileStore {
    path: PathBuf,
    doc: Mutex<StoreDoc>,
}

impl FileStore {
    pub fn load(path: PathBuf) -> Self {
        let doc = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(doc) => doc,
                Err(e) => {
                    // Refusing to start empty would be worse, but overwriting a file we
                    // could not read would silently drop every other account's secret.
                    let aside = fsutil::set_aside(&path, "corrupt");
                    eprintln!(
                        "usagebar: {} did not parse ({e}); moved to {}",
                        path.display(),
                        aside
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "a backup".into())
                    );
                    StoreDoc::default()
                }
            },
            Err(_) => StoreDoc::default(),
        };
        if path.exists() && !fsutil::is_private(&path) {
            eprintln!(
                "usagebar: {} was readable by other users; setting mode 0600",
                path.display()
            );
            let _ = fsutil::set_mode(&path, 0o600);
        }
        Self {
            path,
            doc: Mutex::new(doc),
        }
    }

    /// Serialize and write while the caller still holds the lock, so two writers cannot
    /// land on disk out of order and drop one of the credentials.
    fn flush_locked(doc: &StoreDoc) -> Result<String, String> {
        let mut body = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
        body.push('\n');
        Ok(body)
    }
}

impl CredentialStore for FileStore {
    fn get(&self, account_id: &str) -> Option<StoredCredential> {
        self.doc
            .lock()
            .unwrap()
            .credentials
            .get(account_id)
            .cloned()
    }

    fn put(&self, account_id: &str, credential: StoredCredential) -> Result<(), String> {
        // The lock is held across the write: two accounts rotating in one poll must not
        // land on disk out of order, and the in-memory copy is what the next write reads.
        let mut doc = self.doc.lock().unwrap();
        doc.credentials.insert(account_id.to_string(), credential);
        let body = Self::flush_locked(&doc)?;
        fsutil::write_atomic(&self.path, body.as_bytes(), Some(0o600))
    }

    fn remove(&self, account_id: &str) -> Result<(), String> {
        let mut doc = self.doc.lock().unwrap();
        doc.credentials.remove(account_id);
        let body = Self::flush_locked(&doc)?;
        fsutil::write_atomic(&self.path, body.as_bytes(), Some(0o600))
    }
}

/// Used for detected accounts (which are not persisted unless imported) and for
/// verifying a credential before the user saves it.
#[derive(Default)]
pub struct MemoryStore {
    doc: Mutex<StoreDoc>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, account_id: &str) -> Option<StoredCredential> {
        self.doc
            .lock()
            .unwrap()
            .credentials
            .get(account_id)
            .cloned()
    }

    fn put(&self, account_id: &str, credential: StoredCredential) -> Result<(), String> {
        self.doc
            .lock()
            .unwrap()
            .credentials
            .insert(account_id.to_string(), credential);
        Ok(())
    }

    fn remove(&self, account_id: &str) -> Result<(), String> {
        self.doc.lock().unwrap().credentials.remove(account_id);
        Ok(())
    }
}

/// Where each vendor keeps its credentials. None means the secret is not a file
/// (OpenCode Go keys live in the OpenCode database).
pub fn vendor_file(provider: ProviderId) -> Option<PathBuf> {
    match provider {
        ProviderId::Claude => {
            Some(fsutil::dir_from("CLAUDE_CONFIG_DIR", ".claude").join(".credentials.json"))
        }
        ProviderId::Codex => Some(fsutil::dir_from("CODEX_HOME", ".codex").join("auth.json")),
        ProviderId::OpenCodeGo => None,
        ProviderId::Cursor => {
            Some(fsutil::dir_from("CURSOR_CONFIG_DIR", ".cursor").join("auth.json"))
        }
        ProviderId::Grok => Some(fsutil::dir_from("GROK_HOME", ".grok").join("auth.json")),
        ProviderId::Devin => Some(
            fsutil::xdg_dir("XDG_DATA_HOME", "devin", ".local/share/devin")
                .join("credentials.toml"),
        ),
        ProviderId::CommandCode => {
            Some(fsutil::dir_from("COMMANDCODE_HOME", ".commandcode").join("auth.json"))
        }
    }
}

/// Parse whatever a vendor file holds into a credential. Unknown or unreadable shapes
/// return None rather than guessing.
pub fn from_vendor_file(provider: ProviderId, path: &Path) -> Option<Credential> {
    if provider == ProviderId::Devin {
        let raw = std::fs::read_to_string(path).ok()?;
        let key = raw
            .lines()
            .find_map(|line| line.strip_prefix("windsurf_api_key"))
            .and_then(|rest| rest.split('"').nth(1))?;
        return Some(Credential::Token {
            token: key.to_string(),
        });
    }
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let string = |value: Option<&serde_json::Value>| {
        value
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    match provider {
        ProviderId::Claude => {
            let oauth = doc.get("claudeAiOauth")?;
            Some(Credential::ClaudeOauth {
                access_token: string(oauth.get("accessToken"))?,
                refresh_token: string(oauth.get("refreshToken")).unwrap_or_default(),
                expires_at: oauth
                    .get("expiresAt")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
                subscription_type: string(oauth.get("subscriptionType")),
            })
        }
        ProviderId::Codex => {
            let tokens = doc.get("tokens")?;
            Some(Credential::CodexTokens {
                access_token: string(tokens.get("access_token"))?,
                account_id: string(tokens.get("account_id")),
                refresh_token: string(tokens.get("refresh_token")),
                // The CLI keeps its own copy fresh; the file never says when it expires.
                expires_at: 0,
            })
        }
        ProviderId::Cursor => Some(Credential::Token {
            token: string(doc.get("accessToken"))?,
        }),
        ProviderId::Grok => {
            let key = doc
                .as_object()?
                .values()
                .find_map(|entry| entry.get("key").and_then(serde_json::Value::as_str))?;
            Some(Credential::Token {
                token: key.to_string(),
            })
        }
        ProviderId::CommandCode => {
            let key = string(doc.get("apiKey")).or_else(|| {
                doc.as_object()?.values().find_map(|entry| {
                    (entry.get("type").and_then(serde_json::Value::as_str) == Some("key"))
                        .then(|| string(entry.get("key")))
                        .flatten()
                })
            })?;
            Some(Credential::Token { token: key })
        }
        ProviderId::OpenCodeGo | ProviderId::Devin => None,
    }
}

/// True when a vendor file may hold something newer than our copy. Content decides;
/// this only rules out a file that is clearly older than what we wrote.
fn origin_may_win(origin: &Path, stored: &StoredCredential) -> bool {
    let Ok(modified) = std::fs::metadata(origin).and_then(|meta| meta.modified()) else {
        return false;
    };
    let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    let modified = DateTime::<Utc>::from_timestamp(since_epoch.as_secs() as i64, 0)
        .unwrap_or(stored.updated_at);
    // A file written at the same second as ours is the case that matters most: the
    // vendor just refreshed, and a strict comparison would keep the older copy.
    modified >= stored.updated_at - chrono::Duration::seconds(1)
}

/// The credential to use for an account right now. When the vendor file we imported
/// from has changed since we last wrote it, the file wins: whichever process
/// refreshed, usagebar follows.
pub fn current(
    account: &AccountRef,
    store: &dyn CredentialStore,
) -> Result<StoredCredential, String> {
    let mut stored = store.get(&account.id).ok_or_else(|| {
        format!(
            "no stored credentials; press s and connect {}",
            account.provider.display()
        )
    })?;
    if let Some(origin) = stored.origin.clone() {
        if origin_may_win(&origin, &stored) {
            if let Some(secret) = from_vendor_file(account.provider, &origin) {
                if secret != stored.secret {
                    stored.secret = secret;
                    stored.updated_at = Utc::now();
                    store.put(&account.id, stored.clone())?;
                }
            }
        }
    }
    Ok(stored)
}

/// Persist a freshly rotated secret and, for Claude, write it back to the vendor file
/// so Claude Code keeps working from the same login.
pub fn record(
    account: &AccountRef,
    store: &dyn CredentialStore,
    previous: &StoredCredential,
    secret: Credential,
) -> Result<StoredCredential, String> {
    let next = StoredCredential {
        secret,
        origin: previous.origin.clone(),
        updated_at: Utc::now(),
    };
    store.put(&account.id, next.clone())?;
    if let (Some(origin), Credential::ClaudeOauth { refresh_token, .. }) =
        (previous.origin.as_ref(), &previous.secret)
    {
        if let Err(why) = mirror_claude(origin, refresh_token, &next.secret) {
            // The next rotation tries again; the token we hold is still the one that works.
            eprintln!("usagebar: could not update {}: {why}", origin.display());
        }
    }
    Ok(next)
}

/// Update the vendor file with a rotated pair, but only when it still holds the pair
/// we rotated from. Another process may have refreshed first; its token wins.
fn mirror_claude(origin: &Path, expected_refresh: &str, secret: &Credential) -> Result<(), String> {
    let Credential::ClaudeOauth {
        access_token,
        refresh_token,
        expires_at,
        ..
    } = secret
    else {
        return Ok(());
    };
    let raw = std::fs::read_to_string(origin).map_err(|e| e.to_string())?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let stored_refresh = doc
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if stored_refresh != expected_refresh {
        return Err("another process refreshed first; leaving its token alone".into());
    }
    let slot = doc
        .pointer_mut("/claudeAiOauth")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or("credentials shape changed")?;
    slot.insert(
        "accessToken".into(),
        serde_json::Value::String(access_token.clone()),
    );
    slot.insert(
        "refreshToken".into(),
        serde_json::Value::String(refresh_token.clone()),
    );
    slot.insert(
        "expiresAt".into(),
        serde_json::Value::Number((*expires_at).into()),
    );
    let mut body = serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?;
    body.push(b'\n');
    fsutil::write_atomic(origin, &body, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AccountRef;

    #[test]
    fn claude_file_parses_and_masks() {
        let dir = std::env::temp_dir().join(format!("usagebar-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-abcdefgh","refreshToken":"r1","expiresAt":123}}"#,
        )
        .unwrap();
        let cred = from_vendor_file(ProviderId::Claude, &path).unwrap();
        match cred {
            Credential::ClaudeOauth {
                access_token,
                refresh_token,
                expires_at,
                ..
            } => {
                assert_eq!(access_token, "sk-ant-oat01-abcdefgh");
                assert_eq!(refresh_token, "r1");
                assert_eq!(expires_at, 123);
            }
            other => panic!("wrong shape: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_and_token_files_parse() {
        let dir = std::env::temp_dir().join(format!("usagebar-creds2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let codex = dir.join("auth.json");
        std::fs::write(
            &codex,
            r#"{"tokens":{"access_token":"at","refresh_token":"rt","account_id":"acc"}}"#,
        )
        .unwrap();
        assert_eq!(
            from_vendor_file(ProviderId::Codex, &codex).unwrap(),
            Credential::CodexTokens {
                access_token: "at".into(),
                account_id: Some("acc".into()),
                refresh_token: Some("rt".into()),
                expires_at: 0,
            }
        );
        let devin = dir.join("credentials.toml");
        std::fs::write(
            &devin,
            "windsurf_api_key = \"wsk-123\"\napi_server_url = \"x\"\n",
        )
        .unwrap();
        assert_eq!(
            from_vendor_file(ProviderId::Devin, &devin).unwrap(),
            Credential::Token {
                token: "wsk-123".into()
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A refresh written by the vendor CLI after ours must win, or the freshest login
    /// would be shadowed by a stale copy.
    #[test]
    fn newer_vendor_file_beats_the_stored_copy() {
        let dir = std::env::temp_dir().join(format!("usagebar-creds3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, r#"{"accessToken":"old"}"#).unwrap();
        let store = MemoryStore::new();
        let account = AccountRef::new("cursor", ProviderId::Cursor);
        let mut stored = StoredCredential::new(Credential::Token {
            token: "old".into(),
        })
        .with_origin(Some(path.clone()));
        stored.updated_at = Utc::now() - chrono::Duration::hours(1);
        store.put("cursor", stored).unwrap();

        std::fs::write(&path, r#"{"accessToken":"new"}"#).unwrap();
        let effective = current(&account, &store).unwrap();
        assert_eq!(
            effective.secret,
            Credential::Token {
                token: "new".into()
            }
        );
        // And the adoption is persisted, so the next read starts from the new value.
        assert_eq!(
            store.get("cursor").unwrap().secret,
            Credential::Token {
                token: "new".into()
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The vendor CLI can refresh in the same second we mirror a rotation; its copy
    /// still has to win, so the comparison cannot be a strict one.
    #[test]
    fn same_second_vendor_write_is_adopted() {
        let dir = std::env::temp_dir().join(format!("usagebar-creds5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, r#"{"accessToken":"ours"}"#).unwrap();
        let store = MemoryStore::new();
        let account = AccountRef::new("cursor", ProviderId::Cursor);
        store
            .put(
                "cursor",
                StoredCredential::new(Credential::Token {
                    token: "ours".into(),
                })
                .with_origin(Some(path.clone())),
            )
            .unwrap();
        std::fs::write(&path, r#"{"accessToken":"theirs"}"#).unwrap();
        let effective = current(&account, &store).unwrap();
        assert_eq!(
            effective.secret,
            Credential::Token {
                token: "theirs".into()
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn files_are_written_owner_only() {
        let dir = std::env::temp_dir().join(format!("usagebar-creds4-{}", std::process::id()));
        let path = dir.join("credentials.json");
        let store = FileStore::load(path.clone());
        store
            .put(
                "claude",
                StoredCredential::new(Credential::Token { token: "t".into() }),
            )
            .unwrap();
        assert!(fsutil::is_private(&path));
        let reloaded = FileStore::load(path.clone());
        assert!(reloaded.get("claude").is_some());
        store.remove("claude").unwrap();
        assert!(FileStore::load(path).get("claude").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two fetch threads can rotate two accounts at once; neither write may be lost.
    #[test]
    fn concurrent_writes_keep_every_credential() {
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("usagebar-creds6-{}", std::process::id()));
        let path = dir.join("credentials.json");
        let store = Arc::new(FileStore::load(path.clone()));
        let mut handles = Vec::new();
        for n in 0..8 {
            let store = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                store
                    .put(
                        &format!("account-{n}"),
                        StoredCredential::new(Credential::Token {
                            token: format!("token-{n}"),
                        }),
                    )
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let reloaded = FileStore::load(path);
        for n in 0..8 {
            assert!(
                reloaded.get(&format!("account-{n}")).is_some(),
                "account-{n} was dropped by a concurrent write"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unreadable store is moved aside rather than silently replaced by an empty one.
    #[test]
    fn corrupt_store_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("usagebar-creds7-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(&path, "{ not json").unwrap();
        let store = FileStore::load(path.clone());
        assert!(store.get("anything").is_none());
        assert!(!path.exists(), "the unreadable file was left in place");
        assert!(dir.join("credentials.corrupt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
