//! "Am I ready, and what will it cost": one read-only look at the account and the checkout.

use std::path::{Path, PathBuf};

use mcp_client::checkout_snapshot::{CheckoutSnapshot, SnapshotError};
use mcp_client::remote_checks::{local_language_ids, ProjectLanguages, RemoteChecks};
use mcp_tools::domains::remote_checks::local_languages;
use serde_json::{json, Value};

use crate::config::{self, KeySource, Settings};
use crate::engine::Engine;
use crate::format::{age_text, seconds_text};
use crate::runlog::{RunRecord, RunStore};

/// Runs shown in a status.
pub const RECENT_RUNS: usize = 5;

/// What is known about the folder checks would be about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub folder: PathBuf,
    /// The repository's name, when the folder is in one.
    pub repo: Option<String>,
    /// Whether a snapshot can be taken.
    pub snapshot: bool,
    /// Why not, when it cannot.
    pub problem: Option<String>,
}

impl Checkout {
    /// Look at `folder` with the engine's own first step of a snapshot. Nothing is sent.
    pub async fn inspect(folder: &Path) -> Self {
        let base = Self {
            folder: folder.to_path_buf(),
            repo: None,
            snapshot: false,
            problem: None,
        };
        if !git_present().await {
            return Self {
                problem: Some("git is not installed, and a snapshot needs it".to_string()),
                ..base
            };
        }
        match CheckoutSnapshot::begin(folder).await {
            Ok(start) => Self {
                repo: Some(start.repo_name().to_string()),
                snapshot: true,
                ..base
            },
            Err(SnapshotError::NotARepository(_)) => Self {
                problem: Some("this folder is not inside a git repository".to_string()),
                ..base
            },
            Err(error) => Self {
                problem: Some(error.to_string()),
                ..base
            },
        }
    }
}

async fn git_present() -> bool {
    tokio::process::Command::new("git")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// What the account's side looks like.
#[derive(Debug, Clone)]
pub enum Account {
    SignedOut,
    /// The credential was refused.
    Rejected,
    /// The API could not be reached; the reason, in plain words.
    Unreachable(String),
    Known {
        email: Option<String>,
        checks: Box<RemoteChecks>,
    },
}

#[derive(Debug)]
pub struct Status {
    pub account: Account,
    pub key_source: Option<KeySource>,
    pub api_url: String,
    pub checkout: Checkout,
    pub project: Option<ProjectLanguages>,
    pub language_ids: Vec<String>,
    pub recent: Vec<RunRecord>,
    pub local_fallback: bool,
}

impl Status {
    /// Read everything. Network calls are bounded by the engine's own short timeouts.
    pub async fn gather(
        settings: &Settings,
        engine: Option<&Engine>,
        folder: &Path,
        store: &RunStore,
    ) -> Self {
        let checkout = Checkout::inspect(folder);
        let account = async {
            let Some(engine) = engine else {
                return Account::SignedOut;
            };
            let (me, checks) = tokio::join!(
                tokio::time::timeout(std::time::Duration::from_secs(8), engine.client.me()),
                engine.refresh_status()
            );
            match checks {
                Ok(checks) => Account::Known {
                    email: me.ok().and_then(Result::ok).map(|user| user.email),
                    checks: Box::new(checks),
                },
                Err(mcp_types::Error::Http {
                    status: 401 | 403, ..
                }) => Account::Rejected,
                Err(error) => Account::Unreachable(short_error(&engine.redact(&error.to_string()))),
            }
        };
        let (checkout, account) = tokio::join!(checkout, account);
        let project = match &account {
            Account::Known { checks, .. } => local_languages(Some(folder), checks),
            _ => None,
        };
        let mut recent = store.list();
        recent.truncate(RECENT_RUNS);
        Self {
            account,
            key_source: settings.source,
            api_url: settings.api_url.clone(),
            checkout,
            project,
            language_ids: local_language_ids(folder),
            recent,
            local_fallback: config::local_fallback_enabled(),
        }
    }

    pub fn checks(&self) -> Option<&RemoteChecks> {
        match &self.account {
            Account::Known { checks, .. } => Some(checks),
            _ => None,
        }
    }

    /// Whether a check can be asked for right now.
    pub fn ready(&self) -> bool {
        self.checks().is_some_and(|checks| checks.enabled) && self.checkout.snapshot
    }

    /// A command worth trying first in this checkout.
    pub fn first_check(&self) -> &'static str {
        first_check_for(&self.language_ids)
    }

    /// What is missing, and the exact step that fixes it.
    pub fn next_step(&self) -> String {
        match &self.account {
            Account::SignedOut => "Call setup to sign in.".to_string(),
            Account::Rejected => {
                "The service did not accept this credential. Call setup to sign in again."
                    .to_string()
            }
            Account::Unreachable(_) => {
                "MachineStream cannot be reached right now, so checks will answer \"run it locally\". Try again shortly."
                    .to_string()
            }
            Account::Known { checks, .. } if !checks.enabled && checks.available => {
                "Call setup to see what turning remote checks on means, then setup with enable_remote_checks: true once the user agrees."
                    .to_string()
            }
            Account::Known { checks, .. } if !checks.enabled => {
                "Remote checks are unavailable for this account. Run checks locally.".to_string()
            }
            Account::Known { .. } if !self.checkout.snapshot => format!(
                "A check needs a git checkout: {}. Start the server in a repository.",
                self.checkout
                    .problem
                    .as_deref()
                    .unwrap_or("no snapshot can be taken here")
            ),
            Account::Known { .. } => format!(
                "Call check with a command, for example `{}`.",
                self.first_check()
            ),
        }
    }

    fn headline(&self) -> String {
        match &self.account {
            Account::SignedOut => {
                "Not ready: this machine is not signed in to MachineStream.".into()
            }
            Account::Rejected => "Not ready: the service did not accept this credential.".into(),
            Account::Unreachable(why) => {
                format!("Not ready: MachineStream could not be reached ({why}).")
            }
            Account::Known { checks, .. } if !checks.enabled && checks.available => {
                "Not ready: remote checks are off for this account.".into()
            }
            Account::Known { checks, .. } if !checks.enabled => {
                "Not ready: remote checks are unavailable for this account.".into()
            }
            Account::Known { .. } if !self.checkout.snapshot => {
                "Not ready: no snapshot can be taken of this folder.".into()
            }
            Account::Known { .. } => {
                "Ready: remote checks are on and this checkout can be snapshotted.".into()
            }
        }
    }

    pub fn text(&self) -> String {
        let mut lines = vec![self.headline()];
        match &self.account {
            Account::Known { email, checks } => {
                let source = self
                    .key_source
                    .map(|source| format!(" ({})", source.describe()))
                    .unwrap_or_default();
                lines.push(format!(
                    "Account: {}{source}",
                    email.as_deref().unwrap_or("signed in")
                ));
                lines.push(format!(
                    "Remote checks: {}. Extra languages: {}.",
                    checks.state(),
                    match checks.more_engines {
                        Some(true) => "on",
                        Some(false) => "off",
                        None => "unknown",
                    }
                ));
                lines.push(format!("Price: {}", checks.price_line()));
                if let Some(balance) = checks.balance_line() {
                    lines.push(format!("Balance: {balance}."));
                }
                let (standard, extra) = language_names(checks);
                if !standard.is_empty() {
                    lines.push(format!("Languages: {}.", standard.join(", ")));
                }
                if !extra.is_empty() {
                    lines.push(format!(
                        "Extra languages ({}): {}.",
                        if checks.more_engines == Some(true) {
                            "on"
                        } else {
                            "off"
                        },
                        extra.join(", ")
                    ));
                }
            }
            Account::SignedOut | Account::Rejected | Account::Unreachable(_) => {}
        }
        lines.push(match (&self.checkout.repo, &self.checkout.problem) {
            (Some(repo), _) => {
                let detected = self
                    .project
                    .as_ref()
                    .map(|project| {
                        format!(" Detected: {}.", project.summary().trim_end_matches('.'))
                    })
                    .unwrap_or_default();
                format!(
                    "This checkout: {repo} (git repository; a snapshot can be taken).{detected}"
                )
            }
            (None, Some(problem)) => format!("This checkout: {problem}."),
            (None, None) => "This checkout: unknown.".to_string(),
        });
        if self.local_fallback {
            lines.push(
                "Local fallback: on (a command the service turns away is run on this machine)."
                    .to_string(),
            );
        }
        if !self.recent.is_empty() {
            lines.push("Recent runs:".to_string());
            let now = chrono::Utc::now();
            for run in &self.recent {
                let timing = run
                    .seconds
                    .map(|seconds| format!(" in {}", seconds_text(seconds)))
                    .unwrap_or_default();
                lines.push(format!(
                    "- {}: {}{timing}{}, {} (run {})",
                    one_line(&run.command, 80),
                    run.result_word(),
                    if run.cached { ", from cache" } else { "" },
                    age_text(&run.started_at, now),
                    run.id
                ));
            }
        }
        lines.push(format!("Next: {}", self.next_step()));
        lines.join("\n")
    }

    pub fn structured(&self) -> Value {
        let now = chrono::Utc::now();
        let (signed_in, email, reachable) = match &self.account {
            Account::SignedOut => (false, None, None),
            Account::Rejected => (false, None, Some(true)),
            Account::Unreachable(_) => (true, None, Some(false)),
            Account::Known { email, .. } => (true, email.clone(), Some(true)),
        };
        let checks = self.checks();
        let (standard, extra) = checks.map(language_names).unwrap_or_default();
        json!({
            "ready": self.ready(),
            "signed_in": signed_in,
            "account": email,
            "credential_source": self.key_source.map(KeySource::describe),
            "service_reachable": reachable,
            "remote_checks": checks.map(|checks| checks.state()),
            "more_languages": checks.and_then(|checks| checks.more_engines),
            "price": checks.map(|checks| json!({
                "credits_per_10s": checks.pricing.as_ref().map(|price| price.credits_per_10s),
                "unit": checks.pricing.as_ref().map(|price| price.unit.clone()),
                "no_charge": checks.credits.as_ref().is_some_and(|credits| credits.no_charge),
                "text": checks.price_line(),
            })),
            "credits_available": checks
                .and_then(|checks| checks.credits.as_ref())
                .and_then(|credits| credits.available),
            "languages": standard,
            "extra_languages": extra,
            "checkout": {
                "folder": self.checkout.folder.to_string_lossy(),
                "repo": self.checkout.repo,
                "snapshot": self.checkout.snapshot,
                "problem": self.checkout.problem,
                "detected": self.project.as_ref().map(|project| json!({
                    "remote": project.standard,
                    "remote_with_extra_languages": project.more,
                    "local_only": project.local,
                })),
            },
            "local_fallback": self.local_fallback,
            "recent_runs": self.recent.iter().map(|run| json!({
                "run_id": run.id,
                "command": run.command,
                "result": run.result_word(),
                "exit_code": run.exit_code,
                "seconds": run.seconds,
                "cached": run.cached,
                "where": run.place,
                "age": age_text(&run.started_at, now),
            })).collect::<Vec<_>>(),
            "next_step": self.next_step(),
        })
    }
}

pub fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ready": {"type": "boolean", "description": "A check can be asked for now."},
            "signed_in": {"type": "boolean"},
            "account": {"anyOf": [{"type": "string"}, {"type": "null"}], "description": "The signed-in account's email."},
            "credential_source": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            "service_reachable": {"anyOf": [{"type": "boolean"}, {"type": "null"}]},
            "remote_checks": {"anyOf": [{"type": "string"}, {"type": "null"}], "description": "on, off, or unavailable for this account."},
            "more_languages": {"anyOf": [{"type": "boolean"}, {"type": "null"}]},
            "price": {"anyOf": [{"type": "object"}, {"type": "null"}]},
            "credits_available": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
            "languages": {"type": "array", "items": {"type": "string"}},
            "extra_languages": {"type": "array", "items": {"type": "string"}},
            "checkout": {"type": "object"},
            "local_fallback": {"type": "boolean"},
            "recent_runs": {"type": "array", "items": {"type": "object"}},
            "next_step": {"type": "string"}
        },
        "required": ["ready", "signed_in", "checkout", "recent_runs", "next_step"]
    })
}

/// The names of the standard languages and of the extra ones, as the API lists them.
pub fn language_names(checks: &RemoteChecks) -> (Vec<String>, Vec<String>) {
    let names = |extra: bool| {
        checks
            .languages
            .iter()
            .filter(|language| language.more == extra)
            .map(|language| language.name.clone())
            .collect::<Vec<_>>()
    };
    (names(false), names(true))
}

/// A check worth trying first for the languages found in a folder.
pub fn first_check_for(language_ids: &[String]) -> &'static str {
    match language_ids.first().map(String::as_str) {
        Some("rust") => "cargo check",
        Some("typescript") => "npx tsc --noEmit",
        Some("python") => "pytest -q",
        Some("go") => "go vet ./...",
        _ => "cargo check",
    }
}

pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}...", flat.chars().take(max).collect::<String>())
    }
}

/// An error in a few plain words, without addresses.
pub fn short_error(text: &str) -> String {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("timed out") || lowered.contains("timeout") {
        "the request timed out".to_string()
    } else if lowered.contains("dns") || lowered.contains("resolve") {
        "the address could not be resolved".to_string()
    } else if lowered.contains("connect") || lowered.contains("network") {
        "the connection failed".to_string()
    } else {
        one_line(text, 120)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(enabled: bool, available: bool) -> RemoteChecks {
        serde_json::from_value(json!({
            "tool": "check", "enabled": enabled, "available": available, "more_engines": false,
            "pricing": {"credits_per_10s": 20, "unit": "Context Credits", "billing": "run_time", "free": []},
            "credits": {"available": 12345, "no_charge": false},
            "languages": [
                {"id": "rust", "name": "Rust", "tools": "cargo", "more": false, "markers": ["Cargo.toml"]},
                {"id": "ruby", "name": "Ruby", "tools": "rspec", "more": true, "markers": ["Gemfile"]}
            ]
        }))
        .unwrap()
    }

    fn status(account: Account, snapshot: bool) -> Status {
        Status {
            account,
            key_source: Some(KeySource::Stored),
            api_url: config::DEFAULT_API_URL.into(),
            checkout: Checkout {
                folder: PathBuf::from("/work/demo"),
                repo: snapshot.then(|| "demo".to_string()),
                snapshot,
                problem: (!snapshot).then(|| "this folder is not inside a git repository".into()),
            },
            project: None,
            language_ids: vec!["rust".into()],
            recent: Vec::new(),
            local_fallback: false,
        }
    }

    fn known(enabled: bool, available: bool) -> Account {
        Account::Known {
            email: Some("dev@example.test".into()),
            checks: Box::new(checks(enabled, available)),
        }
    }

    #[test]
    fn a_ready_status_says_so_and_suggests_a_first_check() {
        let status = status(known(true, true), true);
        assert!(status.ready());
        let text = status.text();
        assert!(text.starts_with("Ready: remote checks are on"));
        assert!(text.contains("Account: dev@example.test (saved login)"));
        assert!(text.contains("Remote checks: on. Extra languages: off."));
        assert!(text.contains("Price: 20 Context Credits per 10 s of run time."));
        assert!(text.contains("Balance: You have 12,345 credits."));
        assert!(text.contains("Languages: Rust."));
        assert!(text.contains("Extra languages (off): Ruby."));
        assert!(text.ends_with("Next: Call check with a command, for example `cargo check`."));
        let value = status.structured();
        assert_eq!(value["ready"], true);
        assert_eq!(value["account"], "dev@example.test");
        assert_eq!(value["remote_checks"], "on");
        assert_eq!(value["credits_available"], 12345);
        assert_eq!(value["price"]["credits_per_10s"], 20);
        assert_eq!(value["checkout"]["repo"], "demo");
    }

    #[test]
    fn each_missing_piece_names_its_next_step() {
        let signed_out = status(Account::SignedOut, true);
        assert!(!signed_out.ready());
        assert_eq!(signed_out.next_step(), "Call setup to sign in.");
        assert!(signed_out
            .text()
            .starts_with("Not ready: this machine is not signed in"));
        assert_eq!(signed_out.structured()["signed_in"], false);

        let off = status(known(false, true), true);
        assert!(off.text().starts_with("Not ready: remote checks are off"));
        assert!(off.next_step().contains("enable_remote_checks: true"));

        let unavailable = status(known(false, false), true);
        assert!(unavailable.next_step().contains("unavailable"));

        let no_repo = status(known(true, true), false);
        assert!(!no_repo.ready());
        assert!(no_repo.next_step().contains("not inside a git repository"));

        let down = status(Account::Unreachable("the connection failed".into()), true);
        assert!(down
            .text()
            .contains("could not be reached (the connection failed)"));
        assert_eq!(down.structured()["service_reachable"], false);

        let rejected = status(Account::Rejected, true);
        assert!(rejected.next_step().contains("sign in again"));
    }

    #[test]
    fn recent_runs_are_listed_with_result_and_age() {
        let mut status = status(known(true, true), true);
        status.recent = vec![RunRecord {
            id: "20260101-000000-abcd".into(),
            tool: "check".into(),
            command: "cargo   test\n --all".into(),
            cwd: String::new(),
            repo: "demo".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            ran: true,
            place: "remote".into(),
            exit_code: Some(101),
            seconds: Some(3.94),
            cached: false,
            reason: None,
            lines: 10,
            bytes: 100,
        }];
        let text = status.text();
        assert!(
            text.contains("- cargo test --all: failed (exit 101) in 3.9 s, 0 s ago (run 20260101-000000-abcd)"),
            "{text}"
        );
        assert_eq!(
            status.structured()["recent_runs"][0]["result"],
            "failed (exit 101)"
        );
    }

    #[test]
    fn first_checks_follow_the_detected_language() {
        assert_eq!(first_check_for(&["typescript".into()]), "npx tsc --noEmit");
        assert_eq!(first_check_for(&["python".into()]), "pytest -q");
        assert_eq!(first_check_for(&["go".into()]), "go vet ./...");
        assert_eq!(first_check_for(&[]), "cargo check");
    }

    #[test]
    fn errors_are_shortened_without_addresses() {
        assert_eq!(
            short_error("Network error: error sending request for url (https://internal.example/x): operation timed out"),
            "the request timed out"
        );
        assert_eq!(
            short_error("Network error: tcp connect error"),
            "the connection failed"
        );
        assert_eq!(short_error("odd"), "odd");
    }

    #[tokio::test]
    async fn a_folder_outside_git_cannot_be_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = Checkout::inspect(dir.path()).await;
        assert!(!checkout.snapshot);
        assert!(checkout.repo.is_none());
        assert!(checkout.problem.is_some());
    }

    #[tokio::test]
    async fn a_git_checkout_can_be_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("demo");
        std::fs::create_dir(&repo).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let checkout = Checkout::inspect(&repo).await;
        assert!(checkout.snapshot, "{checkout:?}");
        assert_eq!(checkout.repo.as_deref(), Some("demo"));
    }
}
