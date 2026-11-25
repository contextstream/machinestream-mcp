//! Running commands remotely: one, or several against one snapshot of the checkout.
//!
//! The sequence is the engine's own (the one `contextstream-mcp run` follows): find the
//! tool the account's catalog defines for running a command, begin a snapshot of the
//! checkout, ask the API for the call (it answers with a stream for that one call), finish
//! the snapshot, then drive the stream and take the service's result. Nothing here knows
//! which commands the service takes or where it runs them: the catalog and each answer say.
//!
//! When the service does not run a command (it cannot match this machine, the account is
//! out of credits, it cannot be reached), the answer is "run it locally", with the reason.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use mcp_client::checkout_snapshot::CheckoutSnapshot;
use mcp_client::command_hints;
use mcp_client::frames::{self, Delivery, Event};
use mcp_tools::domains::hosted_tools::{
    begin_snapshot, call_api, call_id_of, drive_stream, enrich_frame_diagnostics,
    format_diagnostic_report, repo_key_of, stream_call, CallRefusal, HostedTool, ADD_CREDITS,
    CREDIT_MESSAGE,
};
use mcp_types::tool::{ContentItem, ToolResult};
use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::billing::{self, Billing, Price, Usage, UsageLine};
use crate::engine::Engine;
use crate::runlog::{self, Capture, RunRecord, RunStore};

/// One command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSpec {
    pub command: String,
    /// A folder inside the repository, relative to the working folder.
    pub cwd: Option<String>,
}

/// What applies to every command of one call.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout_seconds: Option<u64>,
    /// Run again even when the same files and command have a saved result.
    pub fresh: bool,
    /// Run a turned-away command on this machine.
    pub local_fallback: bool,
    /// The tool that asked (`check`, `check_parallel`, `run`), for the records.
    pub tool: &'static str,
}

/// What a running check says on the way.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckEvent {
    /// Output, as it arrives. `index` is the command's place in the call.
    Output { index: usize, bytes: Vec<u8> },
    /// The service is still working on it.
    Heartbeat { index: usize },
    /// A step of a multi-step check started.
    Stage {
        index: usize,
        step: u64,
        total: u64,
        message: String,
    },
    /// The command is being run on this machine instead.
    LocalFallback { index: usize, reason: String },
}

/// Where a command ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Place {
    Remote,
    Local,
    None,
}

impl Place {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Local => "local",
            Self::None => "none",
        }
    }
}

/// A path the snapshot left on this machine, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WithheldPath {
    pub path: String,
    pub reason: String,
}

/// How one command came out.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub command: String,
    /// The folder it was for, relative to the repository's root (`""` for the root).
    pub cwd: String,
    /// Whether the command ran, remotely or (with the local fallback) here.
    pub ran: bool,
    pub place: Place,
    pub exit_code: Option<i32>,
    /// Seconds the service (or the local run) reports for it.
    pub seconds: Option<f64>,
    pub cached: bool,
    /// For a cached result, how long the run it came from took.
    pub first_ran_seconds: Option<f64>,
    /// Seconds the whole call took as this machine saw it (snapshot, transfer and run).
    pub wall_seconds: Option<f64>,
    /// The git tree of the snapshot the command was sent with.
    pub tree: Option<String>,
    pub run_id: String,
    pub log_path: Option<String>,
    /// Why it was not run remotely, in plain words.
    pub reason: Option<String>,
    /// A short stable word for `reason`.
    pub reason_code: Option<&'static str>,
    /// What to do about `reason`, when there is something beyond running it locally.
    pub next_step: Option<String>,
    /// The failures the service extracted, as it describes them.
    pub failures: Vec<Value>,
    /// The engine's compact failure report, when the command failed and it has one.
    pub diagnostic_report: Option<String>,
    pub withheld: Vec<WithheldPath>,
    pub billing: Billing,
    /// The newest part of the output (all of it unless it was very long).
    pub output: String,
    pub output_lines: u64,
    pub output_bytes: u64,
}

impl CheckResult {
    pub fn passed(&self) -> Option<bool> {
        self.ran.then(|| self.exit_code == Some(0))
    }

    /// The instruction for a command that was not run.
    pub fn local_instruction(&self) -> String {
        if self.cwd.is_empty() {
            format!("Run this command locally: {}", self.command)
        } else {
            format!(
                "Run this command locally: {} (in the folder {})",
                self.command, self.cwd
            )
        }
    }
}

/// Why a command is not run remotely.
#[derive(Debug, Clone)]
struct TurnedAway {
    code: &'static str,
    reason: String,
    next_step: Option<String>,
}

impl TurnedAway {
    fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            next_step: None,
        }
    }

    fn with_next(mut self, next_step: impl Into<String>) -> Self {
        self.next_step = Some(next_step.into());
        self
    }
}

/// The first line of text in an engine result, bounded and on one line.
fn refusal_text(result: &ToolResult) -> Option<String> {
    result.content.iter().find_map(|item| match item {
        ContentItem::Text { text } => frames::display_text(text),
        _ => None,
    })
}

/// Why the catalog holds no tool to run a command with.
async fn why_no_tool(engine: &Engine) -> TurnedAway {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return TurnedAway::new(
            "switched_off",
            format!(
                "remote checks are switched off on this machine ({}=off)",
                command_hints::ENV_SWITCH
            ),
        );
    }
    match engine.refresh_status().await {
        Ok(status) if status.enabled => TurnedAway::new(
            "service_unreachable",
            "the list of remote tools could not be read just now",
        ),
        Ok(status) if status.available => {
            TurnedAway::new("checks_off", "remote checks are off for this account")
                .with_next("Call setup to see the price and turn them on.")
        }
        Ok(_) => TurnedAway::new(
            "checks_unavailable",
            "remote checks are unavailable for this account",
        ),
        Err(mcp_types::Error::Http {
            status: 401 | 403, ..
        }) => TurnedAway::new(
            "not_signed_in",
            "the service did not accept this credential",
        )
        .with_next("Call setup to sign in again."),
        Err(_) => TurnedAway::new("service_unreachable", "MachineStream could not be reached"),
    }
}

/// `base` (the working folder inside the repository) joined with `cwd`, as a path from the
/// repository's root. `cwd` must be relative and stay inside the repository.
pub fn resolve_cwd(base: &str, cwd: Option<&str>) -> Result<String, String> {
    let cwd = cwd.map(str::trim).unwrap_or("");
    if cwd.starts_with('/') || cwd.starts_with('\\') || cwd.contains(':') {
        return Err(format!(
            "cwd must be a relative path inside the repository, not {cwd}"
        ));
    }
    if cwd.contains('\0') {
        return Err("cwd is not a valid path".to_string());
    }
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in cwd.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(format!("cwd {cwd} is outside the repository"));
                }
            }
            part => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

/// The arguments of a call to `tool`: the command, the folder, and the options the tool's
/// own schema names.
fn call_arguments(tool: &HostedTool, command: &str, folder: &str, options: &RunOptions) -> Value {
    let hints = tool.command_hints.as_ref();
    let command_argument = hints.map_or("command", |hints| hints.argument.as_str());
    let cwd_argument = hints
        .and_then(|hints| hints.cwd_argument.as_deref())
        .or_else(|| takes(tool, "cwd").then_some("cwd"));
    let mut arguments = Map::new();
    arguments.insert(command_argument.to_string(), json!(command));
    if let Some(name) = cwd_argument.filter(|_| !folder.is_empty()) {
        arguments.insert(name.to_string(), json!(folder));
    }
    if let Some(seconds) = options
        .timeout_seconds
        .filter(|_| takes(tool, "timeout_seconds"))
    {
        arguments.insert("timeout_seconds".into(), json!(seconds));
    }
    if options.fresh && takes(tool, "fresh") {
        arguments.insert("fresh".into(), json!(true));
    }
    Value::Object(arguments)
}

/// Whether the tool's input schema has the property `name`.
fn takes(tool: &HostedTool, name: &str) -> bool {
    tool.input_schema
        .pointer(&format!("/properties/{name}"))
        .is_some()
}

/// What the service's last word on a call comes to.
#[derive(Debug)]
enum Verdict {
    Ran(Box<Value>),
    TurnedAway(TurnedAway),
}

/// What the host says when a run's failure was its own.
fn host_fault(frame: &Value) -> Option<String> {
    let cause = frame.get("environment").filter(|cause| cause.is_object())?;
    let reason = ["detail", "refused_code"].into_iter().find_map(|key| {
        cause
            .get(key)
            .and_then(Value::as_str)
            .and_then(frames::display_text)
    });
    Some(match reason {
        Some(why) => format!("the failure was the host's, not your change: {why}"),
        None => "the failure was the host's, not your change".to_string(),
    })
}

/// Read the result of a driven stream. `wrote` is whether output arrived before it.
fn verdict(result: Result<Value, String>, wrote: bool) -> Verdict {
    match result {
        Ok(frame) if frame.get("ran").and_then(Value::as_bool) == Some(true) => {
            match host_fault(&frame) {
                Some(said) => Verdict::TurnedAway(TurnedAway::new("host_fault", said)),
                None => Verdict::Ran(Box::new(frame)),
            }
        }
        Ok(frame) => {
            let declined = frames::declined_reason(&frame);
            let refused = declined.clone().or_else(|| {
                ["refused", "refused_code"].into_iter().find_map(|key| {
                    frame
                        .get(key)
                        .and_then(Value::as_str)
                        .and_then(frames::display_text)
                })
            });
            let reason = match (refused, wrote) {
                (Some(reason), false) => frames::decline_explanation(&reason).to_string(),
                (Some(reason), true) => format!(
                    "the service did not stand by the output it sent: {}",
                    frames::decline_explanation(&reason)
                ),
                (None, false) => "the service declined to run it".to_string(),
                (None, true) => "the service did not stand by the output it sent".to_string(),
            };
            Verdict::TurnedAway(TurnedAway::new("declined", reason))
        }
        Err(reason) if wrote => Verdict::TurnedAway(TurnedAway::new(
            "stream_ended",
            format!("the remote run ended without a result ({reason})"),
        )),
        Err(reason) => Verdict::TurnedAway(TurnedAway::new("service_unreachable", reason)),
    }
}

/// The command's own exit status, from the service's result.
fn exit_status(frame: &Value) -> i32 {
    match frame.get("exit").and_then(Value::as_i64) {
        Some(status) if (0..=255).contains(&status) => status as i32,
        Some(_) => 1,
        None => 0,
    }
}

/// The failures to show, as the engine's report chooses them: the new ones when the
/// service told them from failures the base already had.
fn failures_of(frame: &Value) -> Vec<Value> {
    let list = |pointer: &str| {
        frame
            .pointer(pointer)
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .cloned()
    };
    list("/diagnostics/new_failures")
        .or_else(|| list("/failing_on_base/new"))
        .or_else(|| list("/failures"))
        .unwrap_or_default()
}

/// A call the API took, waiting for the snapshot and its stream.
struct Issued {
    index: usize,
    data: Value,
}

/// Run `specs` remotely against one snapshot of the checkout, and return one result per
/// command, in order. Dropping the future closes the runs' streams.
pub async fn run_checks(
    engine: &Arc<Engine>,
    store: &RunStore,
    specs: &[CheckSpec],
    options: &RunOptions,
    events: Option<UnboundedSender<CheckEvent>>,
) -> Vec<CheckResult> {
    let ids: Vec<String> = specs.iter().map(|_| runlog::new_id()).collect();
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut outcomes: Vec<Option<Outcome>> = specs.iter().map(|_| None).collect();
    let mut folders: Vec<String> = specs.iter().map(|_| String::new()).collect();
    let mut repo_name = String::new();
    let mut withheld = Vec::new();
    let mut tree = None;
    let price = engine.price();
    let remote = remote_attempt(
        engine,
        store,
        specs,
        &ids,
        options,
        events.as_ref(),
        &mut outcomes,
        &mut folders,
        &mut repo_name,
        &mut withheld,
        &mut tree,
    );
    let (price, ()) = tokio::join!(price, remote);

    let mut results = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        let outcome = outcomes[index]
            .take()
            .unwrap_or_else(|| Outcome::TurnedAway {
                turned: TurnedAway::new("service_unreachable", "the run did not start"),
                capture: Box::new(Capture::in_memory()),
            });
        let mut result = finish(
            spec,
            &folders[index],
            &ids[index],
            outcome,
            price,
            &withheld,
            tree.as_deref(),
        );
        if !result.ran && options.local_fallback && result.reason_code != Some("invalid_cwd") {
            run_locally(engine, store, &mut result, options, index, events.as_ref()).await;
        }
        record(
            engine,
            store,
            &result,
            options.tool,
            &repo_name,
            &started_at,
        );
        results.push(result);
    }
    results
}

enum Outcome {
    Ran {
        frame: Box<Value>,
        capture: Box<Capture>,
        wall: Duration,
    },
    TurnedAway {
        turned: TurnedAway,
        capture: Box<Capture>,
    },
}

fn turned(turned: TurnedAway) -> Option<Outcome> {
    Some(Outcome::TurnedAway {
        turned,
        capture: Box::new(Capture::in_memory()),
    })
}

/// Try every command remotely, filling `outcomes`.
#[allow(clippy::too_many_arguments)]
async fn remote_attempt(
    engine: &Arc<Engine>,
    store: &RunStore,
    specs: &[CheckSpec],
    ids: &[String],
    options: &RunOptions,
    events: Option<&UnboundedSender<CheckEvent>>,
    outcomes: &mut [Option<Outcome>],
    folders: &mut [String],
    repo_name: &mut String,
    withheld: &mut Vec<WithheldPath>,
    tree: &mut Option<String>,
) {
    let all = |outcomes: &mut [Option<Outcome>], away: TurnedAway| {
        for outcome in outcomes.iter_mut().filter(|outcome| outcome.is_none()) {
            *outcome = turned(away.clone());
        }
    };
    let Some(tool) = engine.check_tool().await else {
        let away = why_no_tool(engine).await;
        return all(outcomes, away);
    };
    // One snapshot for the whole call. It is begun first: who the repository is comes at
    // once, and the rest is taken while the API is asked.
    let pending = match begin_snapshot(&engine.session, &tool).await {
        Ok(Some(pending)) => pending,
        Ok(None) => {
            return all(
                outcomes,
                TurnedAway::new("declined", "the service's tool takes no checkout"),
            )
        }
        Err(refusal) => {
            let reason = refusal_text(&refusal)
                .unwrap_or_else(|| "this checkout could not be described".to_string());
            return all(outcomes, TurnedAway::new("no_snapshot", reason));
        }
    };
    if let Some(name) = pending
        .identity
        .pointer("/repo/name")
        .and_then(Value::as_str)
    {
        *repo_name = name.to_string();
    }

    let calls = specs.iter().enumerate().map(|(index, spec)| {
        let folder = resolve_cwd(&pending.folder, spec.cwd.as_deref());
        let tool = &tool;
        let identity = pending.identity.clone();
        async move {
            let folder = match folder {
                Ok(folder) => folder,
                Err(reason) => {
                    return (
                        index,
                        String::new(),
                        Err(TurnedAway::new("invalid_cwd", reason)),
                    )
                }
            };
            let arguments = call_arguments(tool, &spec.command, &folder, options);
            let answer = call_api(
                &engine.client,
                &engine.session,
                tool,
                &arguments,
                Some(identity),
            )
            .await
            .map_err(|refusal| match refusal {
                CallRefusal::OutOfCredits => TurnedAway::new(
                    "out_of_credits",
                    CREDIT_MESSAGE.trim_end_matches('.').to_string(),
                )
                .with_next(ADD_CREDITS),
                CallRefusal::Other(result) => TurnedAway::new(
                    "declined",
                    refusal_text(&result)
                        .map(|text| engine.redact(&text))
                        .unwrap_or_else(|| "the call was not taken".to_string()),
                ),
            });
            (index, folder, answer)
        }
    });
    let mut issued = Vec::new();
    for (index, folder, answer) in join_all(calls).await {
        folders[index] = folder;
        match answer {
            Ok(data) => issued.push(Issued { index, data }),
            Err(away) => outcomes[index] = turned(away),
        }
    }
    if issued.is_empty() {
        return;
    }
    let snapshot = match pending.finish(&tool.name).await {
        Ok(snapshot) => snapshot,
        Err(refusal) => {
            let reason = refusal_text(&refusal)
                .unwrap_or_else(|| "this checkout could not be described".to_string());
            return all(outcomes, TurnedAway::new("no_snapshot", reason));
        }
    };
    *tree = Some(snapshot.target_tree.clone());
    *withheld = snapshot
        .withheld
        .iter()
        .map(|item| WithheldPath {
            path: item.path.clone(),
            reason: item.reason.clone(),
        })
        .collect();
    let (workspace, project) = engine.scope().await;
    let runs = issued.into_iter().map(|call| {
        let snapshot = &snapshot;
        let capture = store.capture(&ids[call.index]);
        let workspace = workspace.as_deref();
        let project = project.as_deref();
        async move {
            let index = call.index;
            (
                index,
                drive(engine, snapshot, call, capture, events, workspace, project).await,
            )
        }
    });
    for (index, outcome) in join_all(runs).await {
        outcomes[index] = Some(outcome);
    }
}

/// Drive one call's stream to its result, saving the output as it arrives.
async fn drive(
    engine: &Engine,
    snapshot: &CheckoutSnapshot,
    call: Issued,
    mut capture: Capture,
    events: Option<&UnboundedSender<CheckEvent>>,
    workspace: Option<&str>,
    project: Option<&str>,
) -> Outcome {
    let index = call.index;
    let stream = match stream_call(&call.data) {
        Ok(stream) => stream,
        Err(message) => {
            return Outcome::TurnedAway {
                turned: TurnedAway::new("declined", message),
                capture: Box::new(capture),
            }
        }
    };
    let started = Instant::now();
    let (sender, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let pump = async {
        while let Some(event) = inbox.recv().await {
            let forwarded = match event {
                Event::Output(bytes) => {
                    capture.push(&bytes);
                    CheckEvent::Output { index, bytes }
                }
                Event::Heartbeat => CheckEvent::Heartbeat { index },
                Event::StageProgress {
                    stage,
                    step,
                    total,
                    message,
                } => CheckEvent::Stage {
                    index,
                    step,
                    total,
                    message: message.unwrap_or_else(|| format!("[{step}/{total}] {stage}")),
                },
            };
            if let Some(events) = events {
                let _ = events.send(forwarded);
            }
        }
    };
    let (result, ()) = tokio::join!(
        drive_stream(&stream, snapshot, Delivery::Streamed, sender),
        pump
    );
    let wall = started.elapsed();
    let wrote = capture.bytes > 0;
    match verdict(result, wrote) {
        Verdict::Ran(mut frame) => {
            enrich_frame_diagnostics(
                &engine.client,
                call_id_of(&call.data),
                workspace,
                project,
                repo_key_of(&call.data),
                &mut frame,
            )
            .await;
            Outcome::Ran {
                frame,
                capture: Box::new(capture),
                wall,
            }
        }
        Verdict::TurnedAway(turned) => Outcome::TurnedAway {
            turned,
            capture: Box::new(capture),
        },
    }
}

/// Turn an outcome into the result the caller gets.
fn finish(
    spec: &CheckSpec,
    folder: &str,
    run_id: &str,
    outcome: Outcome,
    price: Option<Price>,
    withheld: &[WithheldPath],
    tree: Option<&str>,
) -> CheckResult {
    match outcome {
        Outcome::Ran {
            frame,
            mut capture,
            wall,
        } => {
            let exit = exit_status(&frame);
            let cached = frame.get("cache").and_then(Value::as_str) == Some("hit");
            let seconds = frame
                .get("seconds")
                .and_then(Value::as_f64)
                .unwrap_or(wall.as_secs_f64());
            let ran_seconds = frame.get("ran_seconds").and_then(Value::as_f64);
            let output = capture.text();
            let usage = if cached {
                Usage::Cached
            } else {
                Usage::Remote {
                    seconds: ran_seconds.unwrap_or(seconds),
                }
            };
            let diagnostic_report = (exit != 0)
                .then(|| format_diagnostic_report(&frame, ""))
                .flatten();
            CheckResult {
                command: spec.command.clone(),
                cwd: folder.to_string(),
                ran: true,
                place: Place::Remote,
                exit_code: Some(exit),
                seconds: Some(seconds),
                cached,
                first_ran_seconds: cached.then_some(ran_seconds).flatten(),
                wall_seconds: Some(wall.as_secs_f64()),
                tree: tree.map(str::to_string),
                run_id: run_id.to_string(),
                output_lines: capture.line_count(),
                output_bytes: capture.bytes,
                log_path: capture
                    .finish()
                    .map(|path| path.to_string_lossy().into_owned()),
                reason: None,
                reason_code: None,
                next_step: None,
                failures: if exit != 0 {
                    failures_of(&frame)
                } else {
                    Vec::new()
                },
                diagnostic_report,
                withheld: withheld.to_vec(),
                billing: billing::estimate(usage, price),
                output,
            }
        }
        Outcome::TurnedAway {
            turned,
            mut capture,
        } => {
            let output = capture.text();
            CheckResult {
                command: spec.command.clone(),
                cwd: folder.to_string(),
                ran: false,
                place: Place::None,
                exit_code: None,
                seconds: None,
                cached: false,
                first_ran_seconds: None,
                wall_seconds: None,
                tree: tree.map(str::to_string),
                run_id: run_id.to_string(),
                output_lines: capture.line_count(),
                output_bytes: capture.bytes,
                log_path: capture
                    .finish()
                    .map(|path| path.to_string_lossy().into_owned()),
                reason: Some(turned.reason),
                reason_code: Some(turned.code),
                next_step: turned.next_step,
                failures: Vec::new(),
                diagnostic_report: None,
                withheld: withheld.to_vec(),
                billing: billing::estimate(Usage::NotRun, price),
                output,
            }
        }
    }
}

/// Save the run's record and its usage line.
fn record(
    engine: &Engine,
    store: &RunStore,
    result: &CheckResult,
    tool: &str,
    repo: &str,
    started_at: &str,
) {
    store.save(&RunRecord {
        id: result.run_id.clone(),
        tool: tool.to_string(),
        command: result.command.clone(),
        cwd: result.cwd.clone(),
        repo: repo.to_string(),
        started_at: started_at.to_string(),
        ran: result.ran,
        place: result.place.as_str().to_string(),
        exit_code: result.exit_code,
        seconds: result.seconds,
        cached: result.cached,
        reason: result.reason.clone(),
        lines: result.output_lines,
        bytes: result.output_bytes,
    });
    if let Some(path) = crate::config::usage_log_path() {
        let line = UsageLine {
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            tool: tool.to_string(),
            run_id: result.run_id.clone(),
            billable_seconds: result.billing.billable_seconds,
            credits_estimated: result.billing.credits_estimated,
            cached: result.cached,
            ran: result.place == Place::Remote,
            key_fingerprint: engine.fingerprint.clone(),
        };
        if let Err(error) = billing::append_usage(&path, &line) {
            tracing::warn!(%error, "the usage log could not be written");
        }
    }
}

/// The shell a command runs with on this machine: bash, as it does wherever else it runs.
const LOCAL_SHELL: &str = "bash";

/// Run a turned-away command on this machine and put what it did into `result`.
async fn run_locally(
    engine: &Engine,
    store: &RunStore,
    result: &mut CheckResult,
    options: &RunOptions,
    index: usize,
    events: Option<&UnboundedSender<CheckEvent>>,
) {
    let dir = local_dir(&engine.root, result).await;
    if let Some(events) = events {
        let _ = events.send(CheckEvent::LocalFallback {
            index,
            reason: result.reason.clone().unwrap_or_default(),
        });
    }
    // Output the service sent and did not stand by is not part of the local run.
    if let Some(path) = result.log_path.take() {
        let _ = std::fs::remove_file(path);
    }
    result.output.clear();
    let mut capture = store.capture(&result.run_id);
    let limit = Duration::from_secs(options.timeout_seconds.unwrap_or(3600).clamp(1, 3600));
    let started = Instant::now();
    let exit = local_command(&result.command, &dir, limit, &mut capture, index, events).await;
    result.seconds = Some(started.elapsed().as_secs_f64());
    result.output = capture.text();
    result.output_lines = capture.line_count();
    result.output_bytes = capture.bytes;
    result.log_path = capture
        .finish()
        .map(|path| path.to_string_lossy().into_owned());
    match exit {
        Ok(code) => {
            result.ran = true;
            result.place = Place::Local;
            result.exit_code = Some(code);
            result.billing = Billing::free("ran_locally");
        }
        Err(error) => {
            let reason = result.reason.take().unwrap_or_default();
            result.reason = Some(format!("{reason}; and it could not be run here: {error}"));
        }
    }
}

/// The folder a local run starts in: the repository's root joined with the result's folder.
async fn local_dir(root: &Path, result: &CheckResult) -> std::path::PathBuf {
    let top = tokio::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()));
    match top {
        Some(top) if result.cwd.is_empty() => top,
        Some(top) => top.join(&result.cwd),
        None => root.to_path_buf(),
    }
}

async fn local_command(
    command: &str,
    dir: &Path,
    limit: Duration,
    capture: &mut Capture,
    index: usize,
    events: Option<&UnboundedSender<CheckEvent>>,
) -> Result<i32, String> {
    use tokio::io::AsyncReadExt;
    let mut shell = if cfg!(windows) {
        let mut shell = tokio::process::Command::new("cmd");
        shell.arg("/C").arg(command);
        shell
    } else {
        let mut shell = tokio::process::Command::new(LOCAL_SHELL);
        shell.arg("-c").arg(command);
        shell
    };
    let mut child = shell
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("the shell could not be started ({error})"))?;
    let (sender, mut chunks) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    for mut pipe in [
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ]
    .into_iter()
    .flatten()
    {
        let sender = sender.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 8192];
            while let Ok(read) = pipe.read(&mut buffer).await {
                if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
    }
    drop(sender);
    let run = async {
        while let Some(bytes) = chunks.recv().await {
            capture.push(&bytes);
            if let Some(events) = events {
                let _ = events.send(CheckEvent::Output { index, bytes });
            }
        }
        child.wait().await
    };
    match tokio::time::timeout(limit, run).await {
        Ok(Ok(status)) => Ok(status_code(status)),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => {
            capture.push(format!("\n[stopped after {} s]\n", limit.as_secs()).as_bytes());
            Ok(124)
        }
    }
}

#[cfg(unix)]
fn status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn status_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// A result for a call that could not be tried at all (no credential).
pub fn not_signed_in(spec: &CheckSpec) -> CheckResult {
    CheckResult {
        command: spec.command.clone(),
        cwd: spec.cwd.clone().unwrap_or_default(),
        ran: false,
        place: Place::None,
        exit_code: None,
        seconds: None,
        cached: false,
        first_ran_seconds: None,
        wall_seconds: None,
        tree: None,
        run_id: runlog::new_id(),
        log_path: None,
        reason: Some("this machine is not signed in to MachineStream".to_string()),
        reason_code: Some("not_signed_in"),
        next_step: Some("Call setup to sign in.".to_string()),
        failures: Vec::new(),
        diagnostic_report: None,
        withheld: Vec::new(),
        billing: Billing::free("not_run_remotely"),
        output: String::new(),
        output_lines: 0,
        output_bytes: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_tools::domains::hosted_tools::parse_catalog;

    fn tool() -> HostedTool {
        parse_catalog(&json!({"tools": [{
            "name": "check",
            "description": "Runs a command.",
            "input_schema": {"type": "object", "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "fresh": {"type": "boolean"},
                "timeout_seconds": {"type": "integer"}
            }, "required": ["command"]},
            "transport": {"kind": "stream"},
            "attachments": ["checkout_snapshot"],
            "command_hints": {"argument": "command", "cwd_argument": "cwd", "patterns": ["^fixture-check\\b"]}
        }]}))
        .remove(0)
    }

    fn options() -> RunOptions {
        RunOptions {
            timeout_seconds: None,
            fresh: false,
            local_fallback: false,
            tool: "check",
        }
    }

    #[test]
    fn cwd_is_joined_and_kept_inside_the_repository() {
        assert_eq!(resolve_cwd("", None).unwrap(), "");
        assert_eq!(resolve_cwd("", Some("crates/api")).unwrap(), "crates/api");
        assert_eq!(resolve_cwd("web", Some("./src/")).unwrap(), "web/src");
        assert_eq!(resolve_cwd("web/app", Some("../lib")).unwrap(), "web/lib");
        assert_eq!(resolve_cwd("web", Some("..")).unwrap(), "");
        assert_eq!(resolve_cwd("web", Some("a\\b")).unwrap(), "web/a/b");
        assert!(resolve_cwd("", Some("..")).is_err());
        assert!(resolve_cwd("web", Some("../../etc")).is_err());
        assert!(resolve_cwd("", Some("/etc")).is_err());
        assert!(resolve_cwd("", Some("C:\\Windows")).is_err());
        assert!(resolve_cwd("", Some("\\\\server\\share")).is_err());
    }

    #[test]
    fn arguments_follow_the_tools_own_definition() {
        let tool = tool();
        assert_eq!(
            call_arguments(&tool, "echo hi", "", &options()),
            json!({"command": "echo hi"})
        );
        let all = RunOptions {
            timeout_seconds: Some(90),
            fresh: true,
            ..options()
        };
        assert_eq!(
            call_arguments(&tool, "echo hi", "web", &all),
            json!({"command": "echo hi", "cwd": "web", "timeout_seconds": 90, "fresh": true})
        );
    }

    #[test]
    fn options_the_tool_does_not_take_are_not_sent() {
        let mut tool = tool();
        tool.input_schema =
            json!({"type": "object", "properties": {"command": {"type": "string"}}});
        tool.command_hints.as_mut().unwrap().cwd_argument = None;
        let all = RunOptions {
            timeout_seconds: Some(90),
            fresh: true,
            ..options()
        };
        assert_eq!(
            call_arguments(&tool, "echo hi", "web", &all),
            json!({"command": "echo hi"})
        );
    }

    #[test]
    fn a_result_that_ran_is_a_verdict_whatever_its_exit() {
        for exit in [0, 1, 101] {
            let frame = json!({"type": "result", "ran": true, "exit": exit, "seconds": 1.5});
            match verdict(Ok(frame), true) {
                Verdict::Ran(frame) => assert_eq!(exit_status(&frame), exit),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_decline_carries_the_services_reason() {
        let frame = json!({"type": "result", "ran": false, "exit": 75,
            "reason": "the fixture declines this command, so it runs in the session"});
        match verdict(Ok(frame), false) {
            Verdict::TurnedAway(away) => {
                assert_eq!(away.code, "declined");
                assert_eq!(away.reason, "the fixture declines this command");
            }
            other => panic!("{other:?}"),
        }
        match verdict(Ok(json!({"ran": false})), false) {
            Verdict::TurnedAway(away) => assert_eq!(away.reason, "the service declined to run it"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn output_the_service_does_not_stand_by_is_not_a_verdict() {
        let frame = json!({"ran": false, "exit": 75, "refused": "the workspace was lost"});
        match verdict(Ok(frame), true) {
            Verdict::TurnedAway(away) => assert_eq!(
                away.reason,
                "the service did not stand by the output it sent: the workspace was lost"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_host_fault_is_never_reported_as_the_commands_failure() {
        let frame = json!({"ran": true, "exit": 137, "environment": {"detail": "out of memory on the host"}});
        match verdict(Ok(frame), true) {
            Verdict::TurnedAway(away) => {
                assert_eq!(away.code, "host_fault");
                assert!(away.reason.contains("out of memory on the host"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_stream_that_ends_early_says_whether_output_was_seen() {
        match verdict(Err("the connection ended before a result".into()), true) {
            Verdict::TurnedAway(away) => assert_eq!(away.code, "stream_ended"),
            other => panic!("{other:?}"),
        }
        match verdict(Err("the service could not be reached".into()), false) {
            Verdict::TurnedAway(away) => {
                assert_eq!(away.code, "service_unreachable");
                assert_eq!(away.reason, "the service could not be reached");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn exit_statuses_outside_a_byte_count_as_failure() {
        assert_eq!(exit_status(&json!({"exit": 0})), 0);
        assert_eq!(exit_status(&json!({"exit": 2})), 2);
        assert_eq!(exit_status(&json!({"exit": 4096})), 1);
        assert_eq!(exit_status(&json!({"exit": -1})), 1);
        assert_eq!(exit_status(&json!({})), 0);
    }

    #[test]
    fn new_failures_come_before_all_failures() {
        let frame = json!({
            "failures": [{"file": "a.rs"}, {"file": "b.rs"}],
            "failing_on_base": {"new": [{"file": "b.rs"}], "old": [{"file": "a.rs"}]}
        });
        assert_eq!(failures_of(&frame), vec![json!({"file": "b.rs"})]);
        assert_eq!(
            failures_of(&json!({"failures": [{"file": "a.rs"}]})),
            vec![json!({"file": "a.rs"})]
        );
        assert!(failures_of(&json!({})).is_empty());
    }

    #[test]
    fn the_local_instruction_names_the_folder() {
        let mut result = not_signed_in(&CheckSpec {
            command: "cargo test".into(),
            cwd: None,
        });
        assert_eq!(
            result.local_instruction(),
            "Run this command locally: cargo test"
        );
        result.cwd = "crates/api".into();
        assert_eq!(
            result.local_instruction(),
            "Run this command locally: cargo test (in the folder crates/api)"
        );
        assert_eq!(result.passed(), None);
    }

    #[tokio::test]
    async fn a_local_run_captures_both_streams_and_the_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut capture = Capture::in_memory();
        let exit = local_command(
            "echo to-stdout; echo to-stderr >&2; exit 3",
            dir.path(),
            Duration::from_secs(20),
            &mut capture,
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(exit, 3);
        let text = capture.text();
        assert!(text.contains("to-stdout"), "{text}");
        assert!(text.contains("to-stderr"), "{text}");
    }

    #[tokio::test]
    async fn a_local_run_is_stopped_at_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut capture = Capture::in_memory();
        let exit = local_command(
            "sleep 30",
            dir.path(),
            Duration::from_millis(300),
            &mut capture,
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(exit, 124);
        assert!(capture.text().contains("stopped after"));
    }
}
