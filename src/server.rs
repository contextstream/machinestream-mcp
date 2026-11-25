//! The MCP server: a few tools, each a whole job.
//!
//! `check` and `check_parallel` run commands remotely, `status` says whether that is ready
//! and what it costs, `setup` does the next missing step, and `run_output` reads a saved
//! run. Tools the account's catalog defines beyond these are listed as the API describes
//! them. Results are short, lead with the answer and say the next step.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Extensions,
    GetPromptRequestParams, GetPromptResponse, GetPromptResult, Implementation, JsonObject,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, ProgressToken, Prompt, PromptArgument,
    PromptMessage, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, Role, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{json, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::catalog::{self, Catalog};
use crate::check::{self, CheckEvent, CheckResult, CheckSpec, RunOptions};
use crate::config::{self, Settings};
use crate::engine::Engine;
use crate::format::{self, OutputQuery};
use crate::runlog::RunStore;
use crate::setup::{self, Login, LoginState};
use crate::status::{self, Status};

pub const SERVER_NAME: &str = "machinestream";
pub const SERVER_TITLE: &str = "MachineStream";
pub const RUN_URI_PREFIX: &str = "machinestream://runs/";
pub const ROOT_HEADER: &str = "x-machinestream-root";

pub const INSTRUCTIONS: &str = "\
MachineStream runs builds, tests, lints and type checks remotely, on the exact state of this checkout (committed, uncommitted and untracked files), and returns the same exit code and output a local run gives.
- Use `check` for a build, test, lint or type-check command instead of running it in the shell.
- Use `check_parallel` to run several commands at once against one snapshot of the checkout.
- If a result says a command was not run remotely, run it locally as written. That is a routing answer, not an error.
- Do not send commands that deploy, publish, change files, start servers or need secrets.
- `status` says whether checks are ready and what they cost, `setup` does the next missing step, and `run_output` reads a saved run's full output.";

/// How a session's working folder is chosen.
#[derive(Debug, Clone)]
pub enum Roots {
    /// One folder for the whole process (stdio).
    Fixed(PathBuf),
    /// Folders HTTP sessions may work in; the first is the default.
    Allowed(Vec<PathBuf>),
}

pub struct Core {
    roots: Roots,
    login: Arc<Login>,
    /// The folder the client named as its workspace, asked once when the process's own
    /// folder is not a checkout.
    client_root: tokio::sync::OnceCell<Option<PathBuf>>,
    catalog: Arc<Catalog>,
    engines: tokio::sync::Mutex<HashMap<String, Arc<Engine>>>,
    /// The client of the newest request, for announcing a changed tool list.
    peer: Mutex<Option<Peer<RoleServer>>>,
    /// The catalog tools the last `tools/list` answer showed.
    last_listed: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct MachineStream {
    core: Arc<Core>,
}

/// Who a request is for and where it works.
struct Scope {
    settings: Settings,
    root: PathBuf,
    engine: Option<Arc<Engine>>,
    http: bool,
    /// Where this caller's runs are saved. Over HTTP each account has its own.
    store: RunStore,
}

fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn structured_result(text: impl Into<String>, structured: Value) -> CallToolResult {
    let mut result = text_result(text);
    result.structured_content = Some(structured);
    result.is_error = Some(false);
    result
}

/// A failure of the tool itself (bad arguments, a broken local file), which the caller reads.
fn tool_error(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}

fn object(value: Value) -> Arc<JsonObject> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => Arc::new(JsonObject::new()),
    }
}

fn check_tool_definition() -> Tool {
    Tool::new(
        "check",
        "Run one build, test, lint or type-check command remotely on MachineStream, against the exact state of this checkout (committed, uncommitted and untracked files), and get back the command's own exit code and output. Use it instead of the shell for commands such as `cargo test`, `npx tsc --noEmit`, `pytest` or `go vet ./...`. Do not use it for commands that deploy, publish, change files, start a server or need secrets: run those yourself. If the result says the command was not run remotely, run it locally as written (that is a routing answer, not a failure). The same command on unchanged files returns the saved result at no cost. To run several commands at once, use `check_parallel`.",
        object(json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "minLength": 1, "maxLength": 2000, "description": "The command, as you would type it in a shell from the working folder (or from `cwd`). Example: `cargo test -p api`."},
                "cwd": {"type": "string", "maxLength": 500, "description": "A folder inside the repository to run in, relative to the working folder. Defaults to the working folder."},
                "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 3600, "description": "Stop the command after this many seconds."},
                "fresh": {"type": "boolean", "description": "Run again even if the same command already ran on exactly these files (bypasses the result cache)."},
                "max_output_lines": {"type": "integer", "minimum": 0, "maximum": 2000, "description": "How many of the last output lines to return. Default: 200 for a failing command, 30 for a passing one. The full output is always saved; read more with `run_output`."}
            },
            "required": ["command"],
            "additionalProperties": false
        })),
    )
    .with_title("Run a check remotely")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(true)
            .open_world(true),
    )
    .with_raw_output_schema(object(format::check_output_schema()))
}

fn check_parallel_tool_definition() -> Tool {
    Tool::new(
        "check_parallel",
        "Run 2 to 8 build, test, lint or type-check commands at the same time on MachineStream, all against one snapshot of this checkout, and get one result per command with failures first. Use it to verify a change in one call (for example lint, types and tests together) instead of running the commands one after another. The same rules as `check` apply: no commands that deploy, publish, change files, start a server or need secrets, and a command that comes back as not run remotely should be run locally. For a single command, use `check`.",
        object(json!({
            "type": "object",
            "properties": {
                "checks": {
                    "type": "array", "minItems": 2, "maxItems": 8,
                    "description": "The commands to run at the same time.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "minLength": 1, "maxLength": 2000, "description": "The command, as you would type it in a shell."},
                            "cwd": {"type": "string", "maxLength": 500, "description": "A folder inside the repository to run in, relative to the working folder."}
                        },
                        "required": ["command"],
                        "additionalProperties": false
                    }
                },
                "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 3600, "description": "Stop each command after this many seconds."},
                "fresh": {"type": "boolean", "description": "Run again even if a command already ran on exactly these files."},
                "max_output_lines": {"type": "integer", "minimum": 0, "maximum": 2000, "description": "How many of the last output lines to return per command. Default: 60 for a failing command, 5 for a passing one."}
            },
            "required": ["checks"],
            "additionalProperties": false
        })),
    )
    .with_title("Run several checks at once")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(true)
            .open_world(true),
    )
    .with_raw_output_schema(object(format::parallel_output_schema()))
}

fn status_tool_definition() -> Tool {
    Tool::new(
        "status",
        "Report whether remote checks are ready here and what they cost: the signed-in account, whether remote checks and the extra languages are on, the credit balance and the price, the languages the account can run, whether this folder is a git checkout a snapshot can be taken of, and the last five runs. When something is missing it names the exact next step. Read-only and free. Use it before a first check, or when a check was not run remotely and you want to know why. It runs nothing: use `check` for that.",
        object(json!({"type": "object", "properties": {}, "additionalProperties": false})),
    )
    .with_title("Readiness and cost")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .idempotent(true)
            .open_world(true),
    )
    .with_raw_output_schema(object(status::output_schema()))
}

fn setup_tool_definition() -> Tool {
    Tool::new(
        "setup",
        "Do the next missing step toward a working remote check. Safe to call again at any time. Not signed in: starts a browser sign-in and returns the link and code to give the user. Signed in with remote checks off: says what turning them on means (they are billed in Context Credits by run time), with the price and the balance, and turns them on only when called with `enable_remote_checks: true` after the user agreed. Already set up: says so and suggests a first check. It never turns anything on or spends credits without the explicit argument. Use it when `status` or a check result says something is missing. To only look, use `status`.",
        object(json!({
            "type": "object",
            "properties": {
                "enable_remote_checks": {"type": "boolean", "description": "Turn remote checks on for the account. Pass true only after the user agreed to the price."},
                "enable_more_languages": {"type": "boolean", "description": "Also turn on the extra languages (for example Java, .NET, Ruby, PHP, C and C++). Pass true only after the user agreed."}
            },
            "additionalProperties": false
        })),
    )
    .with_title("Set up remote checks")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(true)
            .open_world(true),
    )
}

fn run_output_tool_definition() -> Tool {
    Tool::new(
        "run_output",
        "Read the saved output of an earlier check without running it again. Use it when a result was truncated, to search a long log with `grep`, or to look at an earlier run. Defaults to the latest run. Read-only, local and free. It cannot run a command: use `check` for that.",
        object(json!({
            "type": "object",
            "properties": {
                "run_id": {"type": "string", "description": "The run to read, from a check result or `status`. Default: the latest run with output."},
                "grep": {"type": "string", "maxLength": 500, "description": "Show only lines matching this regular expression (Rust regex syntax, case-sensitive; prefix with (?i) to ignore case)."},
                "context": {"type": "integer", "minimum": 0, "maximum": 20, "description": "Lines to show around each `grep` match. Default 0."},
                "head": {"type": "integer", "minimum": 1, "description": "Show the first N lines (or the first N matches with `grep`)."},
                "tail": {"type": "integer", "minimum": 1, "description": "Show the last N lines (or the last N matches with `grep`)."},
                "max_lines": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "The most lines to return. Default 200."}
            },
            "additionalProperties": false
        })),
    )
    .with_title("Read a saved run")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .idempotent(true)
            .open_world(false),
    )
    .with_raw_output_schema(object(json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string"},
            "command": {"type": "string"},
            "result": {"type": "string"},
            "total_lines": {"type": "integer"},
            "shown_lines": {"type": "integer"},
            "matches": {"anyOf": [{"type": "integer"}, {"type": "null"}], "description": "Lines matching `grep`, before any limit."},
            "truncated": {"type": "boolean", "description": "Selected lines were left out to stay within max_lines."},
            "log_path": {"type": "string"}
        },
        "required": ["run_id", "total_lines", "shown_lines", "truncated"]
    })))
}

/// This server's own tools, in the order they are listed.
pub fn builtin_tools() -> Vec<Tool> {
    vec![
        check_tool_definition(),
        check_parallel_tool_definition(),
        status_tool_definition(),
        setup_tool_definition(),
        run_output_tool_definition(),
    ]
}

/// Read a string argument.
fn arg_string(args: &JsonObject, name: &str) -> Result<Option<String>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{name} must be a string")),
    }
}

fn arg_bool(args: &JsonObject, name: &str) -> Result<Option<bool>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{name} must be true or false")),
    }
}

fn arg_integer(args: &JsonObject, name: &str, min: u64, max: u64) -> Result<Option<u64>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64().or_else(|| {
            value
                .as_f64()
                .filter(|number| number.fract() == 0.0 && *number >= 0.0)
                .map(|number| number as u64)
        }) {
            Some(number) if (min..=max).contains(&number) => Ok(Some(number)),
            _ => Err(format!("{name} must be a whole number from {min} to {max}")),
        },
    }
}

/// Reject arguments the tool does not have, so a typo is not silently ignored.
fn known_arguments(args: &JsonObject, known: &[&str]) -> Result<(), String> {
    match args.keys().find(|key| !known.contains(&key.as_str())) {
        Some(unknown) => Err(format!(
            "unknown argument {unknown}; this tool takes: {}",
            known.join(", ")
        )),
        None => Ok(()),
    }
}

fn command_argument(value: Option<String>) -> Result<String, String> {
    let command = value.ok_or("command is required")?;
    let command = command.trim();
    if command.is_empty() {
        return Err("command is required".to_string());
    }
    if command.len() > 2000 {
        return Err("command is longer than 2000 characters".to_string());
    }
    if command.contains('\0') {
        return Err("command is not valid text".to_string());
    }
    Ok(command.to_string())
}

/// The arguments of `check`.
#[derive(Debug, PartialEq)]
pub struct CheckArguments {
    pub spec: CheckSpec,
    pub timeout_seconds: Option<u64>,
    pub fresh: bool,
    pub max_output_lines: Option<usize>,
}

pub fn parse_check_arguments(args: &JsonObject) -> Result<CheckArguments, String> {
    known_arguments(
        args,
        &[
            "command",
            "cwd",
            "timeout_seconds",
            "fresh",
            "max_output_lines",
        ],
    )?;
    Ok(CheckArguments {
        spec: CheckSpec {
            command: command_argument(arg_string(args, "command")?)?,
            cwd: arg_string(args, "cwd")?.filter(|cwd| !cwd.trim().is_empty()),
        },
        timeout_seconds: arg_integer(args, "timeout_seconds", 1, 3600)?,
        fresh: arg_bool(args, "fresh")?.unwrap_or(false),
        max_output_lines: arg_integer(args, "max_output_lines", 0, 2000)?.map(|n| n as usize),
    })
}

/// The arguments of `check_parallel`.
#[derive(Debug, PartialEq)]
pub struct ParallelArguments {
    pub specs: Vec<CheckSpec>,
    pub timeout_seconds: Option<u64>,
    pub fresh: bool,
    pub max_output_lines: Option<usize>,
}

pub fn parse_parallel_arguments(args: &JsonObject) -> Result<ParallelArguments, String> {
    known_arguments(
        args,
        &["checks", "timeout_seconds", "fresh", "max_output_lines"],
    )?;
    let checks = args
        .get("checks")
        .and_then(Value::as_array)
        .ok_or("checks is required: a list of 2 to 8 {command, cwd?} objects")?;
    if !(2..=8).contains(&checks.len()) {
        return Err(format!(
            "checks takes 2 to 8 commands, not {} (for one command use check)",
            checks.len()
        ));
    }
    let mut specs = Vec::with_capacity(checks.len());
    for (index, item) in checks.iter().enumerate() {
        let place = |error: String| format!("checks[{index}]: {error}");
        let object = match item {
            Value::Object(object) => object.clone(),
            // A bare string is taken as the command.
            Value::String(command) => {
                let mut object = JsonObject::new();
                object.insert("command".into(), json!(command));
                object
            }
            _ => return Err(place("must be an object with a command".to_string())),
        };
        known_arguments(&object, &["command", "cwd"]).map_err(place)?;
        specs.push(CheckSpec {
            command: command_argument(arg_string(&object, "command").map_err(place)?)
                .map_err(place)?,
            cwd: arg_string(&object, "cwd")
                .map_err(place)?
                .filter(|cwd| !cwd.trim().is_empty()),
        });
    }
    Ok(ParallelArguments {
        specs,
        timeout_seconds: arg_integer(args, "timeout_seconds", 1, 3600)?,
        fresh: arg_bool(args, "fresh")?.unwrap_or(false),
        max_output_lines: arg_integer(args, "max_output_lines", 0, 2000)?.map(|n| n as usize),
    })
}

/// The arguments of `run_output`.
pub fn parse_output_arguments(args: &JsonObject) -> Result<(Option<String>, OutputQuery), String> {
    known_arguments(
        args,
        &["run_id", "grep", "context", "head", "tail", "max_lines"],
    )?;
    let grep = match arg_string(args, "grep")?.filter(|pattern| !pattern.is_empty()) {
        Some(pattern) if pattern.len() > 500 => {
            return Err("grep is longer than 500 characters".to_string())
        }
        Some(pattern) => Some(
            regex::RegexBuilder::new(&pattern)
                .size_limit(1 << 20)
                .build()
                .map_err(|_| format!("grep is not a valid regular expression: {pattern}"))?,
        ),
        None => None,
    };
    let head = arg_integer(args, "head", 1, 1_000_000)?.map(|n| n as usize);
    let tail = arg_integer(args, "tail", 1, 1_000_000)?.map(|n| n as usize);
    if head.is_some() && tail.is_some() {
        return Err("give head or tail, not both".to_string());
    }
    Ok((
        arg_string(args, "run_id")?
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty() && id != "latest"),
        OutputQuery {
            grep,
            context: arg_integer(args, "context", 0, 20)?.unwrap_or(0) as usize,
            head,
            tail,
            max_lines: arg_integer(args, "max_lines", 1, 2000)?.unwrap_or(200) as usize,
        },
    ))
}

/// Turn what running checks say into progress notifications: the newest output lines, at
/// most about once a second.
async fn forward_progress(
    peer: Peer<RoleServer>,
    token: ProgressToken,
    commands: usize,
    mut events: UnboundedReceiver<CheckEvent>,
) {
    const EVERY: Duration = Duration::from_millis(800);
    let started = Instant::now();
    let mut last_sent: Option<Instant> = None;
    let mut last_value = 0.0_f64;
    let mut partial: Vec<Vec<u8>> = vec![Vec::new(); commands];
    let mut recent: Vec<VecDeque<String>> = vec![VecDeque::new(); commands];
    while let Some(event) = events.recv().await {
        let (index, urgent, said) = match event {
            CheckEvent::Output { index, bytes } => {
                for byte in bytes {
                    if byte == b'\n' {
                        let line = format::strip_ansi(&String::from_utf8_lossy(&partial[index]));
                        partial[index].clear();
                        let line = line.trim_end();
                        if !line.trim().is_empty() {
                            if recent[index].len() == 3 {
                                recent[index].pop_front();
                            }
                            recent[index].push_back(status::one_line(line, 200));
                        }
                    } else if partial[index].len() < 2048 {
                        partial[index].push(byte);
                    }
                }
                (
                    index,
                    false,
                    recent[index].iter().cloned().collect::<Vec<_>>().join("\n"),
                )
            }
            CheckEvent::Heartbeat { index } => (
                index,
                false,
                format!("still running ({} s)", started.elapsed().as_secs()),
            ),
            CheckEvent::Stage { index, message, .. } => (index, true, message),
            CheckEvent::LocalFallback { index, reason } => (
                index,
                true,
                format!("not run remotely ({reason}); running it on this machine"),
            ),
        };
        if said.is_empty() || (!urgent && last_sent.is_some_and(|at| at.elapsed() < EVERY)) {
            continue;
        }
        last_sent = Some(Instant::now());
        // Each value is larger than the one before, as the protocol asks.
        last_value = started.elapsed().as_secs_f64().max(last_value + 0.01);
        let message = if commands > 1 {
            format!("[{}] {said}", index + 1)
        } else {
            said
        };
        let _ = peer
            .notify_progress(
                ProgressNotificationParam::new(token.clone(), last_value).with_message(message),
            )
            .await;
    }
}

/// Whether `folder` is inside a git checkout (a `.git` in it or above it).
fn in_git_checkout(folder: &Path) -> bool {
    folder
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

impl MachineStream {
    pub fn new(roots: Roots) -> Self {
        Self {
            core: Arc::new(Core {
                roots,
                client_root: tokio::sync::OnceCell::new(),
                login: Login::new(),
                catalog: Catalog::new(),
                engines: tokio::sync::Mutex::new(HashMap::new()),
                peer: Mutex::new(None),
                last_listed: Mutex::new(None),
            }),
        }
    }

    /// Start reading the catalog, so the first `tools/list` finds it ready.
    pub async fn prefetch(&self) {
        if let Roots::Fixed(root) = &self.core.roots {
            let settings = Settings::resolve();
            if let Some(engine) = self.engine_for(&settings, root).await {
                self.core
                    .catalog
                    .refresh_if_stale(&engine, self.on_catalog_change());
            }
        }
    }

    async fn engine_for(&self, settings: &Settings, root: &Path) -> Option<Arc<Engine>> {
        let key = settings.api_key.as_deref()?;
        let name = format!(
            "{}\0{}\0{}",
            config::key_fingerprint(key),
            settings.api_url,
            root.display()
        );
        let mut engines = self.core.engines.lock().await;
        if let Some(engine) = engines.get(&name) {
            return Some(engine.clone());
        }
        let engine = Engine::connect(settings, root).await?;
        if engines.len() >= 256 {
            engines.clear();
        }
        engines.insert(name, engine.clone());
        Some(engine)
    }

    /// Who this request is for: the key of an HTTP request's `Authorization` header, else
    /// the process's own; and the folder it works in.
    async fn scope(
        &self,
        extensions: &Extensions,
        peer: &Peer<RoleServer>,
    ) -> Result<Scope, McpError> {
        let parts = extensions.get::<axum::http::request::Parts>();
        let base = Settings::resolve();
        let (settings, root, http) = match (&self.core.roots, parts) {
            (Roots::Allowed(allowed), parts) => {
                let header = |name: &str| {
                    parts
                        .and_then(|parts| parts.headers.get(name))
                        .and_then(|value| value.to_str().ok())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                };
                let bearer = header("authorization").and_then(|value| {
                    value
                        .strip_prefix("Bearer ")
                        .or_else(|| value.strip_prefix("bearer "))
                        .map(str::trim)
                        .filter(|token| !token.is_empty())
                });
                let settings = match bearer {
                    Some(key) => base.with_request_key(key.to_string()),
                    None => base,
                };
                let root = crate::http::resolve_root(allowed, header(ROOT_HEADER))
                    .map_err(|reason| McpError::invalid_params(reason, None))?;
                (settings, root, true)
            }
            (Roots::Fixed(root), _) => {
                let root = match in_git_checkout(root) {
                    true => root.clone(),
                    false => self.client_root(peer).await.unwrap_or_else(|| root.clone()),
                };
                (base, root, false)
            }
        };
        let engine = self.engine_for(&settings, &root).await;
        let store = match (http, &settings.api_key) {
            (false, _) => RunStore::default(),
            // One account's runs are never shown to another.
            (true, Some(key)) => RunStore::at(
                config::cache_dir()
                    .join("accounts")
                    .join(config::key_fingerprint(key))
                    .join("runs"),
            ),
            (true, None) => RunStore::at(config::cache_dir().join("accounts").join("none")),
        };
        Ok(Scope {
            settings,
            root,
            engine,
            http,
            store,
        })
    }

    /// The first folder the client lists as a root, when it lists any. Asked once, and only
    /// of a client that says it has roots (older protocol revisions; the newest has no
    /// server-to-client requests, and marks roots as deprecated).
    #[allow(deprecated)]
    async fn client_root(&self, peer: &Peer<RoleServer>) -> Option<PathBuf> {
        self.core
            .client_root
            .get_or_init(|| async {
                let lists_roots = peer
                    .peer_info()
                    .is_some_and(|info| info.capabilities.roots.is_some());
                if !lists_roots {
                    return None;
                }
                let listed = tokio::time::timeout(Duration::from_secs(3), peer.list_roots())
                    .await
                    .ok()?
                    .ok()?;
                listed
                    .roots
                    .iter()
                    .filter_map(|root| url::Url::parse(&root.uri).ok()?.to_file_path().ok())
                    .find(|path| path.is_dir())
            })
            .await
            .clone()
    }

    /// Read the catalog now and tell the client when the tools it was last shown changed.
    async fn announce_changed_tools(&self, engine: &Arc<Engine>, peer: &Peer<RoleServer>) {
        let now = Value::Array(self.core.catalog.read_now(engine).await).to_string();
        let changed = self
            .core
            .last_listed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|listed| *listed != now);
        if changed {
            let _ = peer.notify_tool_list_changed().await;
        }
    }

    fn remember_peer(&self, peer: &Peer<RoleServer>) {
        *self.core.peer.lock().unwrap_or_else(|e| e.into_inner()) = Some(peer.clone());
    }

    /// What to do when a fresh read of the catalog differs from what was listed.
    fn on_catalog_change(&self) -> catalog::OnChange {
        let core = self.core.clone();
        Arc::new(move || {
            let peer = core.peer.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(peer) = peer {
                tokio::spawn(async move {
                    let _ = peer.notify_tool_list_changed().await;
                });
            }
        })
    }

    async fn run_with_progress(
        &self,
        engine: &Arc<Engine>,
        store: &RunStore,
        specs: &[CheckSpec],
        options: &RunOptions,
        context: &RequestContext<RoleServer>,
    ) -> Option<Vec<CheckResult>> {
        let (events, forwarder) = match context.meta.get_progress_token() {
            Some(token) => {
                let (sender, receiver): (UnboundedSender<CheckEvent>, _) =
                    tokio::sync::mpsc::unbounded_channel();
                let task = tokio::spawn(forward_progress(
                    context.peer.clone(),
                    token,
                    specs.len(),
                    receiver,
                ));
                (Some(sender), Some(task))
            }
            None => (None, None),
        };
        let results = tokio::select! {
            results = check::run_checks(engine, store, specs, options, events) => Some(results),
            // The client gave up on the call: dropping the run stops it.
            () = context.ct.cancelled() => None,
        };
        if let Some(forwarder) = forwarder {
            match &results {
                Some(_) => {
                    let _ = forwarder.await;
                }
                None => forwarder.abort(),
            }
        }
        results
    }

    async fn tool_check(
        &self,
        scope: Scope,
        args: JsonObject,
        context: &RequestContext<RoleServer>,
    ) -> CallToolResult {
        let arguments = match parse_check_arguments(&args) {
            Ok(arguments) => arguments,
            Err(error) => return tool_error(format!("check was not run: {error}.")),
        };
        let Some(engine) = scope.engine.clone() else {
            let result = check::not_signed_in(&arguments.spec);
            let rendered = format::render_check(&result, arguments.max_output_lines);
            return structured_result(rendered.text, format::structured_check(&result, false));
        };
        let options = RunOptions {
            timeout_seconds: arguments.timeout_seconds,
            fresh: arguments.fresh,
            local_fallback: config::local_fallback_enabled() && !scope.http,
            tool: "check",
        };
        let specs = [arguments.spec];
        let Some(mut results) = self
            .run_with_progress(&engine, &scope.store, &specs, &options, context)
            .await
        else {
            return tool_error("check was cancelled before it finished.");
        };
        let result = results.remove(0);
        let rendered = format::render_check(&result, arguments.max_output_lines);
        structured_result(
            rendered.text,
            format::structured_check(&result, rendered.truncated),
        )
    }

    async fn tool_check_parallel(
        &self,
        scope: Scope,
        args: JsonObject,
        context: &RequestContext<RoleServer>,
    ) -> CallToolResult {
        let arguments = match parse_parallel_arguments(&args) {
            Ok(arguments) => arguments,
            Err(error) => return tool_error(format!("check_parallel was not run: {error}.")),
        };
        let started = Instant::now();
        let results = match scope.engine {
            None => arguments.specs.iter().map(check::not_signed_in).collect(),
            Some(engine) => {
                let options = RunOptions {
                    timeout_seconds: arguments.timeout_seconds,
                    fresh: arguments.fresh,
                    local_fallback: config::local_fallback_enabled() && !scope.http,
                    tool: "check_parallel",
                };
                match self
                    .run_with_progress(&engine, &scope.store, &arguments.specs, &options, context)
                    .await
                {
                    Some(results) => results,
                    None => return tool_error("check_parallel was cancelled before it finished."),
                }
            }
        };
        let wall = started.elapsed();
        let (text, truncated) = format::render_parallel(&results, wall, arguments.max_output_lines);
        structured_result(
            text,
            format::structured_parallel(&results, &truncated, wall),
        )
    }

    async fn tool_status(&self, scope: Scope, args: JsonObject) -> CallToolResult {
        if let Err(error) = known_arguments(&args, &[]) {
            return tool_error(format!("status takes no arguments ({error})."));
        }
        let status = Status::gather(
            &scope.settings,
            scope.engine.as_deref(),
            &scope.root,
            &scope.store,
        )
        .await;
        structured_result(status.text(), status.structured())
    }

    async fn tool_setup(
        &self,
        mut scope: Scope,
        args: JsonObject,
        context: &RequestContext<RoleServer>,
    ) -> CallToolResult {
        let parsed = known_arguments(&args, &["enable_remote_checks", "enable_more_languages"])
            .and_then(|()| {
                Ok((
                    arg_bool(&args, "enable_remote_checks")?,
                    arg_bool(&args, "enable_more_languages")?,
                ))
            });
        let (enable, more) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => return tool_error(format!("setup did nothing: {error}.")),
        };
        let mut said: Vec<String> = Vec::new();

        if scope.engine.is_none() {
            if scope.http {
                return structured_result(
                    "Not signed in: this server takes the account's key from each request. Send the header `Authorization: Bearer <your MachineStream API key>` and call setup again.",
                    json!({"ready": false, "state": "needs_authorization_header", "signed_in": false,
                        "next_step": "Send Authorization: Bearer <API key> with each request."}),
                );
            }
            let (pending, state) = match self.core.login.begin(&scope.settings.api_url).await {
                Ok(started) => started,
                Err(reason) => {
                    return tool_error(format!("setup could not start the sign-in: {reason}."))
                }
            };
            self.offer_sign_in_link(context, &pending);
            match self.wait_for_sign_in(state, context).await {
                LoginState::Done(email) => {
                    self.core.login.clear();
                    said.push(match email {
                        Some(email) => format!("Signed in as {email}."),
                        None => "Signed in.".to_string(),
                    });
                    scope = match self.scope(&context.extensions, &context.peer).await {
                        Ok(scope) => scope,
                        Err(error) => return tool_error(error.message.to_string()),
                    };
                }
                LoginState::Failed(reason) => {
                    self.core.login.clear();
                    return tool_error(format!(
                        "The sign-in did not complete: {reason}. Call setup to start again."
                    ));
                }
                LoginState::Waiting(_) | LoginState::Idle => {
                    return structured_result(
                        format!(
                            "Not signed in yet. {}\nThe approval is being watched for in the background, so the next setup call picks it up.",
                            setup::sign_in_text(&pending)
                        ),
                        json!({
                            "ready": false,
                            "state": "waiting_for_sign_in",
                            "signed_in": false,
                            "verification_uri": pending.verification_uri,
                            "user_code": pending.user_code,
                            "expires_in_seconds": pending.expires_in,
                            "next_step": "Give the user the link and the code, then call setup again.",
                        }),
                    );
                }
            }
        }
        let Some(engine) = scope.engine.clone() else {
            return tool_error(
                "Signed in, but the saved login could not be read. Call setup again.",
            );
        };
        if !said.is_empty() {
            // Just signed in: the account's catalog may add tools to what was listed.
            self.announce_changed_tools(&engine, &context.peer).await;
        }

        let checks = match engine.refresh_status().await {
            Ok(checks) => checks,
            Err(mcp_types::Error::Http {
                status: 401 | 403, ..
            }) => {
                return structured_result(
                    "The service did not accept this credential. Run `machinestream-mcp login` in a terminal (or correct MACHINESTREAM_API_KEY), then call setup again.",
                    json!({"ready": false, "state": "credential_rejected", "signed_in": false,
                        "next_step": "Run machinestream-mcp login, then call setup again."}),
                )
            }
            Err(error) => {
                return tool_error(format!(
                    "setup could not read the account's settings: {}. Try again shortly.",
                    status::short_error(&engine.redact(&error.to_string()))
                ))
            }
        };
        let (workspace, _) = engine.scope().await;
        let project =
            mcp_tools::domains::remote_checks::local_languages(Some(&scope.root), &checks);
        let (_, extra_names) = status::language_names(&checks);
        let first =
            status::first_check_for(&mcp_client::remote_checks::local_language_ids(&scope.root));

        // What to change: only what the caller asked for, by name.
        let turn_on = enable == Some(true) && !checks.enabled;
        let turn_on_more = more == Some(true) && checks.more_engines != Some(true);
        if !checks.enabled && !checks.available {
            return structured_result(
                [
                    said.join(" "),
                    "Remote checks are unavailable for this account. Run checks locally."
                        .to_string(),
                ]
                .join(" ")
                .trim()
                .to_string(),
                json!({"ready": false, "state": "unavailable", "signed_in": true,
                    "next_step": "Run checks locally."}),
            );
        }
        if !checks.enabled && !turn_on {
            let mut lines = said;
            lines.push("Remote checks are off for this account.".to_string());
            lines.push("Turning them on means: build, test, lint and type-check commands sent with `check` run remotely on a snapshot of this checkout, billed in Context Credits by run time.".to_string());
            lines.push(format!("Price: {}", checks.price_line()));
            if let Some(balance) = checks.balance_line() {
                lines.push(format!("Balance: {balance}."));
            }
            if let Some(project) = &project {
                lines.push(format!("This project: {}", project.summary()));
            }
            let extra = if extra_names.is_empty() {
                String::new()
            } else {
                format!(
                    " Add enable_more_languages: true for {}.",
                    extra_names.join(", ")
                )
            };
            lines.push(format!(
                "Next: ask the user, then call setup with enable_remote_checks: true to turn them on.{extra} Details: {}",
                config::PRICING_URL
            ));
            return structured_result(
                lines.join("\n"),
                json!({
                    "ready": false,
                    "state": "remote_checks_off",
                    "signed_in": true,
                    "remote_checks": "off",
                    "more_languages": checks.more_engines,
                    "price": checks.price_line(),
                    "credits_per_10s": checks.pricing.as_ref().map(|price| price.credits_per_10s),
                    "credits_available": checks.credits.as_ref().and_then(|credits| credits.available),
                    "next_step": "Ask the user, then call setup with enable_remote_checks: true.",
                }),
            );
        }
        let mut checks = checks;
        if turn_on || turn_on_more {
            let changed = mcp_tools::domains::remote_checks::settings(
                &engine.client,
                workspace.as_deref(),
                turn_on.then_some(true),
                turn_on_more.then_some(true),
            )
            .await;
            match changed {
                Ok(now) => {
                    engine.remember_status(&now);
                    if turn_on && now.enabled {
                        said.push("Remote checks are now on for this account.".to_string());
                    }
                    if turn_on_more && now.more_engines == Some(true) {
                        said.push("The extra languages are now on.".to_string());
                    }
                    checks = now;
                    self.announce_changed_tools(&engine, &context.peer).await;
                }
                Err(error) => {
                    return tool_error(format!(
                        "setup could not change the account's settings: {}. Nothing was turned on.",
                        status::short_error(&engine.redact(&error.to_string()))
                    ))
                }
            }
        }
        if !checks.enabled {
            return tool_error(
                "Remote checks are still off: the service did not apply the change. Try again shortly.",
            );
        }
        let checkout = status::Checkout::inspect(&scope.root).await;
        let mut lines = said;
        let ready = checkout.snapshot;
        lines.push(if ready {
            "Ready: remote checks are on and this checkout can be snapshotted.".to_string()
        } else {
            format!(
                "Remote checks are on, but no snapshot can be taken here: {}.",
                checkout
                    .problem
                    .as_deref()
                    .unwrap_or("this folder is not a git checkout")
            )
        });
        lines.push(format!("Price: {}", checks.price_line()));
        if let Some(balance) = checks.balance_line() {
            lines.push(format!("Balance: {balance}."));
        }
        if checks.more_engines != Some(true) && !extra_names.is_empty() {
            lines.push(format!(
                "Extra languages are off ({}). Call setup with enable_more_languages: true to add them.",
                extra_names.join(", ")
            ));
        }
        let next = if ready {
            format!("Call check with a command, for example `{first}`.")
        } else {
            "Start the server inside a git repository, then call check.".to_string()
        };
        lines.push(format!("Next: {next}"));
        structured_result(
            lines.join("\n"),
            json!({
                "ready": ready,
                "state": if ready { "ready" } else { "no_checkout" },
                "signed_in": true,
                "remote_checks": "on",
                "more_languages": checks.more_engines,
                "price": checks.price_line(),
                "credits_per_10s": checks.pricing.as_ref().map(|price| price.credits_per_10s),
                "credits_available": checks.credits.as_ref().and_then(|credits| credits.available),
                "next_step": next,
            }),
        )
    }

    /// Show the sign-in link through the client's own URL prompt, when it has one. The
    /// call does not wait on it: the approval is watched for either way.
    fn offer_sign_in_link(&self, context: &RequestContext<RoleServer>, pending: &setup::Pending) {
        use rmcp::service::ElicitationMode;
        let peer = context.peer.clone();
        if !peer
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Url)
        {
            return;
        }
        let Ok(url) = pending.verification_uri.parse::<url::Url>() else {
            return;
        };
        let message = format!(
            "Sign in to MachineStream to run checks remotely. Approve the code {}.",
            pending.user_code
        );
        let id = format!("machinestream-sign-in-{}", pending.user_code);
        tokio::spawn(async move {
            let _ = peer
                .elicit_url_with_timeout(message, url, id, Some(setup::wait_in_call()))
                .await;
        });
    }

    /// Wait for the approval inside the call, saying so now and then when the client asked
    /// for progress.
    async fn wait_for_sign_in(
        &self,
        state: tokio::sync::watch::Receiver<LoginState>,
        context: &RequestContext<RoleServer>,
    ) -> LoginState {
        let token = context.meta.get_progress_token();
        let started = Instant::now();
        loop {
            let left = setup::wait_in_call().saturating_sub(started.elapsed());
            if left.is_zero() {
                return state.borrow().clone();
            }
            let step = left.min(Duration::from_secs(5));
            let now = tokio::select! {
                now = setup::wait(state.clone(), step) => now,
                () = context.ct.cancelled() => return state.borrow().clone(),
            };
            if !matches!(now, LoginState::Waiting(_)) {
                return now;
            }
            if let Some(token) = &token {
                let _ = context
                    .peer
                    .notify_progress(
                        ProgressNotificationParam::new(
                            token.clone(),
                            started.elapsed().as_secs_f64(),
                        )
                        .with_total(setup::wait_in_call().as_secs_f64())
                        .with_message("waiting for the sign-in to be approved in the browser"),
                    )
                    .await;
            }
        }
    }

    fn tool_run_output(&self, store: &RunStore, args: JsonObject) -> CallToolResult {
        let (run_id, query) = match parse_output_arguments(&args) {
            Ok(parsed) => parsed,
            Err(error) => return tool_error(format!("run_output read nothing: {error}.")),
        };
        let record = match &run_id {
            Some(id) => match store.get(id) {
                Some(record) => record,
                None => {
                    return tool_error(format!(
                        "There is no saved run {id}. Runs are listed by status; the newest 50 are kept."
                    ))
                }
            },
            None => match store.latest_with_output() {
                Some(record) => record,
                None => {
                    return tool_error(
                        "There is no saved run with output yet. Run check first.",
                    )
                }
            },
        };
        let Some(text) = store.read_log(&record.id) else {
            return structured_result(
                format!(
                    "Run {} ({}: {}) has no saved output.",
                    record.id,
                    status::one_line(&record.command, 120),
                    record.result_word()
                ),
                json!({"run_id": record.id, "command": record.command, "result": record.result_word(),
                    "total_lines": 0, "shown_lines": 0, "matches": null, "truncated": false}),
            );
        };
        let selection = format::select_lines(&text, &query);
        let shown = selection.lines.iter().flatten().count();
        let mut header = format!(
            "Run {} ({}: {}, {}). Showing {shown} of {}",
            record.id,
            status::one_line(&record.command, 120),
            record.result_word(),
            format::age_text(&record.started_at, chrono::Utc::now()),
            format::lines_text(selection.total_lines)
        );
        if query.grep.is_some() {
            header.push_str(&format!(" ({} matching)", selection.matches));
        }
        header.push('.');
        if selection.truncated {
            header.push_str(
                " More were left out: narrow with grep, head or tail, or raise max_lines.",
            );
        }
        let body = format::render_selection(&selection);
        let log_path = store.log_path(&record.id);
        structured_result(
            if body.is_empty() {
                header
            } else {
                format!("{header}\n{body}")
            },
            json!({
                "run_id": record.id,
                "command": record.command,
                "result": record.result_word(),
                "total_lines": selection.total_lines,
                "shown_lines": shown,
                "matches": query.grep.as_ref().map(|_| selection.matches),
                "truncated": selection.truncated,
                "log_path": log_path.to_string_lossy(),
            }),
        )
    }

    /// Run a tool the account's catalog defines, through the engine.
    async fn tool_catalog(
        &self,
        scope: Scope,
        name: &str,
        args: JsonObject,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let unknown = || {
            McpError::invalid_params(
                format!(
                    "Unknown tool {name}. This server has: {}.",
                    catalog::BUILTIN.join(", ")
                ),
                None,
            )
        };
        let Some(engine) = scope.engine else {
            return Err(unknown());
        };
        let Some(tool) = engine.catalog_tool(name).await else {
            return Err(unknown());
        };
        let channel = Arc::new(crate::peer::ProgressChannel::new(context.peer.clone()));
        let token = context
            .meta
            .get_progress_token()
            .and_then(|token| serde_json::to_value(token).ok());
        let peer = mcp_tools::peer::CallPeer::new(channel, token, false);
        let run = mcp_tools::peer::run_with_call_peer(
            peer,
            mcp_tools::domains::hosted_tools::run(
                &engine.client,
                &engine.session,
                &tool,
                Value::Object(args),
            ),
        );
        let result = tokio::select! {
            result = run => result,
            () = context.ct.cancelled() => return Ok(tool_error(format!("{name} was cancelled before it finished."))),
        };
        Ok(crate::peer::convert_result(result))
    }
}

impl ServerHandler for MachineStream {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(
            Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION"))
                .with_title(SERVER_TITLE)
                .with_description(
                    "Remote builds, tests, lints and type checks for AI coding agents.",
                ),
        )
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.remember_peer(&context.peer);
        let scope = self.scope(&context.extensions, &context.peer).await?;
        let mut tools = builtin_tools();
        let mut listed = Value::Array(Vec::new()).to_string();
        if let Some(engine) = &scope.engine {
            let entries = self
                .core
                .catalog
                .for_listing(engine, self.on_catalog_change())
                .await;
            listed = Value::Array(entries.clone()).to_string();
            tools.extend(
                entries
                    .into_iter()
                    .filter_map(|entry| serde_json::from_value::<Tool>(entry).ok())
                    .filter(|tool| !catalog::is_builtin(&tool.name)),
            );
        }
        *self
            .core
            .last_listed
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(listed);
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.remember_peer(&context.peer);
        let scope = self.scope(&context.extensions, &context.peer).await?;
        let args = request.arguments.unwrap_or_default();
        let started = Instant::now();
        let result = match request.name.as_ref() {
            "check" => self.tool_check(scope, args, &context).await,
            "check_parallel" => self.tool_check_parallel(scope, args, &context).await,
            "status" => self.tool_status(scope, args).await,
            "setup" => self.tool_setup(scope, args, &context).await,
            "run_output" => self.tool_run_output(&scope.store, args),
            other => self.tool_catalog(scope, other, args, &context).await?,
        };
        tracing::debug!(
            tool = %request.name,
            seconds = started.elapsed().as_secs_f64(),
            is_error = result.is_error.unwrap_or(false),
            "tool call"
        );
        Ok(result.into())
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        builtin_tools().into_iter().find(|tool| tool.name == name)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let now = chrono::Utc::now();
        let scope = self.scope(&context.extensions, &context.peer).await?;
        let store = &scope.store;
        let resources = store
            .list()
            .into_iter()
            .filter(|run| store.log_path(&run.id).is_file())
            .map(|run| {
                Resource::new(
                    format!("{RUN_URI_PREFIX}{}", run.id),
                    status::one_line(&run.command, 80),
                )
                .with_title(format!("Run {}", run.id))
                .with_description(format!(
                    "{}, {}, {}",
                    run.result_word(),
                    run.place,
                    format::age_text(&run.started_at, now)
                ))
                .with_mime_type("text/plain")
                .with_size(run.bytes)
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(
            vec![ResourceTemplate::new(
                format!("{RUN_URI_PREFIX}{{run_id}}"),
                "run-output",
            )
            .with_title("Saved run output")
            .with_description(
                "The full output of a saved check, by its run id (see a check result or status).",
            )
            .with_mime_type("text/plain")],
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        /// A resource carries at most this much of a run (its newest part).
        const MAX_RESOURCE_BYTES: usize = 1 << 20;
        let not_found =
            || McpError::resource_not_found(format!("no saved run at {}", request.uri), None);
        let id = request
            .uri
            .strip_prefix(RUN_URI_PREFIX)
            .ok_or_else(not_found)?;
        let scope = self.scope(&context.extensions, &context.peer).await?;
        let mut text = scope.store.read_log(id).ok_or_else(not_found)?;
        if text.len() > MAX_RESOURCE_BYTES {
            let mut cut = text.len() - MAX_RESOURCE_BYTES;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            let cut = text[cut..]
                .find('\n')
                .map_or(cut, |offset| cut + offset + 1);
            text = format!(
                "[the first {cut} bytes are left out; the whole output is in the run's log file]\n{}",
                &text[cut..]
            );
        }
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri.clone())]).into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult::with_all_items(vec![verify_prompt()]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        if request.name != VERIFY_PROMPT {
            return Err(McpError::invalid_params(
                format!(
                    "Unknown prompt {}. This server has: {VERIFY_PROMPT}.",
                    request.name
                ),
                None,
            ));
        }
        let scope = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("scope"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(|scope| status::one_line(scope, 300));
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            verify_prompt_text(scope.as_deref()),
        )])
        .with_description(
            "Find this project's checks, run them remotely at once, and summarise the failures.",
        )
        .into())
    }
}

pub const VERIFY_PROMPT: &str = "verify-changes";

fn verify_prompt() -> Prompt {
    Prompt::new(
        VERIFY_PROMPT,
        Some("Find this project's checks, run them remotely at once with check_parallel, and summarise any failures with file and line."),
        Some(vec![PromptArgument::new("scope")
            .with_title("Scope")
            .with_description("What to verify, when not everything (for example: only the api package).")
            .with_required(false)]),
    )
    .with_title("Verify my changes")
}

pub fn verify_prompt_text(scope: Option<&str>) -> String {
    let mut text = String::from(
        "Verify the current changes in this repository with MachineStream.\n\
         1. Find this project's checks: read the build files (for example Cargo.toml, package.json scripts, pyproject.toml, go.mod, a Makefile) and any CI configuration to learn the lint, type-check and test commands this project really uses.\n\
         2. Run them in one call with the `check_parallel` tool (2 to 8 commands; for a single command use `check`). Leave out anything that deploys, publishes, changes files, starts a server or needs secrets.\n\
         3. If a command comes back as not run remotely, run it locally exactly as written.\n\
         4. Summarise the outcome: which checks passed, and for each failure the file and line, the message, and the fix you propose. When a result was truncated, read more with `run_output`.\n\
         Do not change any code until you have reported the failures.",
    );
    if let Some(scope) = scope {
        text.push_str(&format!("\nScope: {scope}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> JsonObject {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn every_tool_says_when_to_use_it_and_when_not_to() {
        let tools = builtin_tools();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            catalog::BUILTIN
        );
        for tool in &tools {
            let description = tool.description.as_deref().unwrap();
            assert!(description.contains("Use it"), "{}: when to use", tool.name);
            assert!(
                description.contains("Do not use")
                    || description.contains("use `")
                    || description.contains("It never"),
                "{}: when not to use",
                tool.name
            );
            assert!(
                !description.contains('\u{2014}'),
                "{}: no em dash",
                tool.name
            );
            assert!(tool.title.is_some(), "{}: a title", tool.name);
            let annotations = tool.annotations.as_ref().unwrap();
            assert!(annotations.read_only_hint.is_some());
            assert!(annotations.open_world_hint.is_some());
            assert!(annotations.idempotent_hint.is_some());
            assert_eq!(tool.input_schema.get("type"), Some(&json!("object")));
        }
        let read_only: Vec<&str> = tools
            .iter()
            .filter(|tool| tool.annotations.as_ref().unwrap().read_only_hint == Some(true))
            .map(|tool| tool.name.as_ref())
            .collect();
        assert_eq!(read_only, ["status", "run_output"]);
        for name in ["check", "check_parallel", "status", "run_output"] {
            let tool = tools.iter().find(|tool| tool.name == name).unwrap();
            assert!(tool.output_schema.is_some(), "{name}: an output schema");
        }
    }

    #[test]
    fn check_arguments_are_validated() {
        let parsed = parse_check_arguments(&args(json!({
            "command": "  cargo test  ", "cwd": "crates/api", "timeout_seconds": 120,
            "fresh": true, "max_output_lines": 50
        })))
        .unwrap();
        assert_eq!(parsed.spec.command, "cargo test");
        assert_eq!(parsed.spec.cwd.as_deref(), Some("crates/api"));
        assert_eq!(parsed.timeout_seconds, Some(120));
        assert!(parsed.fresh);
        assert_eq!(parsed.max_output_lines, Some(50));

        let minimal = parse_check_arguments(&args(json!({"command": "pytest"}))).unwrap();
        assert_eq!(minimal.spec.cwd, None);
        assert!(!minimal.fresh);
        assert_eq!(minimal.max_output_lines, None);

        for (bad, why) in [
            (json!({}), "command is required"),
            (json!({"command": "   "}), "command is required"),
            (json!({"command": 5}), "command must be a string"),
            (
                json!({"command": "x", "timeout_seconds": 0}),
                "timeout_seconds must be a whole number from 1 to 3600",
            ),
            (
                json!({"command": "x", "timeout_seconds": 99999}),
                "timeout_seconds must be",
            ),
            (
                json!({"command": "x", "timeout_seconds": 1.5}),
                "timeout_seconds must be",
            ),
            (
                json!({"command": "x", "fresh": "yes"}),
                "fresh must be true or false",
            ),
            (
                json!({"command": "x", "max_output_lines": 5000}),
                "max_output_lines must be",
            ),
            (
                json!({"command": "x", "comand": "y"}),
                "unknown argument comand",
            ),
            (json!({"command": "x".repeat(2001)}), "longer than 2000"),
        ] {
            let error = parse_check_arguments(&args(bad.clone())).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
    }

    #[test]
    fn a_whole_number_sent_as_a_float_is_accepted() {
        let parsed =
            parse_check_arguments(&args(json!({"command": "x", "timeout_seconds": 60.0}))).unwrap();
        assert_eq!(parsed.timeout_seconds, Some(60));
    }

    #[test]
    fn parallel_arguments_take_two_to_eight_commands() {
        let parsed = parse_parallel_arguments(&args(json!({
            "checks": [{"command": "cargo fmt --check"}, {"command": "cargo test", "cwd": "api"}, "cargo clippy"],
            "fresh": true
        })))
        .unwrap();
        assert_eq!(parsed.specs.len(), 3);
        assert_eq!(parsed.specs[1].cwd.as_deref(), Some("api"));
        assert_eq!(parsed.specs[2].command, "cargo clippy");
        assert!(parsed.fresh);

        for (bad, why) in [
            (json!({}), "checks is required"),
            (
                json!({"checks": [{"command": "x"}]}),
                "2 to 8 commands, not 1",
            ),
            (
                json!({"checks": (0..9).map(|_| json!({"command": "x"})).collect::<Vec<_>>()}),
                "2 to 8 commands, not 9",
            ),
            (
                json!({"checks": [{"command": "x"}, {}]}),
                "checks[1]: command is required",
            ),
            (
                json!({"checks": [{"command": "x"}, 7]}),
                "checks[1]: must be an object",
            ),
            (
                json!({"checks": [{"command": "x"}, {"command": "y", "dir": "z"}]}),
                "checks[1]: unknown argument dir",
            ),
        ] {
            let error = parse_parallel_arguments(&args(bad.clone())).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
    }

    #[test]
    fn run_output_arguments_are_validated() {
        let (run_id, query) = parse_output_arguments(&args(json!({
            "run_id": "20260101-000000-abcd", "grep": "error\\[", "context": 2, "tail": 5
        })))
        .unwrap();
        assert_eq!(run_id.as_deref(), Some("20260101-000000-abcd"));
        assert!(query.grep.unwrap().is_match("error[E0308]"));
        assert_eq!(query.context, 2);
        assert_eq!(query.tail, Some(5));
        assert_eq!(query.max_lines, 200);

        let (latest, _) = parse_output_arguments(&args(json!({"run_id": "latest"}))).unwrap();
        assert_eq!(latest, None);

        for (bad, why) in [
            (json!({"grep": "("}), "not a valid regular expression"),
            (json!({"head": 1, "tail": 1}), "head or tail, not both"),
            (json!({"context": 99}), "context must be"),
            (json!({"max_lines": 0}), "max_lines must be"),
            (json!({"lines": 3}), "unknown argument lines"),
        ] {
            let error = parse_output_arguments(&args(bad.clone())).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
    }

    #[test]
    fn the_instructions_are_short_and_say_the_rules() {
        assert!(INSTRUCTIONS.len() < 1200);
        for phrase in [
            "`check`",
            "`check_parallel`",
            "run it locally",
            "deploy, publish",
            "secrets",
        ] {
            assert!(INSTRUCTIONS.contains(phrase), "{phrase}");
        }
        assert!(!INSTRUCTIONS.contains('\u{2014}'));
    }

    #[test]
    fn the_prompt_names_the_tools_and_takes_a_scope() {
        let text = verify_prompt_text(Some("only the api package"));
        assert!(text.contains("`check_parallel`"));
        assert!(text.contains("file and line"));
        assert!(text.ends_with("Scope: only the api package"));
        assert!(!verify_prompt_text(None).contains("Scope:"));
    }
}
