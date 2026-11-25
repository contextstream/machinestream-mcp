//! Shared test support: a fake API and a fake stream server on localhost, a throwaway git
//! checkout, and a small MCP client that speaks JSON-RPC to the built binary over stdio.
//!
//! The fake service defines generic tools: `check` (answered on a stream) and `echo`
//! (answered by the API). What `check` does is chosen by its command:
//!
//! - `fixture echo <text>`: prints the text, exits 0. An exact repeat on the same tree is a cache hit.
//! - `fixture slow <ms> <label>`: prints `<label> tick N` every 50 ms for that long, exits 0.
//! - `fixture fail`: prints a compiler-style error and exits 101, with one extracted failure.
//! - `fixture refuse`, or any command holding `#refuse`: the stream declines it (exit 75).
//! - `fixture break`: prints some output, then drops the connection.
//! - `fixture credits`: the API answers that the account is out of credits.
//! - `fixture unsupported`: the API refuses the call.
//! - `fixture hostfault`: the run "fails", and the result says the failure was the host's.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Json;
use futures::{SinkExt, StreamExt};
use mcp_client::frames::{encode_frame, FrameBuffer};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

pub const KEY: &str = "fixture-key-alpha-0123456789";
pub const KEY_EMAIL: &str = "alpha@example.test";
pub const SECOND_KEY: &str = "fixture-key-beta-9876543210";
pub const SECOND_EMAIL: &str = "beta@example.test";
/// The key the fake device login hands out.
pub const MINTED_KEY: &str = "fixture-key-minted-5555555555";
pub const MINTED_EMAIL: &str = "minted@example.test";
const DEVICE_TOKEN: &str = "fixture-device-token";

/// What the fake service has seen and how it is set.
pub struct FakeState {
    stream_port: u16,
    pub enabled: AtomicBool,
    pub more: AtomicBool,
    /// Polls of the device login before it is approved.
    pub device_polls_left: AtomicU64,
    /// Bodies of calls to `check`.
    pub calls: Mutex<Vec<Value>>,
    /// The `hello` of every stream that was opened.
    pub hellos: Mutex<Vec<Value>>,
    /// Commands whose stream the client closed before the result.
    pub abandoned: Mutex<Vec<String>>,
    /// Routes asked for that the fake does not have.
    pub unknown: Mutex<Vec<String>>,
    /// Bodies of settings changes.
    pub settings_changes: Mutex<Vec<Value>>,
    seen: Mutex<HashSet<(String, String)>>,
    tokens: Mutex<HashMap<String, String>>,
    counter: AtomicU64,
}

pub struct Fake {
    pub api_url: String,
    pub state: Arc<FakeState>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn account_of(headers: &HeaderMap) -> Option<&'static str> {
    let bearer = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if bearer == Some(DEVICE_TOKEN) {
        return Some(MINTED_EMAIL);
    }
    match headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
    {
        Some(KEY) => Some(KEY_EMAIL),
        Some(SECOND_KEY) => Some(SECOND_EMAIL),
        Some(MINTED_KEY) => Some(MINTED_EMAIL),
        _ => None,
    }
}

type Answer = (StatusCode, Json<Value>);

fn ok(data: Value) -> Answer {
    (StatusCode::OK, Json(json!({"success": true, "data": data})))
}

fn refused(status: StatusCode, code: &str, message: &str) -> Answer {
    (
        status,
        Json(json!({"success": false, "error": {"code": code, "message": message}})),
    )
}

fn unauthorized() -> Answer {
    refused(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Invalid API key")
}

fn settings(state: &FakeState) -> Value {
    json!({
        "tool": "check",
        "enabled": state.enabled.load(Ordering::SeqCst),
        "available": true,
        "policy_locked": false,
        "more_engines": state.more.load(Ordering::SeqCst),
        "pricing": {"credits_per_10s": 20, "unit": "Context Credits", "billing": "run_time",
            "free": ["exact repeats", "checks that run on your machine", "checks that never start"]},
        "credits": {"available": 1000, "no_charge": false},
        "languages": [
            {"id": "rust", "name": "Rust", "tools": "cargo check", "more": false, "markers": ["Cargo.toml"]},
            {"id": "ruby", "name": "Ruby", "tools": "rspec", "more": true, "markers": ["Gemfile"]}
        ]
    })
}

fn catalog(state: &FakeState) -> Value {
    if !state.enabled.load(Ordering::SeqCst) {
        return json!({"tools": []});
    }
    json!({"tools": [
        {
            "name": "check",
            "title": "Check",
            "description": "Runs a command for this repository and returns what it prints and its exit status.",
            "input_schema": {"type": "object", "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "fresh": {"type": "boolean"},
                "timeout_seconds": {"type": "integer"}
            }, "required": ["command"], "additionalProperties": false},
            "annotations": {"readOnlyHint": false, "idempotentHint": true},
            "transport": {"kind": "stream"},
            "attachments": ["checkout_snapshot"],
            "command_hints": {"argument": "command", "cwd_argument": "cwd", "patterns": ["^fixture\\b"]}
        },
        {
            "name": "echo",
            "title": "Echo",
            "description": "Echoes its text back. A generic fixture tool.",
            "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
            "annotations": {"readOnlyHint": true},
            "transport": {"kind": "api"}
        }
    ]})
}

async fn get_catalog(State(state): State<Arc<FakeState>>, headers: HeaderMap) -> Answer {
    match account_of(&headers) {
        Some(_) => ok(catalog(&state)),
        None => unauthorized(),
    }
}

async fn get_settings(State(state): State<Arc<FakeState>>, headers: HeaderMap) -> Answer {
    match account_of(&headers) {
        Some(_) => ok(settings(&state)),
        None => unauthorized(),
    }
}

async fn put_settings(
    State(state): State<Arc<FakeState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Answer {
    if account_of(&headers).is_none() {
        return unauthorized();
    }
    state.settings_changes.lock().unwrap().push(body.clone());
    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        state.enabled.store(enabled, Ordering::SeqCst);
    }
    if let Some(more) = body.get("more_engines").and_then(Value::as_bool) {
        state.more.store(more, Ordering::SeqCst);
    }
    ok(settings(&state))
}

async fn get_more(State(state): State<Arc<FakeState>>, headers: HeaderMap) -> Answer {
    match account_of(&headers) {
        Some(_) => ok(json!({"enabled": state.more.load(Ordering::SeqCst)})),
        None => unauthorized(),
    }
}

async fn put_more(
    State(state): State<Arc<FakeState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Answer {
    if account_of(&headers).is_none() {
        return unauthorized();
    }
    state.settings_changes.lock().unwrap().push(body.clone());
    let more = body
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    state.more.store(more, Ordering::SeqCst);
    ok(json!({"enabled": more}))
}

async fn call_tool(
    State(state): State<Arc<FakeState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Answer {
    if account_of(&headers).is_none() {
        return unauthorized();
    }
    let arguments = body.get("arguments").cloned().unwrap_or(json!({}));
    match name.as_str() {
        "echo" => {
            let text = arguments.get("text").and_then(Value::as_str).unwrap_or("");
            ok(json!({
                "content": [{"type": "text", "text": format!("echo: {text}")}],
                "structuredContent": {"echoed": text},
                "isError": false
            }))
        }
        "check" => {
            state.calls.lock().unwrap().push(body.clone());
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if command.starts_with("fixture credits") {
                return refused(
                    StatusCode::PAYMENT_REQUIRED,
                    "INSUFFICIENT_CREDITS",
                    "Not enough credits",
                );
            }
            if command.starts_with("fixture unsupported") {
                return refused(
                    StatusCode::BAD_REQUEST,
                    "VALIDATION_ERROR",
                    "This command is not one remote checks take.",
                );
            }
            if arguments.get("fresh").and_then(Value::as_bool) == Some(true) {
                // A fresh run forgets the saved result for this command.
                state
                    .seen
                    .lock()
                    .unwrap()
                    .retain(|(_, seen)| *seen != command);
            }
            let number = state.counter.fetch_add(1, Ordering::SeqCst);
            let token = format!("call-token-{number}");
            state
                .tokens
                .lock()
                .unwrap()
                .insert(token.clone(), command.clone());
            let repo = body
                .pointer("/checkout/repo/name")
                .and_then(Value::as_str)
                .unwrap_or("repo");
            ok(json!({
                "call": {"id": format!("00000000-0000-4000-8000-{number:012}")},
                "stream": {
                    "url": format!("ws://127.0.0.1:{}/stream", state.stream_port),
                    "token": token,
                    "protocol": "frames/1",
                    "request": {
                        "id": format!("call-{number}"),
                        "repo_key": format!("{repo}-00000000"),
                        "cwd": arguments.get("cwd").and_then(Value::as_str).unwrap_or(""),
                        "command": command,
                        "timeout_s": arguments.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(60),
                        "wait_s": 5,
                        "versions": [],
                        "env_names": []
                    }
                }
            }))
        }
        _ => refused(StatusCode::NOT_FOUND, "NOT_FOUND", "No such tool"),
    }
}

async fn me(headers: HeaderMap) -> Answer {
    match account_of(&headers) {
        Some(email) => ok(json!({
            "id": "11111111-1111-4111-8111-111111111111",
            "email": email,
            "name": "Fixture",
            "created_at": "2026-01-01T00:00:00Z"
        })),
        None => unauthorized(),
    }
}

async fn create_key(headers: HeaderMap) -> Answer {
    match account_of(&headers) {
        Some(_) => ok(json!({"id": "key-1", "secret_key": MINTED_KEY})),
        None => unauthorized(),
    }
}

async fn device_start() -> Json<Value> {
    Json(json!({
        "device_code": "fixture-device-code",
        "user_code": "FIXT-CODE",
        "verification_uri": "https://example.test/device?user_code=FIXT-CODE",
        "interval": 1,
        "expires_in": 600
    }))
}

async fn device_token(State(state): State<Arc<FakeState>>) -> Json<Value> {
    let left = state.device_polls_left.load(Ordering::SeqCst);
    if left > 0 {
        state.device_polls_left.store(left - 1, Ordering::SeqCst);
        return Json(json!({"status": "pending", "interval": 1}));
    }
    Json(json!({"status": "authorized", "access_token": DEVICE_TOKEN, "expires_in": 600}))
}

async fn unknown_route(
    State(state): State<Arc<FakeState>>,
    request: axum::extract::Request,
) -> Answer {
    state
        .unknown
        .lock()
        .unwrap()
        .push(format!("{} {}", request.method(), request.uri().path()));
    refused(StatusCode::NOT_FOUND, "NOT_FOUND", "No such route")
}

async fn send_frame<S>(sink: &mut S, kind: u8, payload: &[u8]) -> bool
where
    S: futures::Sink<Message> + Unpin,
{
    sink.send(Message::binary(encode_frame(kind, payload)))
        .await
        .is_ok()
}

/// Answer one stream: read the hello and the pack, then play the command's fixture.
#[allow(clippy::result_large_err)] // the handshake callback's type is the library's
async fn serve_stream(state: Arc<FakeState>, tcp: tokio::net::TcpStream) {
    let mut token = None;
    let callback =
        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
         response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            token = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::to_string);
            Ok(response)
        };
    let Ok(socket) = tokio_tungstenite::accept_hdr_async(tcp, callback).await else {
        return;
    };
    let (mut sink, mut stream) = socket.split();
    let known = token
        .as_ref()
        .and_then(|token| state.tokens.lock().unwrap().remove(token));
    if known.is_none() {
        let _ = sink.close().await;
        return;
    }
    let mut buffer = FrameBuffer::default();
    let mut hello: Option<Value> = None;
    let mut packed = false;
    while !(hello.is_some() && packed) {
        let Some(Ok(message)) = stream.next().await else {
            return;
        };
        if let Message::Binary(bytes) = message {
            buffer.push(&bytes);
        }
        while let Ok(Some((kind, payload))) = buffer.next_frame() {
            match kind {
                b'J' if hello.is_none() => hello = serde_json::from_slice(&payload).ok(),
                b'D' => packed = true,
                _ => {}
            }
        }
    }
    let hello = hello.unwrap_or(Value::Null);
    state.hellos.lock().unwrap().push(hello.clone());
    let command = hello
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tree = hello
        .get("target_tree")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let words: Vec<&str> = command.split_whitespace().collect();
    let result = |fields: Value| {
        let mut frame = json!({"type": "result", "id": hello.get("id")});
        for (key, value) in fields.as_object().into_iter().flatten() {
            frame[key] = value.clone();
        }
        serde_json::to_vec(&frame).unwrap()
    };

    if command.contains("#refuse") || command.starts_with("fixture refuse") {
        let frame = result(json!({"ran": false, "exit": 75,
            "reason": "the fixture service declines this command, so it runs in the session"}));
        send_frame(&mut sink, b'J', &frame).await;
        return;
    }
    match words.get(1).copied() {
        Some("echo") => {
            let text = words[2..].join(" ");
            let first = state
                .seen
                .lock()
                .unwrap()
                .insert((tree.clone(), command.clone()));
            send_frame(&mut sink, b'O', format!("{text}\n").as_bytes()).await;
            let frame = if first {
                result(
                    json!({"ran": true, "exit": 0, "seconds": 0.5, "ran_seconds": 0.4, "cache": "miss"}),
                )
            } else {
                result(
                    json!({"ran": true, "exit": 0, "seconds": 0.01, "ran_seconds": 0.4, "cache": "hit"}),
                )
            };
            send_frame(&mut sink, b'J', &frame).await;
        }
        Some("slow") => {
            let millis: u64 = words
                .get(2)
                .and_then(|word| word.parse().ok())
                .unwrap_or(300);
            let label = words.get(3).copied().unwrap_or("slow");
            let started = Instant::now();
            let mut tick = 0;
            send_frame(&mut sink, b'J', br#"{"type":"info"}"#).await;
            while started.elapsed() < Duration::from_millis(millis) {
                tick += 1;
                let sent =
                    send_frame(&mut sink, b'O', format!("{label} tick {tick}\n").as_bytes()).await;
                let gone = tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(50)) => false,
                    message = stream.next() => matches!(message, None | Some(Err(_)) | Some(Ok(Message::Close(_)))),
                };
                if !sent || gone {
                    state.abandoned.lock().unwrap().push(command.clone());
                    return;
                }
            }
            let seconds = started.elapsed().as_secs_f64();
            let frame = result(
                json!({"ran": true, "exit": 0, "cache": "miss", "seconds": seconds, "ran_seconds": seconds}),
            );
            send_frame(&mut sink, b'J', &frame).await;
        }
        Some("fail") => {
            send_frame(
                &mut sink,
                b'O',
                b"error[E0001]: the fixture is broken\n --> src/lib.rs:1:1\n",
            )
            .await;
            let failures = json!([{"file": "src/lib.rs", "line": 1, "col": 1, "code": "E0001", "message": "the fixture is broken"}]);
            let frame = result(
                json!({"ran": true, "exit": 101, "seconds": 0.3, "ran_seconds": 0.25, "cache": "miss",
                "failures": failures, "failing_on_base": Value::Null}),
            );
            send_frame(&mut sink, b'J', &frame).await;
        }
        Some("stage") => {
            for step in 1..=2 {
                let info = json!({"type": "info", "stage": format!("part {step}"), "step": step, "total": 2});
                send_frame(&mut sink, b'J', &serde_json::to_vec(&info).unwrap()).await;
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            send_frame(&mut sink, b'O', b"both parts done\n").await;
            let frame = result(json!({"ran": true, "exit": 0, "seconds": 0.2, "ran_seconds": 0.1}));
            send_frame(&mut sink, b'J', &frame).await;
        }
        Some("hostfault") => {
            send_frame(&mut sink, b'O', b"Killed\n").await;
            let frame = result(json!({"ran": true, "exit": 137, "seconds": 0.2,
                "environment": {"detail": "the fixture host failed"}}));
            send_frame(&mut sink, b'J', &frame).await;
        }
        Some("break") => {
            send_frame(&mut sink, b'O', b"partial output before the break\n").await;
            let _ = sink.flush().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Dropped without a closing handshake.
            return;
        }
        _ => {
            let frame = result(json!({"ran": false, "exit": 75,
                "reason": "this command is not a fixture, so it runs in the session"}));
            send_frame(&mut sink, b'J', &frame).await;
        }
    }
    // The client closes once it has the result.
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
}

impl Fake {
    pub async fn start() -> Self {
        let stream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream_port = stream_listener.local_addr().unwrap().port();
        let state = Arc::new(FakeState {
            stream_port,
            enabled: AtomicBool::new(true),
            more: AtomicBool::new(false),
            device_polls_left: AtomicU64::new(1),
            calls: Mutex::default(),
            hellos: Mutex::default(),
            abandoned: Mutex::default(),
            unknown: Mutex::default(),
            settings_changes: Mutex::default(),
            seen: Mutex::default(),
            tokens: Mutex::default(),
            counter: AtomicU64::new(1),
        });
        let streams = {
            let state = state.clone();
            tokio::spawn(async move {
                while let Ok((tcp, _)) = stream_listener.accept().await {
                    tokio::spawn(serve_stream(state.clone(), tcp));
                }
            })
        };
        let api = axum::Router::new()
            .route("/api/v1/mcp/hosted-tools", get(get_catalog))
            .route(
                "/api/v1/mcp/hosted-tools/enabled",
                get(get_settings).put(put_settings),
            )
            .route(
                "/api/v1/mcp/hosted-tools/more-engines",
                get(get_more).put(put_more),
            )
            .route("/api/v1/mcp/hosted-tools/{name}/call", post(call_tool))
            .route("/api/v1/auth/me", get(me))
            .route("/api/v1/auth/api-keys", post(create_key))
            .route("/api/v1/auth/device/start", post(device_start))
            .route("/api/v1/auth/device/token", post(device_token))
            .fallback(unknown_route)
            .with_state(state.clone());
        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_url = format!("http://{}", api_listener.local_addr().unwrap());
        let serving = tokio::spawn(async move {
            let _ = axum::serve(api_listener, api).await;
        });
        Self {
            api_url,
            state,
            tasks: vec![streams, serving],
        }
    }

    pub fn hellos(&self) -> Vec<Value> {
        self.state.hellos.lock().unwrap().clone()
    }

    pub fn calls(&self) -> Vec<Value> {
        self.state.calls.lock().unwrap().clone()
    }
}

/// A throwaway home folder and git checkout.
pub struct Sandbox {
    _dir: tempfile::TempDir,
    pub home: PathBuf,
    pub project: PathBuf,
}

pub fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("fixture-project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("README.md"), "# fixture\n").unwrap();
        std::fs::write(project.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(&project, &["add", "-A"]);
        git(&project, &["commit", "-q", "-m", "A fixture project"]);
        Self {
            _dir: dir,
            home,
            project,
        }
    }

    pub fn cache(&self) -> PathBuf {
        self.home.join(".cache").join("machinestream")
    }

    /// A command for the built binary with a clean environment pointed at `fake`.
    pub fn command(&self, fake: &Fake, key: Option<&str>) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_machinestream-mcp"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("MACHINESTREAM_API_URL", &fake.api_url)
            .current_dir(&self.project)
            .kill_on_drop(true);
        if let Some(key) = key {
            command.env("MACHINESTREAM_API_KEY", key);
        }
        command
    }
}

/// A client speaking MCP to the server's stdin and stdout.
pub struct McpClient {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    inbox: tokio::sync::mpsc::UnboundedReceiver<Value>,
    next_id: u64,
    /// Notifications (and anything else that was not an answer) seen so far.
    pub notifications: Vec<Value>,
}

impl McpClient {
    /// Start the server and complete the handshake.
    pub async fn start(command: tokio::process::Command) -> Self {
        Self::start_with(command, json!({})).await
    }

    pub async fn start_with(command: tokio::process::Command, capabilities: Value) -> Self {
        let mut client = Self::spawn(command).await;
        let initialized = client
            .request(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": capabilities,
                    "clientInfo": {"name": "fixture-client", "version": "0"}}),
            )
            .await;
        assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
        client.notify("notifications/initialized", json!({})).await;
        client
    }

    /// Start the server without a handshake (the newest revision has none).
    pub async fn spawn(mut command: tokio::process::Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the server starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, inbox) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Everything on stdout must be protocol: a line that is not JSON fails the test.
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| json!({"not_json_on_stdout": line}));
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            inbox,
            next_id: 0,
            notifications: Vec::new(),
        }
    }

    pub async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// Send a request without waiting for its answer. Returns its id.
    pub async fn begin(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    /// Wait for the answer to request `id`, at most `limit`.
    pub async fn answer(&mut self, id: u64, limit: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let message = tokio::time::timeout_at(deadline, self.inbox.recv())
                .await
                .ok()??;
            assert!(
                message.get("not_json_on_stdout").is_none(),
                "stdout carried something that is not protocol: {message}"
            );
            if message.get("id").and_then(Value::as_u64) == Some(id)
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                return Some(message);
            }
            // A request from the server (roots, elicitation) is answered as unsupported.
            if let (Some(server_id), Some(_)) = (message.get("id"), message.get("method")) {
                let refusal = json!({"jsonrpc": "2.0", "id": server_id,
                    "error": {"code": -32601, "message": "not supported by the fixture client"}});
                self.send(refusal).await;
            }
            self.notifications.push(message);
        }
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.begin(method, params).await;
        self.answer(id, Duration::from_secs(60))
            .await
            .unwrap_or_else(|| panic!("no answer to {method}"))
    }

    /// Call a tool and return its result object.
    pub async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let answer = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        assert!(answer.get("error").is_none(), "{name}: {answer}");
        answer["result"].clone()
    }

    /// Call a tool asking for progress notifications under `token`.
    pub async fn call_with_progress(&mut self, name: &str, arguments: Value, token: &str) -> Value {
        let answer = self
            .request(
                "tools/call",
                json!({"name": name, "arguments": arguments, "_meta": {"progressToken": token}}),
            )
            .await;
        assert!(answer.get("error").is_none(), "{name}: {answer}");
        answer["result"].clone()
    }

    /// Progress messages received for `token`.
    pub fn progress(&self, token: &str) -> Vec<String> {
        self.notifications
            .iter()
            .filter(|note| note["method"] == "notifications/progress")
            .filter(|note| note["params"]["progressToken"] == token)
            .filter_map(|note| note["params"]["message"].as_str().map(str::to_string))
            .collect()
    }

    /// Wait until a notification of `method` has arrived.
    pub async fn wait_for_notification(&mut self, method: &str, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if self
                .notifications
                .iter()
                .any(|note| note["method"] == method)
            {
                return true;
            }
            match tokio::time::timeout_at(deadline, self.inbox.recv()).await {
                Ok(Some(message)) => self.notifications.push(message),
                _ => return false,
            }
        }
    }

    pub async fn stop(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

/// The text of a tool result.
pub fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
