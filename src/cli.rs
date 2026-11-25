//! The commands people and CI run: `login`, `logout`, `status`, `run`, `doctor`, `install`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mcp_client::checkout_snapshot::CheckoutSnapshot;
use mcp_client::command_hints;
use mcp_tools::domains::hosted_tools::command_hints_for_run;

use crate::check::{self, CheckEvent, CheckSpec, RunOptions};
use crate::config::{self, Settings};
use crate::engine::{Engine, CHECK_TOOL};
use crate::format;
use crate::install::{self, Client, Launch, Outcome};
use crate::runlog::RunStore;
use crate::setup::{self, Login, LoginState};
use crate::status::{self, Status};

/// The exit status of a command that was handed back and not run (`--no-local`).
pub const EXIT_NOT_RUN: i32 = 75;

fn say(line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "machinestream: {line}");
}

/// `login`: sign in through the browser and save the login.
pub async fn login(no_browser: bool) -> i32 {
    let settings = Settings::resolve();
    if matches!(
        settings.source,
        Some(config::KeySource::Env | config::KeySource::EnvAlt)
    ) {
        println!(
            "Note: {} is set and takes precedence over a saved login.",
            settings.source.map_or("", |source| source.describe())
        );
    }
    let login = Login::new();
    let (pending, state) = match login.begin(&settings.api_url).await {
        Ok(started) => started,
        Err(reason) => {
            eprintln!("Sign-in failed: {reason}.");
            return 1;
        }
    };
    println!("Open this link to sign in to MachineStream:");
    println!("  {}", pending.verification_uri);
    println!("Code: {}", pending.user_code);
    if !no_browser && open::that(&pending.verification_uri).is_ok() {
        println!("(Opened in your browser.)");
    }
    println!("Waiting for the approval...");
    let limit = Duration::from_secs(pending.expires_in.clamp(60, 1800));
    match setup::wait(state, limit).await {
        LoginState::Done(email) => {
            let saved = crate::credentials::path()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            match email {
                Some(email) => println!("Signed in as {email}."),
                None => println!("Signed in."),
            }
            println!("The login is saved in {saved}.");
            println!("Next: machinestream-mcp status");
            0
        }
        LoginState::Failed(reason) => {
            eprintln!("Sign-in failed: {reason}.");
            1
        }
        LoginState::Waiting(_) | LoginState::Idle => {
            eprintln!("Sign-in timed out before it was approved. Run login again.");
            1
        }
    }
}

/// `logout`: forget the saved login.
pub fn logout() -> i32 {
    match crate::credentials::delete() {
        Ok(true) => println!("Signed out: the saved login was removed."),
        Ok(false) => println!("There was no saved login."),
        Err(error) => {
            eprintln!("The saved login could not be removed: {error}.");
            return 1;
        }
    }
    for name in [config::ENV_API_KEY, config::ENV_API_KEY_ALT] {
        if std::env::var_os(name).is_some() {
            println!("Note: {name} is still set in this environment and will be used.");
        }
    }
    0
}

/// `status`: the same look the `status` tool gives. Exits 0 when a check can be asked for.
pub async fn status(root: &Path, json: bool) -> i32 {
    let settings = Settings::resolve();
    let engine = Engine::connect(&settings, root).await;
    let status = Status::gather(&settings, engine.as_deref(), root, &RunStore::default()).await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status.structured()).unwrap_or_default()
        );
    } else {
        println!("{}", status.text());
    }
    i32::from(!status.ready())
}

/// Options of `run`.
pub struct RunArgs {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub fresh: bool,
    pub no_local: bool,
}

/// `run -- <command>`: run the command remotely with its output as it prints, and end with
/// the command's own exit status. A command that is not run remotely runs here, unless
/// `no_local`.
pub async fn run(root: &Path, args: RunArgs) -> i32 {
    let settings = Settings::resolve();
    let spec = CheckSpec {
        command: args.command.clone(),
        cwd: args.cwd.clone(),
    };
    let here = |reason: Option<&str>| {
        if args.no_local {
            say(&format!(
                "not run remotely{}; not running it here (--no-local)",
                reason
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default()
            ));
            return EXIT_NOT_RUN;
        }
        if let Some(reason) = reason {
            say(&format!("running it here instead ({reason})"));
        }
        run_here(&args.command, &local_folder(root, args.cwd.as_deref()))
    };
    let Some(engine) = Engine::connect(&settings, root).await else {
        return here(Some("not signed in; run machinestream-mcp login"));
    };
    if !takes_command(&engine, &args.command).await {
        // Not a command remote checks take (or they are off): it runs as it always did.
        return here(None);
    }
    let options = RunOptions {
        timeout_seconds: args.timeout_seconds,
        fresh: args.fresh,
        local_fallback: false,
        tool: "run",
    };
    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        let mut wrote = false;
        while let Some(event) = inbox.recv().await {
            match event {
                CheckEvent::Output { bytes, .. } => {
                    if !wrote {
                        say("running remotely on MachineStream (output as it prints)");
                        wrote = true;
                    }
                    let mut stdout = std::io::stdout().lock();
                    // A reader that went away is no reason to stop the run.
                    let _ = stdout.write_all(&bytes).and_then(|()| stdout.flush());
                }
                CheckEvent::Stage { message, .. } => say(&message),
                CheckEvent::Heartbeat { .. } | CheckEvent::LocalFallback { .. } => {}
            }
        }
    });
    let store = RunStore::default();
    let specs = [spec];
    let mut results = tokio::select! {
        results = check::run_checks(&engine, &store, &specs, &options, Some(events)) => results,
        _ = tokio::signal::ctrl_c() => {
            say("interrupted");
            return 130;
        }
    };
    let _ = printer.await;
    let result = results.remove(0);
    if result.ran {
        if let Some(report) = &result.diagnostic_report {
            eprintln!("{report}");
        }
        say(&format!(
            "{} (run {})",
            format::status_line(&result),
            result.run_id
        ));
        return result.exit_code.unwrap_or(1);
    }
    if let Some(next) = &result.next_step {
        say(next);
    }
    here(result.reason.as_deref())
}

/// Whether the catalog's check tool says it takes `command` (the engine's own hints).
async fn takes_command(engine: &Arc<Engine>, command: &str) -> bool {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return false;
    }
    let config = engine.client.config().await;
    let Some(key) = config.api_key.as_deref() else {
        return false;
    };
    let identity = command_hints::local_identity(key);
    command_hints_for_run(
        &engine.client,
        &engine.session,
        &identity,
        Duration::from_millis(2500),
    )
    .await
    .is_some_and(|cache| {
        cache
            .tool_for(command)
            .is_some_and(|tool| tool.name == CHECK_TOOL)
    })
}

fn local_folder(root: &Path, cwd: Option<&str>) -> PathBuf {
    match cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => root.join(cwd),
        None => root.to_path_buf(),
    }
}

/// Run the command here, as a shell would: its own streams, its own exit status.
pub fn run_here(command: &str, dir: &Path) -> i32 {
    let mut shell = if cfg!(windows) {
        let mut shell = std::process::Command::new("cmd");
        shell.arg("/C").arg(command);
        shell
    } else {
        let mut shell = std::process::Command::new("bash");
        shell.arg("-c").arg(command);
        shell
    };
    match shell.current_dir(dir).status() {
        Ok(status) => exit_code(status),
        Err(error) => {
            say(&format!("could not run it here: {error}"));
            127
        }
    }
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "ok  "
    } else {
        "FAIL"
    }
}

/// `doctor`: check each thing a remote check needs, and say what to do about what is missing.
pub async fn doctor(root: &Path, explain_upload: bool) -> i32 {
    let mut failed = false;
    let mut line = |ok: bool, text: String| {
        failed |= !ok;
        println!("[{}] {text}", mark(ok));
    };
    println!("machinestream-mcp {}", env!("CARGO_PKG_VERSION"));
    println!("Folder: {}", root.display());

    let git = std::process::Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    match &git {
        Some(version) => line(true, format!("git: {version}")),
        None => line(
            false,
            "git: not found (a snapshot of the checkout needs git)".to_string(),
        ),
    }

    let checkout = status::Checkout::inspect(root).await;
    match (&checkout.repo, &checkout.problem) {
        (Some(repo), _) => line(true, format!("checkout: {repo}, a snapshot can be taken")),
        (None, problem) => line(
            false,
            format!(
                "checkout: {} (run inside a git repository)",
                problem.as_deref().unwrap_or("no snapshot can be taken")
            ),
        ),
    }

    let settings = Settings::resolve();
    match (&settings.api_key, settings.source) {
        (Some(key), Some(source)) => line(
            true,
            format!(
                "credentials: from {} (fingerprint {})",
                source.describe(),
                config::key_fingerprint(key)
            ),
        ),
        _ => line(
            false,
            "credentials: none (run machinestream-mcp login, or set MACHINESTREAM_API_KEY)"
                .to_string(),
        ),
    }
    println!("       API: {}", settings.api_url);

    let engine = Engine::connect(&settings, root).await;
    if let Some(engine) = &engine {
        match engine.refresh_status().await {
            Ok(checks) => {
                line(true, "service: reachable".to_string());
                line(
                    checks.enabled,
                    if checks.enabled {
                        format!(
                            "remote checks: on (extra languages {})",
                            match checks.more_engines {
                                Some(true) => "on",
                                Some(false) => "off",
                                None => "unknown",
                            }
                        )
                    } else if checks.available {
                        "remote checks: off (ask your agent to call setup, or turn them on in the console)".to_string()
                    } else {
                        "remote checks: unavailable for this account".to_string()
                    },
                );
                println!("       {}", checks.price_line());
                if let Some(balance) = checks.balance_line() {
                    println!("       {balance}.");
                }
            }
            Err(mcp_types::Error::Http {
                status: 401 | 403, ..
            }) => line(
                false,
                "service: the credential was not accepted (run machinestream-mcp login)"
                    .to_string(),
            ),
            Err(error) => line(
                false,
                format!(
                    "service: not reachable ({})",
                    status::short_error(&engine.redact(&error.to_string()))
                ),
            ),
        }
        let entries = engine
            .catalog_entries(Duration::from_secs(8), |_| false)
            .await;
        let names: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry.get("name").and_then(serde_json::Value::as_str))
            .collect();
        line(
            names.contains(&CHECK_TOOL),
            if names.is_empty() {
                "catalog: no tools (remote checks are off, or the service is unreachable)"
                    .to_string()
            } else {
                format!("catalog: {}", names.join(", "))
            },
        );
    }

    let cache = config::cache_dir();
    let writable = std::fs::create_dir_all(&cache)
        .and_then(|()| tempfile::NamedTempFile::new_in(&cache).map(|_| ()))
        .is_ok();
    line(
        writable,
        format!("saved runs: {}", cache.join("runs").display()),
    );
    if config::local_fallback_enabled() {
        println!(
            "       local fallback: on ({}=1)",
            config::ENV_LOCAL_FALLBACK
        );
    }
    if let Some(path) = config::usage_log_path() {
        println!("       usage log: {}", path.display());
    }
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        line(
            false,
            format!(
                "switch: {}=off keeps every check on this machine",
                command_hints::ENV_SWITCH
            ),
        );
    }

    if explain_upload {
        println!();
        if let Err(reason) = explain_upload_of(root).await {
            line(false, format!("snapshot: {reason}"));
        }
    }
    if failed {
        println!("\nSomething is missing: see the FAIL lines above.");
    } else {
        println!("\nEverything a remote check needs is in place.");
    }
    i32::from(failed)
}

fn bytes_text(bytes: u64) -> String {
    match bytes {
        0..=999 => format!("{bytes} B"),
        1000..=999_999 => format!("{:.1} kB", bytes as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1} MB", bytes as f64 / 1e6),
        _ => format!("{:.2} GB", bytes as f64 / 1e9),
    }
}

fn git_lines(root: &Path, args: &[&str]) -> Vec<String> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split('\0')
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Take the snapshot a check would take and describe it. Nothing is sent anywhere.
async fn explain_upload_of(root: &Path) -> Result<(), String> {
    let snapshot = CheckoutSnapshot::capture(root)
        .await
        .map_err(|error| error.to_string())?;
    let pack = snapshot
        .initial_pack()
        .await
        .map_err(|error| error.to_string())?;
    let changed_bytes: u64 = snapshot
        .overlay
        .iter()
        .filter_map(|entry| std::fs::symlink_metadata(snapshot.root.join(&entry.path)).ok())
        .map(|metadata| metadata.len())
        .sum();
    println!("What a check of this checkout would send (nothing was sent):");
    println!(
        "  Repository: {} (named by its folder name and a digest of its origin, never the URL)",
        snapshot.repo_name
    );
    println!(
        "  With each check: the command, the folder inside the repository, time limits, the two tree ids below, and the checkout's path for mapping output"
    );
    println!("    last commit's tree: {}", snapshot.base_tree);
    println!("    this checkout's tree: {}", snapshot.target_tree);
    println!(
        "  Changed or new since the last commit: {} file(s), {} ({} packed); {} deleted",
        snapshot.overlay.len(),
        bytes_text(changed_bytes),
        bytes_text(pack.len() as u64),
        snapshot.deletes.len()
    );
    for entry in snapshot.overlay.iter().take(20) {
        println!("    + {}", entry.path);
    }
    if snapshot.overlay.len() > 20 {
        println!("    (+{} more)", snapshot.overlay.len() - 20);
    }
    for path in snapshot.deletes.iter().take(10) {
        println!("    - {path}");
    }
    if snapshot.withheld.is_empty() {
        println!("  Withheld (looks like a credential, or listed in .contextignore): none");
    } else {
        println!("  Withheld, never sent ({}):", snapshot.withheld.len());
        for item in snapshot.withheld.iter().take(20) {
            println!("    ! {} ({})", item.path, item.reason);
        }
    }
    let tracked = git_lines(&snapshot.root, &["ls-tree", "-r", "-l", "-z", "HEAD"]);
    let tracked_bytes: u64 = tracked
        .iter()
        .filter_map(|line| {
            line.split('\t')
                .next()?
                .split_whitespace()
                .nth(3)?
                .parse::<u64>()
                .ok()
        })
        .sum();
    println!(
        "  The last commit: {} tracked file(s), {}. Sent only when the service does not hold them yet (a first check of a repository can send most of them)",
        tracked.len(),
        bytes_text(tracked_bytes)
    );
    let ignored = git_lines(
        &snapshot.root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    );
    println!(
        "  Ignored by git, never sent: {} path(s){}",
        ignored.len(),
        if ignored.is_empty() {
            String::new()
        } else {
            format!(
                ": {}{}",
                ignored
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                if ignored.len() > 8 { ", ..." } else { "" }
            )
        }
    );
    println!(
        "  About this machine: operating system and architecture, CPU count, whether it runs as root, the time zone's name, and the first line of --version of the tools the command uses (user, home and host names removed)"
    );
    println!("  Environment variables: only the ones the service names for the command's language (never ones that look like credentials), and only when set");
    println!("  Limits: 4,000 changed files, 64 MiB in all, 16 MiB per file. Over a limit the check runs on this machine.");
    Ok(())
}

/// `install <client>`: write, or print, the client's MCP configuration.
pub fn install(root: &Path, client: Client, print: bool, global: bool, npx: bool) -> i32 {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let config_dir = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
    let base = match (npx, std::env::current_exe()) {
        (false, Ok(binary)) if std::env::var_os("MACHINESTREAM_MCP_LAUNCHER").is_none() => {
            Launch::binary(&binary)
        }
        _ => Launch::npx(),
    };
    let launch = install::launch_for(client, base, root);
    let target = install::target(client, root, &home, &config_dir, global);
    if print {
        println!("# {}: add to {}", client.label(), target.path.display());
        println!("{}", install::snippet(&target, &launch));
        return 0;
    }
    match install::install(&target, &launch) {
        Ok(Outcome::Written(path)) => {
            println!("{}: wrote {}", client.label(), path.display());
            if target.project_scoped {
                println!("This file belongs to the project. It holds no credential, so it can be committed.");
            }
            println!("Next: {}", client.after());
            0
        }
        Ok(Outcome::AlreadySet(path)) => {
            println!("{}: already set up in {}", client.label(), path.display());
            0
        }
        Ok(Outcome::LeftAlone {
            path,
            reason,
            snippet,
        }) => {
            eprintln!(
                "{}: {} was left as it is ({reason}). Add this to it by hand:",
                client.label(),
                path.display()
            );
            println!("{snippet}");
            1
        }
        Err(error) => {
            eprintln!(
                "{}: could not write the configuration: {error}",
                client.label()
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(bytes_text(0), "0 B");
        assert_eq!(bytes_text(312), "312 B");
        assert_eq!(bytes_text(1500), "1.5 kB");
        assert_eq!(bytes_text(2_500_000), "2.5 MB");
        assert_eq!(bytes_text(3_210_000_000), "3.21 GB");
    }

    #[test]
    fn a_local_run_ends_with_the_commands_own_status() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_here("exit 0", dir.path()), 0);
        assert_eq!(run_here("exit 7", dir.path()), 7);
    }

    #[test]
    fn the_local_folder_follows_cwd() {
        let root = Path::new("/work/project");
        assert_eq!(local_folder(root, None), root);
        assert_eq!(local_folder(root, Some("  ")), root);
        assert_eq!(local_folder(root, Some("web")), root.join("web"));
    }
}
