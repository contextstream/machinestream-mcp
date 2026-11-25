//! The CLI and the Streamable HTTP transport against the fake service.

mod support;

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use support::{Fake, Sandbox, KEY, KEY_EMAIL, MINTED_EMAIL, MINTED_KEY, SECOND_EMAIL, SECOND_KEY};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

async fn run_cli(sandbox: &Sandbox, fake: &Fake, key: Option<&str>, args: &[&str]) -> Ran {
    let output = sandbox
        .command(fake, key)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the binary runs");
    Ran {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[tokio::test]
async fn run_streams_the_output_and_ends_with_the_commands_status() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();

    let passed = run_cli(
        &sandbox,
        &fake,
        Some(KEY),
        &["run", "--", "fixture", "echo", "from", "the", "cli"],
    )
    .await;
    assert_eq!(passed.code, 0, "{}", passed.stderr);
    assert_eq!(passed.stdout, "from the cli\n");
    assert!(
        passed.stderr.contains("Passed (exit 0)"),
        "{}",
        passed.stderr
    );
    assert!(passed.stderr.contains("ran remotely"), "{}", passed.stderr);

    let failed = run_cli(&sandbox, &fake, Some(KEY), &["run", "--", "fixture fail"]).await;
    assert_eq!(failed.code, 101, "the command's own exit status");
    assert!(failed.stdout.contains("error[E0001]"));
    assert!(
        failed.stderr.contains("src/lib.rs:1:1"),
        "{}",
        failed.stderr
    );
}

#[tokio::test]
async fn run_falls_back_to_this_machine_unless_told_not_to() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();

    // A command the check tool does not take runs here, with nothing sent.
    let local = run_cli(
        &sandbox,
        &fake,
        Some(KEY),
        &["run", "--", "echo", "ran-here"],
    )
    .await;
    assert_eq!(local.code, 0);
    assert_eq!(local.stdout, "ran-here\n");
    assert!(fake.calls().is_empty());

    let held = run_cli(
        &sandbox,
        &fake,
        Some(KEY),
        &["run", "--no-local", "--", "echo", "ran-here"],
    )
    .await;
    assert_eq!(held.code, 75);
    assert_eq!(held.stdout, "");

    // A command the service turns away runs here after a line that says why.
    let refused = run_cli(
        &sandbox,
        &fake,
        Some(KEY),
        &["run", "--", "fixture echo x #refuse"],
    )
    .await;
    assert!(
        refused
            .stderr
            .contains("running it here instead (the fixture service declines this command)"),
        "{}",
        refused.stderr
    );
    let refused_held = run_cli(
        &sandbox,
        &fake,
        Some(KEY),
        &["run", "--no-local", "--", "fixture refuse"],
    )
    .await;
    assert_eq!(refused_held.code, 75);
    assert!(refused_held
        .stderr
        .contains("not running it here (--no-local)"));

    // Signed out, the command still runs here.
    let signed_out = run_cli(&sandbox, &fake, None, &["run", "--", "echo", "no-account"]).await;
    assert_eq!(signed_out.code, 0);
    assert_eq!(signed_out.stdout, "no-account\n");
}

#[tokio::test]
async fn status_and_doctor_report_readiness() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();

    let status = run_cli(&sandbox, &fake, Some(KEY), &["status", "--json"]).await;
    assert_eq!(status.code, 0, "{}", status.stderr);
    let value: Value = serde_json::from_str(&status.stdout).unwrap();
    assert_eq!(value["ready"], true);
    assert_eq!(value["account"], KEY_EMAIL);

    let signed_out = run_cli(&sandbox, &fake, None, &["status"]).await;
    assert_eq!(signed_out.code, 1);
    assert!(signed_out.stdout.contains("Next: Call setup to sign in."));

    std::fs::write(sandbox.project.join(".env"), "TOKEN=fixture\n").unwrap();
    std::fs::write(sandbox.project.join("notes.txt"), "scratch\n").unwrap();
    let doctor = run_cli(&sandbox, &fake, Some(KEY), &["doctor", "--explain-upload"]).await;
    assert_eq!(doctor.code, 0, "{}{}", doctor.stdout, doctor.stderr);
    for line in [
        "] git: git version",
        "] checkout: fixture-project, a snapshot can be taken",
        "] credentials: from MACHINESTREAM_API_KEY (fingerprint ",
        "] service: reachable",
        "] remote checks: on",
        "] catalog: check, echo",
        "What a check of this checkout would send (nothing was sent):",
        "Changed or new since the last commit: 1 file(s)",
        "+ notes.txt",
        "! .env (",
        "The last commit: 2 tracked file(s)",
    ] {
        assert!(
            doctor.stdout.contains(line),
            "missing {line:?} in:\n{}",
            doctor.stdout
        );
    }
    assert!(!doctor.stdout.contains(KEY), "the key is never printed");
    assert!(
        fake.calls().is_empty() && fake.hellos().is_empty(),
        "nothing was sent"
    );

    let broken = run_cli(&sandbox, &fake, None, &["doctor"]).await;
    assert_eq!(broken.code, 1);
    assert!(broken.stdout.contains("[FAIL] credentials: none"));
}

#[tokio::test]
async fn login_saves_the_shared_login_and_logout_forgets_it() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    let login = run_cli(&sandbox, &fake, None, &["login", "--no-browser"]).await;
    assert_eq!(login.code, 0, "{}{}", login.stdout, login.stderr);
    assert!(login
        .stdout
        .contains("https://example.test/device?user_code=FIXT-CODE"));
    assert!(login.stdout.contains("Code: FIXT-CODE"));
    assert!(login
        .stdout
        .contains(&format!("Signed in as {MINTED_EMAIL}.")));
    assert!(
        !login.stdout.contains(MINTED_KEY),
        "the key is never printed"
    );
    let saved = sandbox.home.join(".contextstream").join("credentials.json");
    let credentials: Value =
        serde_json::from_str(&std::fs::read_to_string(&saved).unwrap()).unwrap();
    assert_eq!(credentials["api_key"], MINTED_KEY);
    assert_eq!(
        credentials["api_url"],
        fake.api_url.as_str(),
        "a non-default API is remembered"
    );

    // The saved login is used when no key is in the environment.
    let status = run_cli(&sandbox, &fake, None, &["status", "--json"]).await;
    let value: Value = serde_json::from_str(&status.stdout).unwrap();
    assert_eq!(value["account"], MINTED_EMAIL);
    assert_eq!(value["credential_source"], "saved login");

    let logout = run_cli(&sandbox, &fake, None, &["logout"]).await;
    assert_eq!(logout.code, 0);
    assert!(!saved.exists());
    let again = run_cli(&sandbox, &fake, None, &["logout"]).await;
    assert!(again.stdout.contains("There was no saved login."));
}

#[tokio::test]
async fn install_writes_a_clients_configuration() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    let printed = run_cli(
        &sandbox,
        &fake,
        None,
        &["install", "codex", "--print", "--npx"],
    )
    .await;
    assert_eq!(printed.code, 0);
    assert!(printed.stdout.contains("[mcp_servers.machinestream]"));
    assert!(printed
        .stdout
        .contains("args = [\"-y\", \"@contextstream/machinestream-mcp\"]"));
    assert!(
        !sandbox.home.join(".codex").exists(),
        "--print writes nothing"
    );

    let written = run_cli(&sandbox, &fake, None, &["install", "cursor", "--npx"]).await;
    assert_eq!(written.code, 0, "{}", written.stderr);
    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(sandbox.project.join(".cursor/mcp.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        config,
        json!({"mcpServers": {"machinestream": {"command": "npx", "args": ["-y", "@contextstream/machinestream-mcp"]}}})
    );
    let again = run_cli(&sandbox, &fake, None, &["install", "cursor", "--npx"]).await;
    assert!(again.stdout.contains("already set up"));

    // Without --npx the entry starts this very binary.
    let binary = run_cli(
        &sandbox,
        &fake,
        None,
        &["install", "claude-code", "--print"],
    )
    .await;
    assert!(binary
        .stdout
        .contains(env!("CARGO_BIN_EXE_machinestream-mcp")));

    let version = run_cli(&sandbox, &fake, None, &["--version"]).await;
    assert_eq!(
        version.stdout.trim(),
        format!("machinestream-mcp {}", env!("CARGO_PKG_VERSION"))
    );
}

/// A server started with `serve --http` on a free port.
struct HttpServer {
    _child: tokio::process::Child,
    base: String,
    http: reqwest::Client,
}

impl HttpServer {
    async fn start(sandbox: &Sandbox, fake: &Fake, key: Option<&str>) -> Self {
        Self::start_with(sandbox, fake, key, &[]).await
    }

    async fn start_with(
        sandbox: &Sandbox,
        fake: &Fake,
        key: Option<&str>,
        env: &[(&str, &str)],
    ) -> Self {
        let mut child = sandbox
            .command(fake, key)
            .envs(env.iter().copied())
            .args(["serve", "--http", "127.0.0.1:0", "--root"])
            .arg(&sandbox.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let base = line
            .split_whitespace()
            .find_map(|word| word.strip_suffix("/mcp"))
            .unwrap_or_else(|| panic!("no address in {line:?}"))
            .to_string();
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Self {
            _child: child,
            base,
            http: reqwest::Client::new(),
        }
    }

    /// One JSON-RPC message to `/mcp`. Returns the session id header and the answer.
    async fn post(
        &self,
        session: Option<&str>,
        key: Option<&str>,
        root: Option<&str>,
        message: Value,
    ) -> (Option<String>, Value) {
        let mut request = self
            .http
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .body(message.to_string());
        if let Some(session) = session {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(key) = key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        if let Some(root) = root {
            request = request.header("X-MachineStream-Root", root);
        }
        let response = request.send().await.unwrap();
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response.text().await.unwrap();
        // The answer is JSON, or the last JSON `data:` line of an event stream.
        let answer = serde_json::from_str::<Value>(&body).ok().or_else(|| {
            body.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
                .rfind(|value| value.get("id").is_some())
        });
        (session, answer.unwrap_or(Value::Null))
    }

    async fn session(&self, key: Option<&str>) -> String {
        let (session, answer) = self
            .post(
                None,
                key,
                None,
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "fixture-http", "version": "0"}}}),
            )
            .await;
        assert_eq!(
            answer["result"]["serverInfo"]["name"], "machinestream",
            "{answer}"
        );
        let session = session.expect("a session id");
        self.post(
            Some(&session),
            key,
            None,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        session
    }

    async fn call(
        &self,
        session: &str,
        key: Option<&str>,
        root: Option<&str>,
        name: &str,
        arguments: Value,
    ) -> Value {
        self.post(
            Some(session),
            key,
            root,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}}),
        )
        .await
        .1
    }
}

#[tokio::test]
async fn http_serves_several_accounts_and_keeps_their_runs_apart() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    let server = HttpServer::start(&sandbox, &fake, None).await;

    let health: Value = server
        .http
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");

    let alpha = server.session(Some(KEY)).await;
    let beta = server.session(Some(SECOND_KEY)).await;
    let status = server
        .call(&alpha, Some(KEY), None, "status", json!({}))
        .await;
    assert_eq!(
        status["result"]["structuredContent"]["account"], KEY_EMAIL,
        "{status}"
    );
    assert_eq!(
        status["result"]["structuredContent"]["credential_source"],
        "request header"
    );
    let status = server
        .call(&beta, Some(SECOND_KEY), None, "status", json!({}))
        .await;
    assert_eq!(
        status["result"]["structuredContent"]["account"],
        SECOND_EMAIL
    );

    let check = server
        .call(
            &alpha,
            Some(KEY),
            None,
            "check",
            json!({"command": "fixture echo over http"}),
        )
        .await;
    assert_eq!(
        check["result"]["structuredContent"]["passed"], true,
        "{check}"
    );

    // One account's saved runs are not shown to another.
    let own = server
        .call(&alpha, Some(KEY), None, "run_output", json!({}))
        .await;
    assert!(own["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("over http"));
    let other = server
        .call(&beta, Some(SECOND_KEY), None, "run_output", json!({}))
        .await;
    assert_eq!(other["result"]["isError"], true);
    let other_status = server
        .call(&beta, Some(SECOND_KEY), None, "status", json!({}))
        .await;
    assert_eq!(
        other_status["result"]["structuredContent"]["recent_runs"],
        json!([])
    );

    // Without a key, tools answer with what to send; nothing is run for anyone.
    let anonymous = server.session(None).await;
    let setup = server
        .call(&anonymous, None, None, "setup", json!({}))
        .await;
    assert_eq!(
        setup["result"]["structuredContent"]["state"],
        "needs_authorization_header"
    );
    let unsigned = server
        .call(
            &anonymous,
            None,
            None,
            "check",
            json!({"command": "fixture echo x"}),
        )
        .await;
    assert_eq!(
        unsigned["result"]["structuredContent"]["reason_code"],
        "not_signed_in"
    );
}

#[tokio::test]
async fn http_keeps_sessions_inside_the_allowed_folders_and_never_runs_locally() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    std::fs::create_dir(sandbox.project.join("web")).unwrap();
    let server = HttpServer::start_with(
        &sandbox,
        &fake,
        Some(KEY),
        &[("MACHINESTREAM_LOCAL_FALLBACK", "1")],
    )
    .await;
    // The process's own key serves a request that carries none.
    let session = server.session(None).await;
    let status = server.call(&session, None, None, "status", json!({})).await;
    assert_eq!(status["result"]["structuredContent"]["account"], KEY_EMAIL);

    let inside = server
        .call(&session, None, Some("web"), "status", json!({}))
        .await;
    assert!(
        inside["result"]["structuredContent"]["checkout"]["folder"]
            .as_str()
            .unwrap()
            .ends_with("/web"),
        "{inside}"
    );
    for outside in ["/etc", "../..", "missing"] {
        let refused = server
            .call(&session, None, Some(outside), "status", json!({}))
            .await;
        assert_eq!(refused["error"]["code"], -32602, "{outside}: {refused}");
    }

    // Even with the local fallback switched on, a caller over HTTP never gets a command
    // run on the server's own machine.
    let refused = server
        .call(
            &session,
            None,
            None,
            "check",
            json!({"command": "echo on-the-server #refuse"}),
        )
        .await;
    assert_eq!(
        refused["result"]["structuredContent"]["ran"], false,
        "{refused}"
    );
    assert_eq!(refused["result"]["structuredContent"]["where"], "none");

    // A Host that is not this server's is refused before anything is read.
    let rebinding = server
        .http
        .post(format!("{}/mcp", server.base))
        .header("Host", "attacker.example")
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(rebinding.status(), 403);
}

/// `setup` with only the sandbox's own folders to find clients in (nothing on the PATH).
async fn run_setup(
    sandbox: &Sandbox,
    fake: &Fake,
    key: Option<&str>,
    args: &[&str],
    answers: Option<&str>,
) -> Ran {
    let empty = sandbox.home.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let input = sandbox.home.join("answers.txt");
    let mut command = sandbox.command(fake, key);
    command.env("PATH", &empty).arg("setup").args(args);
    match answers {
        Some(answers) => {
            std::fs::write(&input, answers).unwrap();
            command.env("MACHINESTREAM_SETUP_INPUT", &input);
        }
        // A path that cannot be opened stands for "no terminal".
        None => {
            command.env(
                "MACHINESTREAM_SETUP_INPUT",
                sandbox.home.join("no-terminal"),
            );
        }
    }
    let output = command.stdin(Stdio::null()).output().await.unwrap();
    Ran {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn entry_command(path: &std::path::Path) -> Value {
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    config["mcpServers"]["machinestream"]["command"].clone()
}

#[tokio::test]
async fn setup_yes_connects_every_client_found_and_enables_nothing() {
    let fake = Fake::start().await;
    fake.state
        .enabled
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.join(".cursor")).unwrap();
    std::fs::create_dir_all(sandbox.home.join(".codex")).unwrap();

    let ran = run_setup(&sandbox, &fake, Some(KEY), &["--yes"], None).await;
    assert_eq!(ran.code, 0, "{}{}", ran.stdout, ran.stderr);
    assert!(ran.stdout.contains("1. Cursor"), "{}", ran.stdout);
    assert!(ran.stdout.contains("2. Codex"), "{}", ran.stdout);
    assert!(
        ran.stdout
            .contains("[ok] Cursor: connected (~/.cursor/mcp.json)"),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout
            .contains("[ok] Codex: connected (~/.codex/config.toml)"),
        "{}",
        ran.stdout
    );
    // The entries start this very binary by its path, not through npx.
    assert_eq!(
        entry_command(&sandbox.home.join(".cursor/mcp.json")),
        env!("CARGO_BIN_EXE_machinestream-mcp")
    );
    let codex = std::fs::read_to_string(sandbox.home.join(".codex/config.toml")).unwrap();
    assert!(codex.contains("[mcp_servers.machinestream]"));
    assert!(codex.contains(env!("CARGO_BIN_EXE_machinestream-mcp")));
    // --yes never turns on anything that costs.
    assert!(
        ran.stdout
            .contains("Remote checks are off for this account."),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout.contains("Price: ") || ran.stdout.contains("Context Credits per 10 s"),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout.contains("Almost ready: remote checks are off."),
        "{}",
        ran.stdout
    );
    assert!(fake.state.settings_changes.lock().unwrap().is_empty());

    // A rerun changes nothing.
    let again = run_setup(&sandbox, &fake, Some(KEY), &["--yes"], None).await;
    assert!(
        again.stdout.contains("[ok] Cursor: already connected"),
        "{}",
        again.stdout
    );
}

#[tokio::test]
async fn setup_with_flags_connects_named_clients_and_enables_on_request() {
    let fake = Fake::start().await;
    fake.state
        .enabled
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let sandbox = Sandbox::new();
    let ran = run_setup(
        &sandbox,
        &fake,
        Some(KEY),
        &["--client", "windsurf,gemini-cli", "--enable-remote-checks"],
        None,
    )
    .await;
    assert_eq!(ran.code, 0, "{}{}", ran.stdout, ran.stderr);
    assert!(
        ran.stdout.contains("[ok] Windsurf: connected"),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout
            .contains("[ok] Gemini CLI: connected (~/.gemini/settings.json)"),
        "{}",
        ran.stdout
    );
    assert!(sandbox
        .home
        .join(".codeium/windsurf/mcp_config.json")
        .is_file());
    assert_eq!(
        fake.state.settings_changes.lock().unwrap().as_slice(),
        [json!({"enabled": true})]
    );
    assert!(
        ran.stdout.contains("[ok] Remote checks: on"),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout.trim_end().ends_with("Ready. Restart your agent, open a project and say: \"Run my tests with MachineStream.\""),
        "{}",
        ran.stdout
    );

    // Signed out and told to skip the sign-in: it says what is left.
    let skipped = run_setup(
        &sandbox,
        &fake,
        None,
        &["--client", "cursor", "--skip-login"],
        None,
    )
    .await;
    assert_eq!(skipped.code, 0);
    assert!(
        skipped.stdout.contains("Not signed in (skipped)"),
        "{}",
        skipped.stdout
    );
    assert!(
        skipped
            .stdout
            .contains("Almost ready: sign in with `machinestream-mcp login`"),
        "{}",
        skipped.stdout
    );
}

#[tokio::test]
async fn setup_without_a_terminal_says_so_and_changes_nothing() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.join(".cursor")).unwrap();
    let ran = run_setup(&sandbox, &fake, Some(KEY), &[], None).await;
    assert_eq!(ran.code, 1);
    assert!(
        ran.stdout
            .contains("There is no terminal to ask on, so nothing was changed."),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout.contains("machinestream-mcp setup --yes"),
        "{}",
        ran.stdout
    );
    assert!(!sandbox.home.join(".cursor/mcp.json").exists());
}

#[tokio::test]
async fn setup_asks_on_the_terminal_signs_in_and_enables_after_a_yes() {
    let fake = Fake::start().await;
    fake.state
        .enabled
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.join(".cursor")).unwrap();
    std::fs::create_dir_all(sandbox.home.join(".codex")).unwrap();
    // Answers: connect the second client only, then yes to remote checks.
    let ran = run_setup(&sandbox, &fake, None, &["--no-browser"], Some("2\ny\n")).await;
    assert_eq!(ran.code, 0, "{}{}", ran.stdout, ran.stderr);
    assert!(ran.stdout.contains("Connect which?"), "{}", ran.stdout);
    assert!(
        ran.stdout.contains("[ok] Codex: connected"),
        "{}",
        ran.stdout
    );
    assert!(
        !sandbox.home.join(".cursor/mcp.json").exists(),
        "only the picked client"
    );
    assert!(
        ran.stdout.contains("and enter the code FIXT-CODE"),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout
            .contains(&format!("[ok] Signed in as {MINTED_EMAIL}")),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout.contains("Turn remote checks on now? [y/N]"),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout.contains("[ok] Remote checks: on"),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout.contains("Ready. Restart your agent"),
        "{}",
        ran.stdout
    );
    assert!(!ran.stdout.contains(MINTED_KEY), "the key is never printed");

    // Enter at the remote-checks question is a no.
    let fake = Fake::start().await;
    fake.state
        .enabled
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.join(".cursor")).unwrap();
    let declined = run_setup(&sandbox, &fake, Some(KEY), &[], Some("\n\n")).await;
    assert_eq!(declined.code, 0, "{}", declined.stdout);
    assert!(
        declined.stdout.contains("[ok] Cursor: connected"),
        "{}",
        declined.stdout
    );
    assert!(
        declined.stdout.contains("[--] Remote checks: off."),
        "{}",
        declined.stdout
    );
    assert!(fake.state.settings_changes.lock().unwrap().is_empty());
}
