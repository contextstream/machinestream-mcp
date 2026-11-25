use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use machinestream_mcp::cli;
use machinestream_mcp::config;
use machinestream_mcp::http::{self, HttpOptions};
use machinestream_mcp::install::Client;
use machinestream_mcp::server::{MachineStream, Roots};
use machinestream_mcp::setup_cli;
use rmcp::ServiceExt;

/// MCP server for MachineStream: remote builds, tests, lints and type checks for AI coding
/// agents. With no subcommand it speaks MCP on stdin and stdout.
#[derive(Parser)]
#[command(name = "machinestream-mcp", version, about, long_about = None)]
struct Cli {
    /// The folder checks are about (default: MACHINESTREAM_ROOT, else the current folder).
    #[arg(long, global = true, value_name = "DIR")]
    root: Vec<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Connect your MCP clients, sign in and get ready, in one go.
    Setup {
        /// Connect every client found without asking. Never turns on anything that costs.
        #[arg(long)]
        yes: bool,
        /// Connect exactly these clients (comma separated).
        #[arg(long, value_enum, value_delimiter = ',', value_name = "CLIENTS")]
        client: Vec<Client>,
        /// Turn remote checks on without asking (they are billed in Context Credits by run time).
        #[arg(long)]
        enable_remote_checks: bool,
        /// Do not sign in.
        #[arg(long)]
        skip_login: bool,
        /// Print the sign-in link without opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Sign in through the browser and save the login.
    Login {
        /// Print the link without opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Forget the saved login.
    Logout,
    /// Say whether remote checks are ready here and what they cost.
    Status {
        /// Print the structured form.
        #[arg(long)]
        json: bool,
    },
    /// Run a command remotely and end with its own exit status.
    ///
    /// A command the service does not run is run here, unless --no-local.
    Run {
        /// A folder inside the repository to run in.
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,
        /// Stop the command after this many seconds.
        #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout_seconds: Option<u64>,
        /// Run again even if the same command already ran on exactly these files.
        #[arg(long)]
        fresh: bool,
        /// Do not run the command here when it is not run remotely (exit 75 instead).
        #[arg(long)]
        no_local: bool,
        /// The command, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Check git, credentials, the service and the catalog.
    Doctor {
        /// Show what a snapshot of this checkout would send, without sending it.
        #[arg(long)]
        explain_upload: bool,
    },
    /// Write (or print) the MCP configuration for a client.
    Install {
        #[arg(value_enum)]
        client: Client,
        /// Only print the configuration.
        #[arg(long)]
        print: bool,
        /// Use the client's user-wide file where it also has a project one.
        #[arg(long)]
        global: bool,
        /// Start the server through `npx -y @contextstream/machinestream-mcp` instead of this binary's path.
        #[arg(long, hide = true)]
        npx: bool,
    },
    /// Serve MCP over Streamable HTTP.
    Serve {
        /// The address to listen on, for example 127.0.0.1:8808.
        #[arg(long, value_name = "ADDR")]
        http: SocketAddr,
        /// A Host name clients use to reach this server (repeatable). Loopback names are always accepted.
        #[arg(long, value_name = "HOST")]
        allowed_host: Vec<String>,
        /// Accept any Host header (when a proxy in front already checks it).
        #[arg(long)]
        any_host: bool,
    },
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let debug = config::debug_logging();
    let filter = EnvFilter::new(if debug {
        "machinestream_mcp=debug,rmcp=info,warn"
    } else {
        "machinestream_mcp=warn,error"
    });
    // Logs go to stderr only: in stdio mode stdout carries the protocol.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if debug {
        builder.json().init();
    } else {
        builder.with_target(false).without_time().init();
    }
}

fn working_folder(roots: &[PathBuf]) -> PathBuf {
    roots
        .first()
        .cloned()
        .or_else(|| std::env::var_os("MACHINESTREAM_ROOT").map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A shell command line from words, quoting the ones that need it.
fn join_command(words: &[String]) -> String {
    if words.len() == 1 {
        return words[0].clone();
    }
    words
        .iter()
        .map(|word| {
            let plain = !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
            if plain {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Before any thread starts: the engine reads its API address from the environment.
    config::export_for_engine();
    init_logging();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let root = working_folder(&cli.root);
    let code = runtime.block_on(async move {
        Ok::<i32, anyhow::Error>(match cli.command {
            None => {
                serve_stdio(root).await?;
                0
            }
            Some(Command::Setup {
                yes,
                client,
                enable_remote_checks,
                skip_login,
                no_browser,
            }) => {
                setup_cli::run(
                    &root,
                    setup_cli::SetupArgs {
                        yes,
                        clients: client,
                        enable_remote_checks,
                        skip_login,
                        no_browser,
                    },
                )
                .await
            }
            Some(Command::Login { no_browser }) => cli::login(no_browser).await,
            Some(Command::Logout) => cli::logout(),
            Some(Command::Status { json }) => cli::status(&root, json).await,
            Some(Command::Run {
                cwd,
                timeout_seconds,
                fresh,
                no_local,
                command,
            }) => {
                cli::run(
                    &root,
                    cli::RunArgs {
                        command: join_command(&command),
                        cwd,
                        timeout_seconds,
                        fresh,
                        no_local,
                    },
                )
                .await
            }
            Some(Command::Doctor { explain_upload }) => cli::doctor(&root, explain_upload).await,
            Some(Command::Install {
                client,
                print,
                global,
                npx,
            }) => cli::install(&root, client, print, global, npx),
            Some(Command::Serve {
                http,
                allowed_host,
                any_host,
            }) => {
                let roots = if cli.root.is_empty() {
                    vec![root]
                } else {
                    cli.root.clone()
                };
                http::serve(HttpOptions {
                    addr: http,
                    roots,
                    allowed_hosts: allowed_host,
                    any_host,
                })
                .await?;
                0
            }
        })
    })?;
    // The runtime is dropped without waiting for background work (a catalog read).
    std::process::exit(code);
}

async fn serve_stdio(root: PathBuf) -> anyhow::Result<()> {
    let server = MachineStream::new(Roots::Fixed(root));
    server.prefetch().await;
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_given_as_words_is_joined_for_a_shell() {
        let words = |list: &[&str]| list.iter().map(|word| word.to_string()).collect::<Vec<_>>();
        assert_eq!(
            join_command(&words(&["cargo test -p api"])),
            "cargo test -p api"
        );
        assert_eq!(
            join_command(&words(&["cargo", "test", "-p", "api"])),
            "cargo test -p api"
        );
        assert_eq!(
            join_command(&words(&["pytest", "-k", "a b"])),
            "pytest -k 'a b'"
        );
        assert_eq!(join_command(&words(&["echo", "it's"])), "echo 'it'\\''s'");
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
