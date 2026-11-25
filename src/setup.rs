//! Signing in and turning remote checks on.
//!
//! Sign-in is the engine's device login: a link and a short code the person approves in a
//! browser. The approval is traded for an API key that is saved in the login the
//! ContextStream MCP shares (`credentials`), so signing in once covers both.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcp_client::auth::{poll_device_login_once, start_device_login, DevicePollOutcome};
use mcp_client::ContextStreamClient;
use tokio::sync::watch;

use crate::config::Settings;
use crate::credentials;

/// How long one `setup` call waits for the approval before answering with the link, unless
/// [`ENV_WAIT`] says otherwise. Kept under a minute: many clients give a tool call 60 seconds.
pub const WAIT_IN_CALL: Duration = Duration::from_secs(50);

/// Seconds a `setup` call waits for the approval (0 to 120).
pub const ENV_WAIT: &str = "MACHINESTREAM_SETUP_WAIT_SECONDS";

/// How long this process waits inside a `setup` call.
pub fn wait_in_call() -> Duration {
    std::env::var(ENV_WAIT)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.min(120)))
        .unwrap_or(WAIT_IN_CALL)
}

/// A sign-in the person has been asked to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub verification_uri: String,
    pub user_code: String,
    /// Seconds until the code stops working.
    pub expires_in: u64,
}

/// Where a sign-in stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    Idle,
    Waiting(Pending),
    /// Signed in; the account's email when it could be read.
    Done(Option<String>),
    Failed(String),
}

struct Flow {
    started: Instant,
    pending: Pending,
    state: watch::Receiver<LoginState>,
    task: tokio::task::JoinHandle<()>,
}

/// The sign-in in progress, if any. One per process.
#[derive(Default)]
pub struct Login {
    flow: Mutex<Option<Flow>>,
}

impl Login {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The sign-in in progress, started now if there is none (or the last one expired or
    /// failed). The approval is watched for in the background from then on.
    pub async fn begin(
        &self,
        api_url: &str,
    ) -> Result<(Pending, watch::Receiver<LoginState>), String> {
        if let Some(flow) = self.flow.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let live = flow.started.elapsed() < Duration::from_secs(flow.pending.expires_in)
                && matches!(*flow.state.borrow(), LoginState::Waiting(_));
            if live {
                let mut pending = flow.pending.clone();
                pending.expires_in = pending
                    .expires_in
                    .saturating_sub(flow.started.elapsed().as_secs());
                return Ok((pending, flow.state.clone()));
            }
        }
        let started = start_device_login().await.map_err(|_| {
            "the sign-in could not be started (MachineStream could not be reached)".to_string()
        })?;
        let pending = Pending {
            verification_uri: started.verification_uri.clone(),
            user_code: started.user_code.clone(),
            expires_in: started.expires_in,
        };
        let (sender, receiver) = watch::channel(LoginState::Waiting(pending.clone()));
        let api_url = api_url.to_string();
        let device_code = started.device_code.clone();
        let (interval, expires_in) = (started.interval, started.expires_in);
        let task = tokio::spawn(async move {
            let outcome = wait_for_approval(&device_code, interval, expires_in, &api_url).await;
            let _ = sender.send(match outcome {
                Ok(email) => LoginState::Done(email),
                Err(reason) => LoginState::Failed(reason),
            });
        });
        let mut flow = self.flow.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = flow.replace(Flow {
            started: Instant::now(),
            pending: pending.clone(),
            state: receiver.clone(),
            task,
        }) {
            previous.task.abort();
        }
        Ok((pending, receiver))
    }

    /// Where the sign-in stands, without starting one.
    pub fn state(&self) -> LoginState {
        self.flow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(LoginState::Idle, |flow| flow.state.borrow().clone())
    }

    /// Forget a finished sign-in, so the next `begin` starts a new one.
    pub fn clear(&self) {
        if let Some(flow) = self.flow.lock().unwrap_or_else(|e| e.into_inner()).take() {
            flow.task.abort();
        }
    }
}

/// Wait for `state` to leave `Waiting`, at most `limit`.
pub async fn wait(mut state: watch::Receiver<LoginState>, limit: Duration) -> LoginState {
    let settled = async {
        loop {
            if !matches!(*state.borrow(), LoginState::Waiting(_)) {
                return state.borrow().clone();
            }
            if state.changed().await.is_err() {
                return state.borrow().clone();
            }
        }
    };
    match tokio::time::timeout(limit, settled).await {
        Ok(state) => state,
        Err(_) => state.borrow().clone(),
    }
}

/// Poll until the person approves, the code expires or is refused; then save the login.
async fn wait_for_approval(
    device_code: &str,
    interval: u64,
    expires_in: u64,
    api_url: &str,
) -> Result<Option<String>, String> {
    let deadline = Instant::now() + Duration::from_secs(expires_in.clamp(30, 1800));
    let mut pause = Duration::from_secs(interval.clamp(1, 30));
    let mut failures = 0;
    loop {
        tokio::time::sleep(pause).await;
        if Instant::now() >= deadline {
            return Err("the sign-in code expired before it was approved".to_string());
        }
        match poll_device_login_once(device_code).await {
            Ok(DevicePollOutcome::Pending { interval }) => {
                failures = 0;
                pause = Duration::from_secs(interval.clamp(1, 30));
            }
            Ok(DevicePollOutcome::Authorized(token)) => {
                return save_login(&token.access_token, api_url).await;
            }
            Ok(DevicePollOutcome::Expired) => {
                return Err("the sign-in code expired before it was approved".to_string())
            }
            Ok(DevicePollOutcome::Denied) => return Err("the sign-in was refused".to_string()),
            Err(_) => {
                failures += 1;
                if failures >= 5 {
                    return Err(
                        "the sign-in could not be confirmed (MachineStream could not be reached)"
                            .to_string(),
                    );
                }
            }
        }
    }
}

/// Trade a sign-in's short-lived token for an API key and save it. The key never leaves
/// this process except to the credentials file.
pub async fn save_login(access_token: &str, api_url: &str) -> Result<Option<String>, String> {
    let client = ContextStreamClient::new(mcp_types::Config {
        api_url: api_url.to_string(),
        jwt: Some(access_token.to_string()),
        api_key: None,
        user_agent: format!("machinestream-mcp/{}", env!("CARGO_PKG_VERSION")),
        ..Default::default()
    });
    let email = client.me().await.ok().map(|user| user.email);
    let api_key = client.create_api_key(&key_name()).await.map_err(|_| {
        "signed in, but an API key could not be created; try again shortly".to_string()
    })?;
    credentials::write(&api_key, api_url, email.as_deref())
        .map_err(|error| format!("signed in, but the login could not be saved ({error})"))?;
    Ok(email)
}

/// The name the API key is listed under in the account.
fn key_name() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty() && name.len() <= 64);
    match host {
        Some(host) => format!("MachineStream MCP ({host})"),
        None => "MachineStream MCP".to_string(),
    }
}

/// What a sign-in prompt says.
pub fn sign_in_text(pending: &Pending) -> String {
    format!(
        "Sign in to MachineStream: open {} and enter the code {} (valid for about {} minutes). \
         Then call setup again.",
        pending.verification_uri,
        pending.user_code,
        (pending.expires_in / 60).max(1)
    )
}

/// Whether `settings` already hold a credential.
pub fn signed_in(settings: &Settings) -> bool {
    settings.signed_in()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sign_in_prompt_names_the_link_and_the_code() {
        let text = sign_in_text(&Pending {
            verification_uri: "https://console.machinestream.io/device?user_code=ABCD-EFGH".into(),
            user_code: "ABCD-EFGH".into(),
            expires_in: 600,
        });
        assert!(text.contains("https://console.machinestream.io/device?user_code=ABCD-EFGH"));
        assert!(text.contains("the code ABCD-EFGH"));
        assert!(text.contains("about 10 minutes"));
        assert!(text.ends_with("Then call setup again."));
    }

    #[tokio::test]
    async fn waiting_returns_as_soon_as_the_sign_in_settles() {
        let pending = Pending {
            verification_uri: "https://example.test/device".into(),
            user_code: "CODE".into(),
            expires_in: 600,
        };
        let (sender, receiver) = watch::channel(LoginState::Waiting(pending.clone()));
        let still = wait(receiver.clone(), Duration::from_millis(50)).await;
        assert_eq!(still, LoginState::Waiting(pending));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = sender.send(LoginState::Done(Some("dev@example.test".into())));
        });
        let done = wait(receiver, Duration::from_secs(5)).await;
        assert_eq!(done, LoginState::Done(Some("dev@example.test".into())));
    }

    #[test]
    fn a_login_starts_idle() {
        let login = Login::default();
        assert_eq!(login.state(), LoginState::Idle);
        login.clear();
        assert_eq!(login.state(), LoginState::Idle);
    }
}
