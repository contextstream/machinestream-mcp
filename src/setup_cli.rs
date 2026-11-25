//! `machinestream-mcp setup`: from installed to ready in one go.
//!
//! It finds the MCP clients on this machine, connects the ones the person picks to this
//! binary, signs in through the browser, shows the price and the balance, and turns remote
//! checks on only after a yes. Answers are read from the terminal even when the command's
//! own input is a pipe (as when an install script runs it).

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::config::{self, Settings};
use crate::engine::Engine;
use crate::install::{self, Client, Launch, Outcome};
use crate::setup::{self, Login, LoginState};
use crate::status;

/// A file to read answers from in place of the terminal, for scripted setups.
pub const ENV_INPUT: &str = "MACHINESTREAM_SETUP_INPUT";

/// What `setup` was asked to do.
#[derive(Debug, Default, Clone)]
pub struct SetupArgs {
    /// Connect every client found without asking. Never turns on anything that costs.
    pub yes: bool,
    /// Connect exactly these clients.
    pub clients: Vec<Client>,
    /// Turn remote checks on without asking.
    pub enable_remote_checks: bool,
    /// Do not sign in.
    pub skip_login: bool,
    /// Print the sign-in link without opening a browser.
    pub no_browser: bool,
}

/// Where answers come from.
trait Ask {
    /// Show `prompt` and read one line. `None` when there is nobody to answer.
    fn ask(&mut self, prompt: &str) -> Option<String>;
}

struct Lines<R: BufRead, W: Write> {
    reader: R,
    writer: W,
    /// Write the answer after the prompt (input that is not typed leaves no trace of its own).
    echo: bool,
}

impl<R: BufRead, W: Write> Ask for Lines<R, W> {
    fn ask(&mut self, prompt: &str) -> Option<String> {
        let _ = write!(self.writer, "{prompt}");
        let _ = self.writer.flush();
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(read) if read > 0 => {
                let answer = line.trim().to_string();
                if self.echo {
                    let _ = writeln!(self.writer, "{answer}");
                }
                Some(answer)
            }
            _ => None,
        }
    }
}

/// The terminal to ask on: this process's input when it is one, else the terminal device
/// (so a script that came through a pipe can still ask). `None` when there is neither.
fn terminal() -> Option<Box<dyn Ask>> {
    if let Some(path) = std::env::var_os(ENV_INPUT) {
        let file = std::fs::File::open(path).ok()?;
        return Some(Box::new(Lines {
            reader: BufReader::new(file),
            writer: std::io::stdout(),
            echo: true,
        }));
    }
    if std::io::stdin().is_terminal() {
        return Some(Box::new(Lines {
            reader: BufReader::new(std::io::stdin()),
            writer: std::io::stdout(),
            echo: false,
        }));
    }
    let (input, output) = if cfg!(windows) {
        ("CONIN$", "CONOUT$")
    } else {
        ("/dev/tty", "/dev/tty")
    };
    let reader = std::fs::File::open(input).ok()?;
    let writer = std::fs::OpenOptions::new().write(true).open(output).ok()?;
    Some(Box::new(Lines {
        reader: BufReader::new(reader),
        writer,
        echo: false,
    }))
}

/// Which of `found` an answer picks: empty for all, `n` for none, or numbers like `1,3`.
pub fn pick(found: &[Client], answer: &str) -> Result<Vec<Client>, String> {
    let answer = answer.trim().to_ascii_lowercase();
    match answer.as_str() {
        "" | "a" | "all" | "y" | "yes" => return Ok(found.to_vec()),
        "n" | "no" | "none" | "0" => return Ok(Vec::new()),
        _ => {}
    }
    let mut picked = Vec::new();
    for word in answer.split([',', ' ']).filter(|word| !word.is_empty()) {
        let client = word
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| found.get(index))
            .ok_or_else(|| format!("{word} is not one of 1 to {}", found.len()))?;
        if !picked.contains(client) {
            picked.push(*client);
        }
    }
    Ok(picked)
}

fn yes(answer: Option<String>) -> bool {
    answer.is_some_and(|answer| matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// `path` with the home folder written as `~`.
fn short(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn step(ok: bool, text: &str) {
    println!("  [{}] {text}", if ok { "ok" } else { "--" });
}

/// Run the setup. Returns the process's exit status.
pub async fn run(root: &Path, args: SetupArgs) -> i32 {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let config_dir = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
    let mut asker = terminal();
    let scripted = args.yes || !args.clients.is_empty();
    println!("MachineStream setup");
    println!();

    // 1. Which clients to connect.
    let found: Vec<Client> = Client::ALL
        .into_iter()
        .filter(|client| install::detected(*client, &home, &config_dir, &install::on_path))
        .collect();
    let chosen: Vec<Client> = if !args.clients.is_empty() {
        args.clients.clone()
    } else if found.is_empty() {
        println!("  No MCP client was found on this machine.");
        println!("  Connect one later with: machinestream-mcp install <client>");
        Vec::new()
    } else {
        println!("  Clients found:");
        for (index, client) in found.iter().enumerate() {
            println!("    {}. {}", index + 1, client.label());
        }
        if args.yes {
            found.clone()
        } else {
            let Some(asker) = asker.as_mut() else {
                println!();
                println!("There is no terminal to ask on, so nothing was changed.");
                println!("Run `machinestream-mcp setup` in a terminal, or `machinestream-mcp setup --yes` to connect every client found.");
                return 1;
            };
            loop {
                let answer =
                    asker.ask("  Connect which? [Enter = all, numbers like 1,2, or n = none] ");
                let Some(answer) = answer else {
                    println!();
                    println!("No answer came, so nothing was changed.");
                    return 1;
                };
                match pick(&found, &answer) {
                    Ok(picked) => break picked,
                    Err(problem) => println!("  {problem}."),
                }
            }
        }
    };

    // 2. Connect them to this binary, by its own path.
    let mut failed = false;
    let binary = std::env::current_exe().ok();
    for client in &chosen {
        let Some(binary) = &binary else {
            step(
                false,
                &format!(
                    "{}: this program's own path could not be read",
                    client.label()
                ),
            );
            failed = true;
            continue;
        };
        let launch = if *client == Client::ClaudeDesktop && !in_checkout(root) {
            // No project folder to point at: the entry is written without one.
            Launch::binary(binary)
        } else {
            install::launch_for(*client, Launch::binary(binary), root)
        };
        let target = install::target(*client, root, &home, &config_dir, true);
        match install::install(&target, &launch) {
            Ok(Outcome::Written(path)) => step(
                true,
                &format!("{}: connected ({})", client.label(), short(&path, &home)),
            ),
            Ok(Outcome::AlreadySet(path)) => step(
                true,
                &format!(
                    "{}: already connected ({})",
                    client.label(),
                    short(&path, &home)
                ),
            ),
            Ok(Outcome::LeftAlone { path, reason, .. }) => {
                failed = true;
                step(
                    false,
                    &format!(
                        "{}: {} was left as it is ({reason}). Run `machinestream-mcp install {} --print` and add the entry by hand",
                        client.label(),
                        short(&path, &home),
                        client.id()
                    ),
                );
            }
            Err(error) => {
                failed = true;
                step(
                    false,
                    &format!("{}: could not be written ({error})", client.label()),
                );
            }
        }
        if *client == Client::ClaudeDesktop && !in_checkout(root) {
            println!("       Claude Desktop needs to know your project: run `machinestream-mcp install claude-desktop` inside it.");
        }
    }
    if !chosen.is_empty() {
        println!();
    }

    // 3. Sign in.
    let mut settings = Settings::resolve();
    if settings.signed_in() {
        step(true, "Signed in");
    } else if args.skip_login {
        step(
            false,
            "Not signed in (skipped). Sign in with: machinestream-mcp login",
        );
    } else {
        match sign_in(&settings.api_url, args.no_browser).await {
            Ok(Some(email)) => step(true, &format!("Signed in as {email}")),
            Ok(None) => step(true, "Signed in"),
            Err(reason) => {
                step(false, &format!("Sign-in did not complete: {reason}. Try again with: machinestream-mcp login"));
                failed = true;
            }
        }
        settings = Settings::resolve();
    }

    // 4. The price, the balance, and remote checks (only after a yes).
    let mut ready = false;
    if let Some(engine) = Engine::connect(&settings, root).await {
        match engine.refresh_status().await {
            Ok(checks) if checks.enabled => {
                step(true, "Remote checks: on");
                println!("       {}", checks.price_line());
                if let Some(balance) = checks.balance_line() {
                    println!("       {balance}.");
                }
                ready = true;
            }
            Ok(checks) if !checks.available => {
                step(false, "Remote checks are unavailable for this account");
            }
            Ok(checks) => {
                println!("  Remote checks are off for this account. Turning them on means checks your agent sends run remotely, billed in Context Credits by run time.");
                println!("       {}", checks.price_line());
                if let Some(balance) = checks.balance_line() {
                    println!("       {balance}.");
                }
                println!("       Details: {}", config::PRICING_URL);
                let agreed = args.enable_remote_checks
                    || (!scripted
                        && yes(asker
                            .as_mut()
                            .and_then(|asker| asker.ask("  Turn remote checks on now? [y/N] "))));
                if agreed {
                    let (workspace, _) = engine.scope().await;
                    match mcp_tools::domains::remote_checks::settings(
                        &engine.client,
                        workspace.as_deref(),
                        Some(true),
                        None,
                    )
                    .await
                    {
                        Ok(now) if now.enabled => {
                            step(true, "Remote checks: on");
                            ready = true;
                        }
                        Ok(_) => step(
                            false,
                            "Remote checks are still off: the change was not applied",
                        ),
                        Err(error) => step(
                            false,
                            &format!(
                                "Remote checks could not be turned on ({})",
                                status::short_error(&engine.redact(&error.to_string()))
                            ),
                        ),
                    }
                } else {
                    step(false, "Remote checks: off. Turn them on any time: ask your agent to set up MachineStream");
                }
            }
            Err(mcp_types::Error::Http {
                status: 401 | 403, ..
            }) => {
                step(false, "The service did not accept this login. Sign in again with: machinestream-mcp login");
                failed = true;
            }
            Err(error) => {
                step(
                    false,
                    &format!(
                        "MachineStream could not be reached ({})",
                        status::short_error(&engine.redact(&error.to_string()))
                    ),
                );
                failed = true;
            }
        }
    }

    println!();
    if ready && !chosen.is_empty() {
        println!("Ready. Restart your agent, open a project and say: \"Run my tests with MachineStream.\"");
    } else if ready {
        println!("Ready. Connect a client with `machinestream-mcp install <client>`, then say to your agent: \"Run my tests with MachineStream.\"");
    } else if settings.signed_in() {
        println!(
            "Almost ready: remote checks are off. In your agent, say: \"Set up MachineStream.\""
        );
    } else {
        println!("Almost ready: sign in with `machinestream-mcp login`, or say to your agent: \"Set up MachineStream.\"");
    }
    i32::from(failed)
}

fn in_checkout(folder: &Path) -> bool {
    folder
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

/// Sign in through the browser. The account's email when it could be read.
pub async fn sign_in(api_url: &str, no_browser: bool) -> Result<Option<String>, String> {
    let login = Login::new();
    let (pending, state) = login.begin(api_url).await?;
    println!("  Sign in: open {}", pending.verification_uri);
    println!("           and enter the code {}", pending.user_code);
    if !no_browser && open::that(&pending.verification_uri).is_ok() {
        println!("           (opened in your browser)");
    }
    let limit = std::time::Duration::from_secs(pending.expires_in.clamp(60, 1800));
    match setup::wait(state, limit).await {
        LoginState::Done(email) => Ok(email),
        LoginState::Failed(reason) => Err(reason),
        LoginState::Waiting(_) | LoginState::Idle => Err("it was not approved in time".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOUND: [Client; 3] = [Client::ClaudeCode, Client::Cursor, Client::Codex];

    #[test]
    fn enter_picks_every_client_found() {
        assert_eq!(pick(&FOUND, "").unwrap(), FOUND);
        assert_eq!(pick(&FOUND, " all ").unwrap(), FOUND);
        assert_eq!(pick(&FOUND, "Y").unwrap(), FOUND);
    }

    #[test]
    fn numbers_pick_some_and_n_picks_none() {
        assert_eq!(
            pick(&FOUND, "1,3").unwrap(),
            [Client::ClaudeCode, Client::Codex]
        );
        assert_eq!(pick(&FOUND, "2 2").unwrap(), [Client::Cursor]);
        assert!(pick(&FOUND, "n").unwrap().is_empty());
        assert!(pick(&FOUND, "none").unwrap().is_empty());
        assert_eq!(pick(&FOUND, "4").unwrap_err(), "4 is not one of 1 to 3");
        assert!(pick(&FOUND, "cursor").is_err());
    }

    #[test]
    fn only_a_plain_yes_agrees() {
        assert!(yes(Some("y".into())));
        assert!(yes(Some("YES".into())));
        assert!(!yes(Some(String::new())));
        assert!(!yes(Some("maybe".into())));
        assert!(!yes(None));
    }

    #[test]
    fn answers_are_read_line_by_line() {
        let mut shown = Vec::new();
        let mut asker = Lines {
            reader: BufReader::new("1,2\n\ny\n".as_bytes()),
            writer: &mut shown,
            echo: false,
        };
        assert_eq!(asker.ask("Which? ").as_deref(), Some("1,2"));
        assert_eq!(asker.ask("Sure? ").as_deref(), Some(""));
        assert_eq!(asker.ask("On? ").as_deref(), Some("y"));
        assert_eq!(asker.ask("More? "), None, "the end of input is no answer");
        assert_eq!(String::from_utf8(shown).unwrap(), "Which? Sure? On? More? ");
    }

    #[test]
    fn paths_under_home_are_shown_short() {
        let home = Path::new("/home/someone");
        assert_eq!(
            short(&home.join(".cursor/mcp.json"), home),
            "~/.cursor/mcp.json"
        );
        assert_eq!(short(Path::new("/etc/x"), home), "/etc/x");
    }
}
