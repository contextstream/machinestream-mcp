//! The server over stdio against a fake API and a fake stream server on localhost.

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::{text_of, Fake, McpClient, Sandbox, KEY, MINTED_EMAIL, MINTED_KEY};

async fn signed_in() -> (Fake, Sandbox, McpClient) {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    let client = McpClient::start(sandbox.command(&fake, Some(KEY))).await;
    (fake, sandbox, client)
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn lists_its_own_tools_and_the_catalogs() {
    let (_fake, _sandbox, mut client) = signed_in().await;
    // The catalog is read in the background; a listing right after start may come first.
    let mut names = Vec::new();
    for _ in 0..20 {
        names = tool_names(&client.request("tools/list", json!({})).await);
        if names.len() > 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        names,
        [
            "check",
            "check_parallel",
            "status",
            "setup",
            "run_output",
            "echo"
        ],
        "the catalog's own check is not listed twice"
    );
    let list = client.request("tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let echo = tools.iter().find(|tool| tool["name"] == "echo").unwrap();
    assert_eq!(
        echo["description"], "Echoes its text back. A generic fixture tool.",
        "a catalog tool is listed as the API describes it"
    );
    assert_eq!(echo["title"], "Echo");
    assert_eq!(echo["annotations"]["readOnlyHint"], true);
    assert_eq!(echo["inputSchema"]["required"], json!(["text"]));
    let check = tools.iter().find(|tool| tool["name"] == "check").unwrap();
    assert_eq!(check["title"], "Run a check remotely");
    assert!(check["outputSchema"]["properties"]["billing"].is_object());
    client.stop().await;
}

#[tokio::test]
async fn calls_a_catalog_tool_through_the_engine() {
    let (_fake, _sandbox, mut client) = signed_in().await;
    let result = client.call("echo", json!({"text": "hello there"})).await;
    assert_eq!(text_of(&result), "echo: hello there");
    assert_eq!(result["structuredContent"]["echoed"], "hello there");
    assert_ne!(result["isError"], true);

    let unknown = client
        .request(
            "tools/call",
            json!({"name": "no_such_tool", "arguments": {}}),
        )
        .await;
    assert_eq!(unknown["error"]["code"], -32602);
    assert!(unknown["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown tool no_such_tool"));
    client.stop().await;
}

#[tokio::test]
async fn a_check_runs_remotely_and_is_saved() {
    let (fake, sandbox, mut client) = signed_in().await;
    // An uncommitted edit and an untracked file are part of what is checked.
    std::fs::write(
        sandbox.project.join("src/lib.rs"),
        "pub fn one() -> u8 { 2 }\n",
    )
    .unwrap();
    std::fs::write(sandbox.project.join("notes.txt"), "untracked\n").unwrap();

    let result = client
        .call(
            "check",
            json!({"command": "fixture echo hello from the fixture"}),
        )
        .await;
    let text = text_of(&result);
    assert!(
        text.starts_with("Passed (exit 0) in 0.5 s, ran remotely."),
        "{text}"
    );
    assert!(text.contains("hello from the fixture"));
    assert!(text.contains("Estimated cost: 1 Context Credits (0.4 s of command time)."));
    assert_ne!(result["isError"], true);

    let structured = &result["structuredContent"];
    assert_eq!(structured["ran"], true);
    assert_eq!(structured["exit_code"], 0);
    assert_eq!(structured["passed"], true);
    assert_eq!(structured["cached"], false);
    assert_eq!(structured["where"], "remote");
    assert_eq!(structured["seconds"], 0.5);
    assert_eq!(structured["truncated"], false);
    assert_eq!(structured["failures"], json!([]));
    assert_eq!(
        structured["billing"],
        json!({"credits_estimated": 1, "billable_seconds": 0.4, "free_reason": null,
            "estimate": true, "credits_per_10s": 20})
    );

    // The full output is on disk, private, under the run's id.
    let run_id = structured["run_id"].as_str().unwrap();
    let log = sandbox.cache().join("runs").join(format!("{run_id}.log"));
    assert_eq!(structured["log_path"], log.to_string_lossy().as_ref());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "hello from the fixture\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // The service was sent the command, who the repository is, and the checkout's trees.
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["arguments"],
        json!({"command": "fixture echo hello from the fixture"})
    );
    assert_eq!(calls[0]["checkout"]["repo"]["name"], "fixture-project");
    let hello = &fake.hellos()[0];
    assert_ne!(hello["base_tree"], hello["target_tree"]);
    let mut changed: Vec<&str> = hello["overlay"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[2].as_str().unwrap())
        .collect();
    changed.sort_unstable();
    assert_eq!(changed, ["notes.txt", "src/lib.rs"]);
    assert_eq!(structured["tree"], hello["target_tree"]);
    client.stop().await;
}

#[tokio::test]
async fn an_exact_repeat_is_cached_and_fresh_runs_again() {
    let (_fake, _sandbox, mut client) = signed_in().await;
    let first = client
        .call("check", json!({"command": "fixture echo again"}))
        .await;
    assert_eq!(first["structuredContent"]["cached"], false);

    let repeat = client
        .call("check", json!({"command": "fixture echo again"}))
        .await;
    let structured = &repeat["structuredContent"];
    assert_eq!(structured["cached"], true);
    assert_eq!(structured["passed"], true);
    assert_eq!(structured["billing"]["credits_estimated"], 0);
    assert_eq!(structured["billing"]["free_reason"], "cached");
    assert_eq!(structured["billing"]["billable_seconds"], 0.0);
    let text = text_of(&repeat);
    assert!(text.contains("from cache"), "{text}");
    assert!(text.contains("pass fresh: true to run it again"), "{text}");

    let fresh = client
        .call(
            "check",
            json!({"command": "fixture echo again", "fresh": true}),
        )
        .await;
    assert_eq!(fresh["structuredContent"]["cached"], false);
    assert_eq!(
        fresh["structuredContent"]["billing"]["credits_estimated"],
        1
    );
    client.stop().await;
}

#[tokio::test]
async fn a_failing_check_is_a_result_with_failures_not_a_tool_error() {
    let (_fake, _sandbox, mut client) = signed_in().await;
    let result = client
        .call("check", json!({"command": "fixture fail"}))
        .await;
    assert_ne!(result["isError"], true, "a failed check is a result");
    let structured = &result["structuredContent"];
    assert_eq!(structured["ran"], true);
    assert_eq!(structured["exit_code"], 101);
    assert_eq!(structured["passed"], false);
    assert_eq!(structured["failures"][0]["file"], "src/lib.rs");
    assert_eq!(structured["failures"][0]["line"], 1);
    assert_eq!(structured["failures"][0]["code"], "E0001");
    let text = text_of(&result);
    assert!(
        text.starts_with("Failed (exit 101) in 0.3 s, ran remotely."),
        "{text}"
    );
    let failures = text.find("Failures:").unwrap();
    let report = text
        .find("- src/lib.rs:1:1 [E0001] the fixture is broken")
        .unwrap();
    let output = text.find("Output:").unwrap();
    assert!(failures < report && report < output, "{text}");
    assert!(text.contains("error[E0001]: the fixture is broken"));
    assert!(text.ends_with("Next: fix the failures and run the check again."));
    client.stop().await;
}

#[tokio::test]
async fn progress_carries_the_newest_output_lines() {
    let (_fake, _sandbox, mut client) = signed_in().await;
    let result = client
        .call_with_progress(
            "check",
            json!({"command": "fixture slow 1900 build"}),
            "p-1",
        )
        .await;
    assert_eq!(result["structuredContent"]["passed"], true);
    let messages = client.progress("p-1");
    assert!(
        messages
            .iter()
            .any(|message| message.contains("build tick")),
        "{messages:?}"
    );
    // Each progress value is larger than the one before.
    let values: Vec<f64> = client
        .notifications
        .iter()
        .filter(|note| note["method"] == "notifications/progress")
        .filter_map(|note| note["params"]["progress"].as_f64())
        .collect();
    assert!(values.len() >= 2, "{values:?}");
    assert!(
        values.windows(2).all(|pair| pair[0] < pair[1]),
        "{values:?}"
    );

    // Without a token there are none.
    let before = client.notifications.len();
    client
        .call("check", json!({"command": "fixture slow 300 quiet"}))
        .await;
    assert_eq!(client.notifications.len(), before);

    let staged = client
        .call_with_progress("check", json!({"command": "fixture stage"}), "p-2")
        .await;
    assert_eq!(staged["structuredContent"]["passed"], true);
    assert!(
        client
            .progress("p-2")
            .iter()
            .any(|message| message.contains("[1/2] part 1")),
        "{:?}",
        client.progress("p-2")
    );
    client.stop().await;
}

#[tokio::test]
async fn cancelling_a_call_stops_the_remote_run() {
    let (fake, _sandbox, mut client) = signed_in().await;
    let id = client
        .begin(
            "tools/call",
            json!({"name": "check", "arguments": {"command": "fixture slow 20000 long"}}),
        )
        .await;
    // Wait until the run is under way.
    let started = Instant::now();
    while fake.hellos().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the run never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    client
        .notify(
            "notifications/cancelled",
            json!({"requestId": id, "reason": "the user moved on"}),
        )
        .await;
    // The run's stream is closed.
    let cancelled = Instant::now();
    while fake.state.abandoned.lock().unwrap().is_empty() {
        assert!(
            cancelled.elapsed() < Duration::from_secs(5),
            "the stream was still open 5 s after the cancellation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        fake.state.abandoned.lock().unwrap().as_slice(),
        ["fixture slow 20000 long"]
    );
    // The server goes on answering.
    let after = client
        .call("check", json!({"command": "fixture echo still here"}))
        .await;
    assert_eq!(after["structuredContent"]["passed"], true);
    client.stop().await;
}

#[tokio::test]
async fn a_turned_away_check_says_to_run_it_locally() {
    let (_fake, sandbox, mut client) = signed_in().await;
    std::fs::create_dir(sandbox.project.join("web")).unwrap();
    let result = client
        .call("check", json!({"command": "fixture refuse", "cwd": "web"}))
        .await;
    assert_ne!(result["isError"], true, "a routing answer is not an error");
    let structured = &result["structuredContent"];
    assert_eq!(structured["ran"], false);
    assert_eq!(structured["where"], "none");
    assert_eq!(structured["exit_code"], Value::Null);
    assert_eq!(structured["passed"], Value::Null);
    assert_eq!(
        structured["reason"], "the fixture service declines this command",
        "the service's reason, as it gave it"
    );
    assert_eq!(structured["run_locally"], "fixture refuse");
    assert_eq!(structured["cwd"], "web");
    assert_eq!(structured["billing"]["free_reason"], "not_run_remotely");
    assert_eq!(structured["billing"]["credits_estimated"], 0);
    let text = text_of(&result);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "Not run remotely: the fixture service declines this command."
    );
    assert_eq!(
        lines[1],
        "Run this command locally: fixture refuse (in the folder web)"
    );

    // The API itself refusing the call is the same kind of answer.
    let unsupported = client
        .call("check", json!({"command": "fixture unsupported"}))
        .await;
    assert_ne!(unsupported["isError"], true);
    assert_eq!(unsupported["structuredContent"]["ran"], false);
    assert_eq!(
        unsupported["structuredContent"]["reason"],
        "This command is not one remote checks take."
    );

    // A failure the host calls its own is never reported as the command's.
    let fault = client
        .call("check", json!({"command": "fixture hostfault"}))
        .await;
    assert_eq!(fault["structuredContent"]["ran"], false);
    assert_eq!(fault["structuredContent"]["reason_code"], "host_fault");
    assert!(text_of(&fault).contains("the fixture host failed"));

    // A folder outside the repository is refused before anything is sent.
    let outside = client
        .call(
            "check",
            json!({"command": "fixture echo x", "cwd": "../.."}),
        )
        .await;
    assert_eq!(outside["structuredContent"]["ran"], false);
    assert_eq!(outside["structuredContent"]["reason_code"], "invalid_cwd");
    client.stop().await;
}

#[tokio::test]
async fn out_of_credits_is_a_routing_answer_with_the_way_to_add_credits() {
    let (_fake, _sandbox, mut client) = signed_in().await;
    let result = client
        .call("check", json!({"command": "fixture credits"}))
        .await;
    assert_ne!(result["isError"], true);
    let structured = &result["structuredContent"];
    assert_eq!(structured["ran"], false);
    assert_eq!(structured["reason_code"], "out_of_credits");
    assert_eq!(structured["run_locally"], "fixture credits");
    let text = text_of(&result);
    assert!(
        text.starts_with(
            "Not run remotely: Your account is out of Context Credits for remote checks.\n\
             Run this command locally: fixture credits\n\
             Add credits at "
        ),
        "{text}"
    );
    client.stop().await;
}

#[tokio::test]
async fn signed_out_everything_names_the_next_step() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    let mut client = McpClient::start(sandbox.command(&fake, None)).await;

    let names = tool_names(&client.request("tools/list", json!({})).await);
    assert_eq!(
        names,
        ["check", "check_parallel", "status", "setup", "run_output"],
        "without an account only the server's own tools are listed"
    );

    let check = client
        .call("check", json!({"command": "fixture echo hi"}))
        .await;
    assert_ne!(check["isError"], true);
    assert_eq!(check["structuredContent"]["ran"], false);
    assert_eq!(check["structuredContent"]["reason_code"], "not_signed_in");
    let text = text_of(&check);
    assert!(
        text.contains("Run this command locally: fixture echo hi"),
        "{text}"
    );
    assert!(text.contains("Call setup to sign in."), "{text}");

    let status = client.call("status", json!({})).await;
    assert_eq!(status["structuredContent"]["ready"], false);
    assert_eq!(status["structuredContent"]["signed_in"], false);
    assert_eq!(
        status["structuredContent"]["next_step"],
        "Call setup to sign in."
    );
    assert!(fake.calls().is_empty(), "nothing was sent anywhere");
    client.stop().await;
}

#[tokio::test]
async fn a_stream_that_breaks_mid_run_is_reported_and_its_output_kept() {
    let (_fake, _sandbox, mut client) = signed_in().await;
    let result = client
        .call("check", json!({"command": "fixture break"}))
        .await;
    assert_ne!(result["isError"], true);
    let structured = &result["structuredContent"];
    assert_eq!(structured["ran"], false);
    assert_eq!(structured["reason_code"], "stream_ended");
    assert!(structured["reason"]
        .as_str()
        .unwrap()
        .starts_with("the remote run ended without a result"));
    assert_eq!(structured["run_locally"], "fixture break");
    // What arrived before the break is still readable.
    let log = structured["log_path"]
        .as_str()
        .expect("the partial output was saved");
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "partial output before the break\n"
    );
    // And the server is fine.
    let after = client
        .call("check", json!({"command": "fixture echo recovered"}))
        .await;
    assert_eq!(after["structuredContent"]["passed"], true);
    client.stop().await;
}

#[tokio::test]
async fn parallel_checks_share_one_snapshot_and_run_together() {
    let (fake, sandbox, mut client) = signed_in().await;
    std::fs::write(
        sandbox.project.join("src/lib.rs"),
        "pub fn one() -> u8 { 3 }\n",
    )
    .unwrap();
    let started = Instant::now();
    let result = client
        .call_with_progress(
            "check_parallel",
            json!({"checks": [
                {"command": "fixture slow 1500 lint"},
                {"command": "fixture slow 1500 types"},
                {"command": "fixture fail"},
                {"command": "fixture refuse"}
            ]}),
            "p-par",
        )
        .await;
    let wall = started.elapsed();
    assert!(
        wall < Duration::from_millis(2800),
        "two 1.5 s commands ran one after the other ({wall:?})"
    );
    assert_ne!(result["isError"], true);

    let text = text_of(&result);
    assert!(
        text.starts_with("2 passed, 1 failed, 1 not run remotely ("),
        "{text}"
    );
    // Failures first, then what was not run, then what passed.
    let failed = text.find("[3] fixture fail").unwrap();
    let refused = text.find("[4] fixture refuse").unwrap();
    let lint = text.find("[1] fixture slow 1500 lint").unwrap();
    assert!(failed < refused && refused < lint, "{text}");

    let structured = &result["structuredContent"];
    assert_eq!(structured["summary"]["passed"], 2);
    assert_eq!(structured["summary"]["failed"], 1);
    assert_eq!(structured["summary"]["not_run"], 1);
    let checks = structured["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 4);
    assert_eq!(
        checks[0]["command"], "fixture slow 1500 lint",
        "in the order given"
    );
    assert_eq!(checks[2]["exit_code"], 101);
    assert_eq!(checks[3]["ran"], false);
    let ids: std::collections::HashSet<&str> = checks
        .iter()
        .map(|check| check["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4, "each command has its own run");

    // Every stream carried the same snapshot of the checkout.
    let hellos = fake.hellos();
    assert_eq!(hellos.len(), 4);
    let trees: std::collections::HashSet<&str> = hellos
        .iter()
        .map(|hello| hello["target_tree"].as_str().unwrap())
        .collect();
    assert_eq!(trees.len(), 1, "one snapshot for the whole call");

    // Progress says which command a line belongs to.
    let messages = client.progress("p-par");
    assert!(
        messages
            .iter()
            .any(|message| message.starts_with("[1] ") || message.starts_with("[2] ")),
        "{messages:?}"
    );

    let bad = client
        .call(
            "check_parallel",
            json!({"checks": [{"command": "fixture echo only-one"}]}),
        )
        .await;
    assert_eq!(bad["isError"], true);
    assert!(text_of(&bad).contains("2 to 8 commands"));
    client.stop().await;
}

#[tokio::test]
async fn setup_signs_in_then_turns_checks_on_only_when_asked() {
    let fake = Fake::start().await;
    fake.state.enabled.store(false, Ordering::SeqCst);
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.project.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();
    let mut client = McpClient::start(sandbox.command(&fake, None)).await;
    assert_eq!(
        tool_names(&client.request("tools/list", json!({})).await).len(),
        5
    );

    // Not signed in: the sign-in is started, approved (by the fake) and picked up in the call.
    let first = client.call("setup", json!({})).await;
    let text = text_of(&first);
    assert!(
        text.starts_with(&format!("Signed in as {MINTED_EMAIL}.")),
        "{text}"
    );
    assert!(
        text.contains("Remote checks are off for this account."),
        "{text}"
    );
    assert!(
        text.contains("billed in Context Credits by run time"),
        "{text}"
    );
    assert!(
        text.contains("Price: 20 Context Credits per 10 s of run time."),
        "{text}"
    );
    assert!(text.contains("Balance: You have 1,000 credits."), "{text}");
    assert!(text.contains("enable_remote_checks: true"), "{text}");
    assert_eq!(first["structuredContent"]["state"], "remote_checks_off");
    assert!(
        fake.state.settings_changes.lock().unwrap().is_empty(),
        "nothing is turned on without the explicit argument"
    );

    // The login is saved where the shared store keeps it, privately.
    let saved = sandbox.home.join(".contextstream").join("credentials.json");
    let credentials: Value =
        serde_json::from_str(&std::fs::read_to_string(&saved).unwrap()).unwrap();
    assert_eq!(credentials["api_key"], MINTED_KEY);
    assert_eq!(credentials["email"], MINTED_EMAIL);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Asking again changes nothing.
    let again = client.call("setup", json!({})).await;
    assert_eq!(again["structuredContent"]["state"], "remote_checks_off");
    assert!(fake.state.settings_changes.lock().unwrap().is_empty());

    // With the explicit argument they are turned on, and the tool list is said to have changed.
    let enabled = client
        .call("setup", json!({"enable_remote_checks": true}))
        .await;
    let text = text_of(&enabled);
    assert!(
        text.contains("Remote checks are now on for this account."),
        "{text}"
    );
    assert!(text.contains("Ready: remote checks are on"), "{text}");
    assert!(
        text.contains("Next: Call check with a command, for example `cargo check`."),
        "{text}"
    );
    assert_eq!(enabled["structuredContent"]["ready"], true);
    assert_eq!(
        fake.state.settings_changes.lock().unwrap().as_slice(),
        [json!({"enabled": true})]
    );
    assert!(
        client
            .wait_for_notification("notifications/tools/list_changed", Duration::from_secs(5))
            .await,
        "the catalog gained a tool"
    );
    assert!(
        tool_names(&client.request("tools/list", json!({})).await).contains(&"echo".to_string())
    );

    // Already set up: says so. The extra languages are their own explicit switch.
    let ready = client.call("setup", json!({})).await;
    assert_eq!(ready["structuredContent"]["state"], "ready");
    assert!(text_of(&ready).contains("Extra languages are off (Ruby)"));
    let more = client
        .call("setup", json!({"enable_more_languages": true}))
        .await;
    assert!(text_of(&more).contains("The extra languages are now on."));
    assert_eq!(more["structuredContent"]["more_languages"], true);

    // And a check now runs.
    let check = client
        .call("check", json!({"command": "fixture echo set up"}))
        .await;
    assert_eq!(check["structuredContent"]["passed"], true);
    client.stop().await;
}

#[tokio::test]
async fn setup_returns_the_link_when_the_approval_has_not_come() {
    let fake = Fake::start().await;
    // The approval never comes within this test.
    fake.state.device_polls_left.store(10_000, Ordering::SeqCst);
    let sandbox = Sandbox::new();
    let mut command = sandbox.command(&fake, None);
    command.env("MACHINESTREAM_SETUP_WAIT_SECONDS", "2");
    let mut client = McpClient::start(command).await;
    let result = client.call("setup", json!({})).await;
    assert_ne!(result["isError"], true);
    let structured = &result["structuredContent"];
    assert_eq!(structured["state"], "waiting_for_sign_in");
    assert_eq!(structured["user_code"], "FIXT-CODE");
    assert_eq!(
        structured["verification_uri"],
        "https://example.test/device?user_code=FIXT-CODE"
    );
    let text = text_of(&result);
    assert!(
        text.contains("https://example.test/device?user_code=FIXT-CODE"),
        "{text}"
    );
    assert!(text.contains("the code FIXT-CODE"), "{text}");
    // A second call reuses the sign-in already under way.
    let again = client.call("setup", json!({})).await;
    assert_eq!(again["structuredContent"]["user_code"], "FIXT-CODE");
    client.stop().await;
}

#[tokio::test]
async fn status_reports_account_price_checkout_and_recent_runs() {
    let (_fake, sandbox, mut client) = signed_in().await;
    std::fs::write(
        sandbox.project.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();
    client
        .call("check", json!({"command": "fixture echo one"}))
        .await;
    client
        .call("check", json!({"command": "fixture fail"}))
        .await;
    let result = client.call("status", json!({})).await;
    let text = text_of(&result);
    assert!(text.starts_with("Ready: remote checks are on"), "{text}");
    assert!(
        text.contains("Account: alpha@example.test (MACHINESTREAM_API_KEY)"),
        "{text}"
    );
    assert!(
        text.contains("Price: 20 Context Credits per 10 s of run time."),
        "{text}"
    );
    assert!(text.contains("Balance: You have 1,000 credits."), "{text}");
    assert!(text.contains("Languages: Rust."), "{text}");
    assert!(text.contains("This checkout: fixture-project"), "{text}");
    assert!(text.contains("- fixture fail: failed (exit 101)"), "{text}");
    assert!(text.contains("- fixture echo one: passed"), "{text}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["ready"], true);
    assert_eq!(structured["account"], "alpha@example.test");
    assert_eq!(structured["credits_available"], 1000);
    assert_eq!(structured["recent_runs"].as_array().unwrap().len(), 2);
    assert_eq!(
        structured["recent_runs"][0]["command"], "fixture fail",
        "newest first"
    );
    assert_eq!(structured["checkout"]["snapshot"], true);
    client.stop().await;
}

#[tokio::test]
async fn saved_runs_are_read_without_running_again() {
    let (fake, _sandbox, mut client) = signed_in().await;
    let check = client
        .call("check", json!({"command": "fixture fail"}))
        .await;
    let run_id = check["structuredContent"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let calls_before = fake.calls().len();

    let latest = client.call("run_output", json!({})).await;
    assert_eq!(latest["structuredContent"]["run_id"], run_id.as_str());
    assert_eq!(latest["structuredContent"]["total_lines"], 2);
    let text = text_of(&latest);
    assert!(
        text.contains("1: error[E0001]: the fixture is broken"),
        "{text}"
    );

    let grep = client
        .call("run_output", json!({"run_id": run_id, "grep": "-->"}))
        .await;
    assert_eq!(grep["structuredContent"]["matches"], 1);
    assert_eq!(grep["structuredContent"]["shown_lines"], 1);
    assert!(text_of(&grep).contains("2:  --> src/lib.rs:1:1"));

    let missing = client
        .call("run_output", json!({"run_id": "20200101-000000-none"}))
        .await;
    assert_eq!(missing["isError"], true);
    assert_eq!(
        fake.calls().len(),
        calls_before,
        "reading a run runs nothing"
    );

    // The same run is a resource.
    let resources = client.request("resources/list", json!({})).await;
    let uri = format!("machinestream://runs/{run_id}");
    assert_eq!(resources["result"]["resources"][0]["uri"], uri.as_str());
    let read = client.request("resources/read", json!({"uri": uri})).await;
    assert_eq!(
        read["result"]["contents"][0]["text"],
        "error[E0001]: the fixture is broken\n --> src/lib.rs:1:1\n"
    );
    let templates = client.request("resources/templates/list", json!({})).await;
    assert_eq!(
        templates["result"]["resourceTemplates"][0]["uriTemplate"],
        "machinestream://runs/{run_id}"
    );
    let absent = client
        .request(
            "resources/read",
            json!({"uri": "machinestream://runs/../../etc/passwd"}),
        )
        .await;
    assert!(absent["error"].is_object());
    client.stop().await;
}

#[tokio::test]
async fn the_local_fallback_is_opt_in() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    // Off by default: a turned-away command is not run here.
    let mut client = McpClient::start(sandbox.command(&fake, Some(KEY))).await;
    let off = client
        .call(
            "check",
            json!({"command": "echo ran-on-this-machine #refuse"}),
        )
        .await;
    assert_eq!(off["structuredContent"]["ran"], false);
    client.stop().await;

    let mut command = sandbox.command(&fake, Some(KEY));
    command.env("MACHINESTREAM_LOCAL_FALLBACK", "1");
    let mut client = McpClient::start(command).await;
    let on = client
        .call(
            "check",
            json!({"command": "echo ran-on-this-machine #refuse"}),
        )
        .await;
    let structured = &on["structuredContent"];
    assert_eq!(structured["ran"], true);
    assert_eq!(structured["where"], "local");
    assert_eq!(structured["exit_code"], 0);
    assert_eq!(structured["billing"]["free_reason"], "ran_locally");
    let text = text_of(&on);
    assert!(
        text.starts_with("Passed (exit 0) in ") && text.contains("ran locally (MachineStream turned it away: the fixture service declines this command)"),
        "{text}"
    );
    assert!(text.contains("ran-on-this-machine"));
    client.stop().await;
}

#[tokio::test]
async fn the_usage_log_has_one_line_per_check() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    let usage = sandbox.home.join("usage.jsonl");
    let mut command = sandbox.command(&fake, Some(KEY));
    command.env("MACHINESTREAM_USAGE_LOG", &usage);
    let mut client = McpClient::start(command).await;
    client
        .call("check", json!({"command": "fixture echo billed"}))
        .await;
    client
        .call("check", json!({"command": "fixture echo billed"}))
        .await;
    client
        .call("check", json!({"command": "fixture refuse"}))
        .await;
    client.stop().await;

    let lines: Vec<Value> = std::fs::read_to_string(&usage)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["tool"], "check");
    assert_eq!(lines[0]["credits_estimated"], 1);
    assert_eq!(lines[0]["billable_seconds"], 0.4);
    assert_eq!(lines[0]["cached"], false);
    assert_eq!(lines[0]["ran"], true);
    assert_eq!(lines[1]["cached"], true);
    assert_eq!(lines[1]["credits_estimated"], 0);
    assert_eq!(lines[2]["ran"], false);
    assert_eq!(lines[2]["credits_estimated"], 0);
    let fingerprint = lines[0]["key_fingerprint"].as_str().unwrap();
    assert_eq!(fingerprint.len(), 12);
    let raw = std::fs::read_to_string(&usage).unwrap();
    assert!(!raw.contains(KEY), "the key is never written");
    assert!(lines
        .iter()
        .all(|line| line["run_id"].is_string() && line["time"].is_string()));
}

#[tokio::test]
async fn the_prompt_and_the_handshake_are_served() {
    let (_fake, _sandbox, mut client) = signed_in().await;
    let prompts = client.request("prompts/list", json!({})).await;
    assert_eq!(prompts["result"]["prompts"][0]["name"], "verify-changes");
    let prompt = client
        .request(
            "prompts/get",
            json!({"name": "verify-changes", "arguments": {"scope": "only the api package"}}),
        )
        .await;
    let text = prompt["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("check_parallel"));
    assert!(text.ends_with("Scope: only the api package"));
    client.stop().await;
}

#[tokio::test]
async fn older_and_newer_protocol_revisions_are_negotiated() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    for (asked, agreed) in [
        ("2024-11-05", "2024-11-05"),
        ("2025-03-26", "2025-03-26"),
        ("2025-11-25", "2025-11-25"),
        // A revision this server does not know is answered with one it does.
        ("2099-01-01", "2025-11-25"),
    ] {
        let mut command = sandbox.command(&fake, Some(KEY));
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": asked, "capabilities": {}, "clientInfo": {"name": "fixture", "version": "0"}}});
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let answer: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(answer["result"]["protocolVersion"], agreed, "asked {asked}");
        assert_eq!(answer["result"]["serverInfo"]["name"], "machinestream");
    }
}

#[tokio::test]
async fn the_current_revision_works_without_a_handshake() {
    let fake = Fake::start().await;
    let sandbox = Sandbox::new();
    let mut client = McpClient::spawn(sandbox.command(&fake, Some(KEY))).await;
    // The revision of 2026-07-28 has no initialize: each request says who is asking.
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "fixture-stateless", "version": "0"}
    });
    let discover = client
        .request("server/discover", json!({"_meta": meta}))
        .await;
    let versions = discover["result"]["supportedVersions"].as_array().unwrap();
    assert!(versions.contains(&json!("2026-07-28")), "{discover}");
    assert!(versions.contains(&json!("2024-11-05")), "{discover}");
    assert!(discover["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("`check`"));

    let list = client.request("tools/list", json!({"_meta": meta})).await;
    assert!(tool_names(&list).starts_with(&[
        "check".to_string(),
        "check_parallel".to_string(),
        "status".to_string(),
        "setup".to_string(),
        "run_output".to_string()
    ]));

    let mut with_progress = meta.clone();
    with_progress["progressToken"] = json!("p-new");
    let answer = client
        .request(
            "tools/call",
            json!({"name": "check", "arguments": {"command": "fixture slow 1200 newest"}, "_meta": with_progress}),
        )
        .await;
    let result = &answer["result"];
    assert_eq!(result["resultType"], "complete", "{answer}");
    assert_eq!(result["structuredContent"]["passed"], true);
    assert!(
        client
            .progress("p-new")
            .iter()
            .any(|message| message.contains("newest tick")),
        "{:?}",
        client.progress("p-new")
    );

    // A request for a revision nobody knows is refused, not guessed at.
    let mut unknown = meta.clone();
    unknown["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let refused = client
        .request("tools/list", json!({"_meta": unknown}))
        .await;
    assert!(refused["error"].is_object(), "{refused}");
    client.stop().await;
}
