//! Settings: where the API is, which credential to use, and the switches read from the
//! environment. Nothing here talks to the network.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::credentials;

/// The public API base. Overridable with [`ENV_API_URL`] or [`ENV_API_URL_ALT`].
pub const DEFAULT_API_URL: &str = "https://api.contextstream.io";

pub const ENV_API_KEY: &str = "MACHINESTREAM_API_KEY";
pub const ENV_API_KEY_ALT: &str = "CONTEXTSTREAM_API_KEY";
pub const ENV_API_URL: &str = "MACHINESTREAM_API_URL";
pub const ENV_API_URL_ALT: &str = "CONTEXTSTREAM_API_URL";
/// `1` lets the server run a turned-away command on this machine itself.
pub const ENV_LOCAL_FALLBACK: &str = "MACHINESTREAM_LOCAL_FALLBACK";
/// `debug` turns on structured debug logs on stderr.
pub const ENV_LOG: &str = "MACHINESTREAM_LOG";
/// A file that receives one JSON line per check, for per-use billing adapters.
pub const ENV_USAGE_LOG: &str = "MACHINESTREAM_USAGE_LOG";
/// Where saved runs and the catalog cache are kept (default: `~/.cache/machinestream`).
pub const ENV_CACHE_DIR: &str = "MACHINESTREAM_CACHE_DIR";

/// The engine reads these two itself.
const ENGINE_DEVICE_PORTAL: &str = "CONTEXTSTREAM_DEVICE_PORTAL";

pub const WEBSITE: &str = "https://machinestream.io";
pub const DOCS_URL: &str = "https://machinestream.io/docs";
pub const PRICING_URL: &str = "https://machinestream.io/pricing";
pub const CONSOLE_URL: &str = "https://console.machinestream.io";

/// Where the credential in use came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// `MACHINESTREAM_API_KEY`.
    Env,
    /// `CONTEXTSTREAM_API_KEY`.
    EnvAlt,
    /// The saved login shared with the ContextStream MCP.
    Stored,
    /// The `Authorization: Bearer` header of an HTTP request.
    Request,
}

impl KeySource {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Env => "MACHINESTREAM_API_KEY",
            Self::EnvAlt => "CONTEXTSTREAM_API_KEY",
            Self::Stored => "saved login",
            Self::Request => "request header",
        }
    }
}

/// The API address and credential a process (or one HTTP request) runs with.
#[derive(Clone)]
pub struct Settings {
    pub api_url: String,
    pub api_key: Option<String>,
    pub source: Option<KeySource>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("api_url", &self.api_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("source", &self.source)
            .finish()
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Trim, drop trailing slashes and default to https.
pub fn normalize_api_url(url: &str) -> String {
    let mut url = url.trim().trim_end_matches('/').to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("https://{url}");
    }
    url
}

impl Settings {
    /// Environment first, then the saved login.
    pub fn resolve() -> Self {
        let stored = credentials::read();
        let api_url = env_value(ENV_API_URL)
            .or_else(|| env_value(ENV_API_URL_ALT))
            .or_else(|| stored.as_ref().and_then(|saved| saved.api_url.clone()))
            .map(|url| normalize_api_url(&url))
            .unwrap_or_else(|| DEFAULT_API_URL.to_string());
        let (api_key, source) = if let Some(key) = env_value(ENV_API_KEY) {
            (Some(key), Some(KeySource::Env))
        } else if let Some(key) = env_value(ENV_API_KEY_ALT) {
            (Some(key), Some(KeySource::EnvAlt))
        } else if let Some(key) = stored.and_then(|saved| saved.api_key) {
            (Some(key), Some(KeySource::Stored))
        } else {
            (None, None)
        };
        Self {
            api_url,
            api_key,
            source,
        }
    }

    /// The same API address with the key of one HTTP request.
    pub fn with_request_key(&self, key: String) -> Self {
        Self {
            api_url: self.api_url.clone(),
            api_key: Some(key),
            source: Some(KeySource::Request),
        }
    }

    pub fn signed_in(&self) -> bool {
        self.api_key.is_some()
    }
}

/// Make the engine see what this process was configured with. Call once, before any thread
/// is started: the engine's device login reads the API address from the environment.
pub fn export_for_engine() {
    if let Some(url) = env_value(ENV_API_URL) {
        std::env::set_var(ENV_API_URL_ALT, normalize_api_url(&url));
    }
    if std::env::var_os(ENGINE_DEVICE_PORTAL).is_none() {
        // Sign-in approval opens on the MachineStream console.
        std::env::set_var(ENGINE_DEVICE_PORTAL, "machinestream");
    }
}

pub fn local_fallback_enabled() -> bool {
    env_value(ENV_LOCAL_FALLBACK).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

pub fn debug_logging() -> bool {
    env_value(ENV_LOG).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "debug" | "trace" | "1" | "true"
        )
    })
}

pub fn usage_log_path() -> Option<PathBuf> {
    env_value(ENV_USAGE_LOG).map(PathBuf::from)
}

/// `~/.cache/machinestream` (or [`ENV_CACHE_DIR`]).
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = env_value(ENV_CACHE_DIR) {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    let base = dirs::cache_dir();
    #[cfg(not(windows))]
    let base = dirs::home_dir().map(|home| home.join(".cache"));
    base.unwrap_or_else(std::env::temp_dir)
        .join("machinestream")
}

/// A short, stable name for a credential that reveals nothing of it.
pub fn key_fingerprint(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))[..12].to_string()
}

/// Remove a credential from text that is about to be shown or logged.
pub fn redact(text: &str, secret: Option<&str>) -> String {
    match secret.filter(|secret| secret.len() >= 8) {
        Some(secret) if text.contains(secret) => text.replace(secret, "[redacted]"),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_urls_are_normalized() {
        assert_eq!(
            normalize_api_url(" https://api.example.test/ "),
            "https://api.example.test"
        );
        assert_eq!(
            normalize_api_url("api.example.test"),
            "https://api.example.test"
        );
        assert_eq!(
            normalize_api_url("http://127.0.0.1:8080//"),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn a_fingerprint_never_contains_the_key() {
        let key = "example-key-0123456789";
        let fingerprint = key_fingerprint(key);
        assert_eq!(fingerprint.len(), 12);
        assert!(!fingerprint.contains("example"));
        assert_eq!(fingerprint, key_fingerprint(key));
        assert_ne!(fingerprint, key_fingerprint("another-key-0123456789"));
    }

    #[test]
    fn redaction_removes_the_key_wherever_it_appears() {
        let key = "example-key-0123456789";
        let text = format!("request with {key} failed; retry with {key}");
        let shown = redact(&text, Some(key));
        assert!(!shown.contains(key));
        assert_eq!(shown.matches("[redacted]").count(), 2);
        assert_eq!(redact("nothing here", Some(key)), "nothing here");
        assert_eq!(redact("short ab", Some("ab")), "short ab");
    }

    #[test]
    fn settings_debug_output_hides_the_key() {
        let settings = Settings {
            api_url: DEFAULT_API_URL.to_string(),
            api_key: Some("example-key-0123456789".to_string()),
            source: Some(KeySource::Env),
        };
        let shown = format!("{settings:?}");
        assert!(!shown.contains("example-key"));
        assert!(shown.contains("[redacted]"));
    }
}
