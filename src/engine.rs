//! One account's connection to the service, for one working folder.
//!
//! Everything that talks to MachineStream goes through the shared engine crates
//! (`mcp-client`, `mcp-tools`, `mcp-session`): the catalog of tools the API defines, the
//! snapshot of the checkout, and the stream a run is answered on. This module only holds
//! the handles together.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcp_client::remote_checks::RemoteChecks;
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_tools::domains::hosted_tools::{self, HostedTool, HostedToolsGate};
use serde_json::Value;

use crate::billing::Price;
use crate::config::{key_fingerprint, Settings};

/// How long an account's settings (on or off, price, balance) are reused.
const STATUS_TTL: Duration = Duration::from_secs(120);

/// The name of the catalog tool that runs a command.
pub const CHECK_TOOL: &str = "check";

pub struct Engine {
    pub client: ContextStreamClient,
    pub session: Arc<SessionManager>,
    pub gate: Arc<HostedToolsGate>,
    /// The folder checks are about.
    pub root: PathBuf,
    pub api_url: String,
    /// A digest of the credential, for logs and the usage log.
    pub fingerprint: String,
    api_key: String,
    status: Mutex<Option<(Instant, RemoteChecks)>>,
}

impl Engine {
    /// `None` when `settings` hold no credential.
    pub async fn connect(settings: &Settings, root: &Path) -> Option<Arc<Self>> {
        let api_key = settings.api_key.clone()?;
        let config = mcp_types::Config {
            api_url: settings.api_url.clone(),
            api_key: Some(api_key.clone()),
            user_agent: format!("machinestream-mcp/{}", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        // The session's folder is the checkout a snapshot is taken of.
        session
            .initialize(None, None, Some(root.to_string_lossy().into_owned()), None)
            .await;
        Some(Arc::new(Self {
            client,
            session,
            gate: HostedToolsGate::global(),
            root: root.to_path_buf(),
            api_url: settings.api_url.clone(),
            fingerprint: key_fingerprint(&api_key),
            api_key,
            status: Mutex::new(None),
        }))
    }

    /// Remove this account's credential from text that is about to be shown.
    pub fn redact(&self, text: &str) -> String {
        crate::config::redact(text, Some(&self.api_key))
    }

    /// The account's remote-check settings, read at most every two minutes.
    pub async fn status(&self) -> Result<RemoteChecks, mcp_types::Error> {
        if let Some(status) = self.cached_status() {
            return Ok(status);
        }
        self.refresh_status().await
    }

    fn cached_status(&self) -> Option<RemoteChecks> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|(read_at, _)| read_at.elapsed() < STATUS_TTL)
            .map(|(_, status)| status.clone())
    }

    /// Read the settings again, now.
    pub async fn refresh_status(&self) -> Result<RemoteChecks, mcp_types::Error> {
        let status = self.client.remote_checks_status().await?;
        self.remember_status(&status);
        Ok(status)
    }

    pub fn remember_status(&self, status: &RemoteChecks) {
        *self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((Instant::now(), status.clone()));
    }

    /// The published price, when it can be read.
    pub async fn price(&self) -> Option<Price> {
        self.status()
            .await
            .ok()
            .map(|status| Price::from_status(&status))
    }

    /// The catalog's tool for running a command, when the account has one.
    pub async fn check_tool(&self) -> Option<HostedTool> {
        hosted_tools::find(&self.gate, &self.client, &self.session, CHECK_TOOL).await
    }

    /// The catalog's tool called `name`.
    pub async fn catalog_tool(&self, name: &str) -> Option<HostedTool> {
        hosted_tools::find(&self.gate, &self.client, &self.session, name).await
    }

    /// The `tools/list` entries of the catalog, except names `taken` says are in use.
    pub async fn catalog_entries(
        &self,
        budget: Duration,
        taken: impl Fn(&str) -> bool,
    ) -> Vec<Value> {
        hosted_tools::listing_entries(&self.gate, &self.client, &self.session, budget, taken).await
    }

    /// The workspace and project the engine resolves for this folder, when it has them.
    pub async fn scope(&self) -> (Option<String>, Option<String>) {
        let state = self.session.state().await;
        (
            state.workspace_id.map(|id| id.to_string()),
            state.project_id.map(|id| id.to_string()),
        )
    }
}
