//! Provider API keys. A key lives only in the OS keyring (Secret Service) or in its env var,
//! which wins. Keys never go to a DB, a log line, an event or an IPC response: callers get
//! where a key comes from, never the key, except through `Credentials::api_key`.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// The keyring service every entry is saved under.
pub const KEYRING_SERVICE: &str = "com.opendam.desktop";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    Jev,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Anthropic, Provider::Jev];

    pub fn env_var(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::Jev => "TYPESAFE_API_KEY",
        }
    }

    pub fn keyring_entry(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic-api-key",
            Provider::Jev => "typesafe-api-key",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic",
            Provider::Jev => "Jev",
        }
    }
}

/// Where secrets are saved: the OS keyring, or a stand-in for tests and `OPENDAM_NO_KEYRING`.
pub trait SecretStore: Send + Sync {
    /// The saved secret, or `None` when there is no entry.
    fn get(&self, entry: &str) -> Result<Option<String>, String>;
    fn set(&self, entry: &str, secret: &str) -> Result<(), String>;
    /// Removes the entry. Removing a missing entry is not an error.
    fn delete(&self, entry: &str) -> Result<(), String>;
    /// Why secrets can't be saved here, when they can't.
    fn unavailable_reason(&self) -> Option<String> {
        None
    }
}

/// The OS keyring (Secret Service). Every call is a blocking D-Bus round trip: keep it off
/// async workers.
struct OsKeyring;

impl OsKeyring {
    fn entry(entry: &str) -> Result<keyring_core::Entry, String> {
        static STORE: OnceLock<Result<(), String>> = OnceLock::new();
        STORE
            .get_or_init(|| {
                let store = zbus_secret_service_keyring_store::Store::new()
                    .map_err(|e| format!("Keyring unavailable: {e}"))?;
                keyring_core::set_default_store(store);
                Ok(())
            })
            .clone()?;
        keyring_core::Entry::new(KEYRING_SERVICE, entry)
            .map_err(|e| format!("Keyring unavailable: {e}"))
    }
}

impl SecretStore for OsKeyring {
    fn get(&self, entry: &str) -> Result<Option<String>, String> {
        match Self::entry(entry)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("Could not read the keyring: {e}")),
        }
    }

    fn set(&self, entry: &str, secret: &str) -> Result<(), String> {
        Self::entry(entry)?
            .set_password(secret)
            .map_err(|e| format!("Could not save to the keyring: {e}"))
    }

    fn delete(&self, entry: &str) -> Result<(), String> {
        match Self::entry(entry)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("Could not clear the keyring entry: {e}")),
        }
    }
}

/// Debug builds with `OPENDAM_NO_KEYRING=1`: nothing is saved, so the app shows the
/// unconfigured state even on a machine with saved keys.
#[cfg(debug_assertions)]
struct DisabledKeyring;

#[cfg(debug_assertions)]
impl DisabledKeyring {
    const REASON: &'static str = "The keyring is ignored (OPENDAM_NO_KEYRING=1)";
}

#[cfg(debug_assertions)]
impl SecretStore for DisabledKeyring {
    fn get(&self, _entry: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn set(&self, _entry: &str, _secret: &str) -> Result<(), String> {
        Err(Self::REASON.into())
    }

    fn delete(&self, _entry: &str) -> Result<(), String> {
        Ok(())
    }

    fn unavailable_reason(&self) -> Option<String> {
        Some(Self::REASON.into())
    }
}

/// An in-memory keyring, for tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryKeyring(std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl SecretStore for MemoryKeyring {
    fn get(&self, entry: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap().get(entry).cloned())
    }

    fn set(&self, entry: &str, secret: &str) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .insert(entry.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, entry: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(entry);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeySource {
    Env,
    Keyring,
    None,
}

/// What Settings shows for a provider. It never carries the key.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    pub provider: Provider,
    pub configured: bool,
    pub key_source: KeySource,
    /// "from ANTHROPIC_API_KEY", "saved in keyring" or "not set".
    pub source_label: String,
    pub env_var: &'static str,
    /// A key is saved in the keyring, even if the env var overrides it.
    pub saved_in_keyring: bool,
    /// Why the keyring can't be read or written, when it can't.
    pub keyring_problem: Option<String>,
}

type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct Credentials {
    store: Box<dyn SecretStore>,
    env: EnvLookup,
}

impl Credentials {
    /// The process environment and the OS keyring, or no keyring in a debug build run with
    /// `OPENDAM_NO_KEYRING=1`.
    pub fn from_process_env() -> Self {
        Self::with_env(Box::new(|name| std::env::var(name).ok()))
    }

    fn with_env(env: EnvLookup) -> Self {
        #[cfg(debug_assertions)]
        if env("OPENDAM_NO_KEYRING").as_deref() == Some("1") {
            tracing::info!("OPENDAM_NO_KEYRING=1: ignoring the OS keyring");
            return Self {
                store: Box::new(DisabledKeyring),
                env,
            };
        }
        Self {
            store: Box::new(OsKeyring),
            env,
        }
    }

    #[cfg(test)]
    pub(crate) fn new(
        store: impl SecretStore + 'static,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            store: Box::new(store),
            env: Box::new(env),
        }
    }

    fn env_key(&self, provider: Provider) -> Option<String> {
        (self.env)(provider.env_var()).filter(|key| !key.trim().is_empty())
    }

    /// The key requests use: the env var if set, else the keyring. Never log or return it.
    pub fn api_key(&self, provider: Provider) -> Result<Option<String>, String> {
        match self.env_key(provider) {
            Some(key) => Ok(Some(key)),
            None => self.store.get(provider.keyring_entry()),
        }
    }

    pub fn status(&self, provider: Provider) -> ProviderStatus {
        let from_env = self.env_key(provider).is_some();
        let (saved_in_keyring, read_error) = match self.store.get(provider.keyring_entry()) {
            Ok(saved) => (saved.is_some(), None),
            Err(e) => (false, Some(e)),
        };
        let (key_source, source_label) = if from_env {
            (KeySource::Env, format!("from {}", provider.env_var()))
        } else if saved_in_keyring {
            (KeySource::Keyring, "saved in keyring".to_string())
        } else {
            (KeySource::None, "not set".to_string())
        };
        ProviderStatus {
            provider,
            configured: key_source != KeySource::None,
            key_source,
            source_label,
            env_var: provider.env_var(),
            saved_in_keyring,
            keyring_problem: read_error.or_else(|| self.store.unavailable_reason()),
        }
    }

    pub fn statuses(&self) -> Vec<ProviderStatus> {
        Provider::ALL.into_iter().map(|p| self.status(p)).collect()
    }

    /// Saves `key` in the keyring, or clears the saved key when `key` is `None`.
    pub fn set_key(&self, provider: Provider, key: Option<&str>) -> Result<ProviderStatus, String> {
        let entry = provider.keyring_entry();
        match key.map(str::trim) {
            Some("") => return Err("The API key is empty".into()),
            Some(key) => {
                self.store.set(entry, key)?;
                tracing::info!("Saved the {} API key in the keyring", provider.label());
            }
            None => {
                self.store.delete(entry)?;
                tracing::info!("Cleared the {} API key from the keyring", provider.label());
            }
        }
        Ok(self.status(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn the_env_var_overrides_a_saved_key() {
        let creds = Credentials::new(
            MemoryKeyring::default(),
            env_with(&[("ANTHROPIC_API_KEY", "from-env")]),
        );
        creds
            .set_key(Provider::Anthropic, Some("from-keyring"))
            .unwrap();

        assert_eq!(
            creds.api_key(Provider::Anthropic).unwrap().as_deref(),
            Some("from-env")
        );
        let status = creds.status(Provider::Anthropic);
        assert!(status.configured);
        assert_eq!(status.key_source, KeySource::Env);
        assert_eq!(status.source_label, "from ANTHROPIC_API_KEY");
        assert!(status.saved_in_keyring);
    }

    #[test]
    fn without_the_env_var_the_saved_key_is_used() {
        let creds = Credentials::new(MemoryKeyring::default(), env_with(&[]));
        creds.set_key(Provider::Jev, Some("  jev-key \n")).unwrap();

        assert_eq!(
            creds.api_key(Provider::Jev).unwrap().as_deref(),
            Some("jev-key")
        );
        let status = creds.status(Provider::Jev);
        assert!(status.configured);
        assert_eq!(status.key_source, KeySource::Keyring);
        assert_eq!(status.source_label, "saved in keyring");
        assert_eq!(status.env_var, "TYPESAFE_API_KEY");
    }

    #[test]
    fn keys_are_saved_under_the_plan_service_entries() {
        let creds = Credentials::new(MemoryKeyring::default(), env_with(&[]));
        creds.set_key(Provider::Anthropic, Some("a")).unwrap();
        creds.set_key(Provider::Jev, Some("j")).unwrap();

        assert_eq!(KEYRING_SERVICE, "com.opendam.desktop");
        assert_eq!(
            creds.store.get("anthropic-api-key").unwrap().as_deref(),
            Some("a")
        );
        assert_eq!(
            creds.store.get("typesafe-api-key").unwrap().as_deref(),
            Some("j")
        );
    }

    #[test]
    fn clear_removes_the_saved_key() {
        let creds = Credentials::new(MemoryKeyring::default(), env_with(&[]));
        creds.set_key(Provider::Anthropic, Some("key")).unwrap();

        let status = creds.set_key(Provider::Anthropic, None).unwrap();

        assert!(!status.configured);
        assert_eq!(status.key_source, KeySource::None);
        assert_eq!(status.source_label, "not set");
        assert_eq!(creds.api_key(Provider::Anthropic).unwrap(), None);
        // Clearing again is fine.
        creds.set_key(Provider::Anthropic, None).unwrap();
    }

    #[test]
    fn an_empty_key_or_env_var_does_not_count() {
        let creds = Credentials::new(
            MemoryKeyring::default(),
            env_with(&[("TYPESAFE_API_KEY", "  ")]),
        );

        assert!(creds.set_key(Provider::Jev, Some("   ")).is_err());
        assert!(!creds.status(Provider::Jev).configured);
        assert_eq!(creds.api_key(Provider::Jev).unwrap(), None);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn no_keyring_ignores_the_os_keyring_in_debug_builds() {
        let creds = Credentials::with_env(Box::new(env_with(&[("OPENDAM_NO_KEYRING", "1")])));

        for status in creds.statuses() {
            assert!(!status.configured);
            assert!(!status.saved_in_keyring);
            assert!(status
                .keyring_problem
                .unwrap()
                .contains("OPENDAM_NO_KEYRING"));
        }
        assert!(creds.set_key(Provider::Anthropic, Some("key")).is_err());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn no_keyring_still_takes_keys_from_the_env() {
        let creds = Credentials::with_env(Box::new(env_with(&[
            ("OPENDAM_NO_KEYRING", "1"),
            ("ANTHROPIC_API_KEY", "from-env"),
        ])));

        assert_eq!(
            creds.status(Provider::Anthropic).source_label,
            "from ANTHROPIC_API_KEY"
        );
    }
}
