# Design

## What this is

`machinestream-mcp` is a small MCP server and CLI whose only job is remote checks: run a build, test, lint or type-check command on MachineStream against the exact state of a checkout, and give back the command's own exit code and output.

It is one Rust binary. With no arguments it speaks MCP on stdin and stdout. `serve --http` speaks the same protocol over Streamable HTTP. The subcommands (`setup`, `login`, `status`, `run`, `doctor`, `install`) are the same code paths for people and CI.

## Why the engine is shared

The hard part of a remote check is not the MCP surface. It is taking a faithful snapshot of a checkout without writing to the repository, withholding what looks like a credential, describing the local toolchain so the service can match it, driving the stream a run is answered on, and reading the result the same way every client does. That code already exists, tested, in the open-source ContextStream MCP server ([`contextstream/mcp-server`](https://github.com/contextstream/mcp-server), MIT). A second implementation would drift, and a drifted snapshot means a result that differs from the user's machine, which is the one thing the product must never do.

So this project depends on that repository's engine crates through Cargo git dependencies pinned to a release tag:

| Crate | What this project uses from it |
| --- | --- |
| `mcp-client` | The API client, device login, the checkout snapshot, the stream protocol (`frames`), command hints, account settings for remote checks, the description of the local environment |
| `mcp-tools` | The catalog of tools the API defines (`listing_entries`, `find`, `run`), and the pieces of a streamed call (`begin_snapshot`, `call_api`, `stream_call`, `drive_stream`, `enrich_frame_diagnostics`, `format_diagnostic_report`) |
| `mcp-session` | The session a call is made in (the working folder, workspace resolution) |
| `mcp-types` | Shared types (configuration, tool results, errors) |

Updating the engine is a one-line change of the tag in `Cargo.toml`, followed by the test suite and the live test.

Nothing in this repository knows which commands the service takes, where a run happens, or what a stream address or a call token looks like. The tool definitions and command patterns come from the API's catalog at run time, and each call's stream address and token come from the API's answer to that call. The only addresses in the code are the public API base (overridable) and the public sites.

## What this project adds

```
src/
  main.rs        the command line
  server.rs      the MCP surface: tools, resources, the prompt, instructions
  check.rs       one or several commands against one snapshot; turned-away answers; local fallback
  format.rs      result text and structured content; output bounding; run_output selection
  runlog.rs      saved runs (output and record), pruning
  billing.rs     the credit estimate and the usage log
  status.rs      the readiness report
  setup.rs       sign-in state and saving the login
  setup_cli.rs   the setup command: connect clients, sign in, show the price, ask before turning checks on
  config.rs      settings read from the environment
  catalog.rs     catalog tools: background read, memory and disk cache, change detection
  engine.rs      one account's handles on the engine, for one working folder
  credentials.rs the saved login (shared with the ContextStream MCP)
  install.rs     per-client MCP configuration writers
  cli.rs         login, logout, status, run, doctor, install
  http.rs        the Streamable HTTP transport and its folder allowlist
  peer.rs        glue between the engine's progress channel and the MCP SDK
```

The MCP protocol itself is the official Rust SDK (`rmcp`). The server supports every protocol revision the SDK knows, from 2024-11-05 to 2026-07-28, and answers in the one the client asks for: through `initialize` for the older revisions, and per request (with `server/discover`) for 2026-07-28, which has no handshake.

On 2026-07-28 the tool list is marked as not cacheable, so a client sees a change the next time it lists; on older revisions a change is announced with `notifications/tools/list_changed`. `setup` returns the sign-in link as text on every revision, and also offers it through URL elicitation to clients of older revisions that declare it.

## How a check runs

`check` and `check_parallel` follow the sequence the engine's own `run` command follows:

1. Find the catalog's tool for running a command. If there is none (signed out, remote checks off, the API unreachable), the answer is "not run remotely" with the reason and the next step.
2. Begin a snapshot of the checkout. Who the repository is comes back at once; the rest is taken while the API is asked.
3. Ask the API for each call, with the command, the folder, and the options the tool's own schema names. The API answers with a stream for that one call, or refuses (out of credits, a command it does not take).
4. Finish the snapshot. There is one per tool call, however many commands it carries.
5. Drive each stream: send the snapshot's description and the changed files, answer the service's requests for files it lacks, and take output as it prints. Output goes to the run's log file and, when the client asked for progress, to progress notifications.
6. Read the result. A run the service stands by is a verdict whatever its exit status. Anything else (a decline, a failure the service reports as its own, a stream that ended early) is "not run remotely" with the reason.

`check_parallel` does steps 3, 5 and 6 for all commands at the same time, against the one snapshot.

Cancelling the MCP request drops the run's future, which closes its stream.

## Decisions

**A turned-away check is not an error.** `isError` is true only when the tool itself failed (bad arguments, an unreadable local file). A check that failed is a result (`passed: false`). A check that was not run remotely is a routing answer (`ran: false`, the reason, the command to run locally). Agents handle both far better as results than as errors.

**The server does not run commands locally unless asked.** The agent that called the tool has its own permission model for running commands; quietly running them inside an MCP server would go around it. `MACHINESTREAM_LOCAL_FALLBACK=1` opts in. It is never honoured over HTTP, where the caller is not the machine's owner.

**Nothing is turned on or spent without an explicit argument.** `setup` explains and waits for `enable_remote_checks: true`.

**Output is bounded, and never lost.** A result carries a tail (200 lines for a failing command, 30 for a passing one, a few per command in a parallel call, all adjustable) and the path of the full log. `run_output` reads the rest.

**The credit figure is an estimate.** It is the published price times the command time the service reports, rounded up per check. The account's ledger is the authority, and the field is named and flagged accordingly.

**`tools/list` never waits on the network.** The catalog is read in the background and cached in memory and on disk; a listing answers with what is known and a change is announced with `notifications/tools/list_changed`.

**The login is the shared one.** The saved login lives where the ContextStream MCP keeps it, so signing in once covers both. The file is written with owner-only permissions, through a temporary file, never through a symlink, and keys this project does not know are preserved.

## What the engine does that is worth knowing

- It keeps small files under `~/.contextstream` (the saved login, cached command hints, a few timestamp markers). They hold digests and settings, never file contents.
- After a failing check it asks the API for context on the extracted failures (a call bounded to a fraction of a second) so the failure report can separate new failures from ones the base already had.
- Its switches work here as they do in the ContextStream MCP: `CONTEXTSTREAM_HOSTED_TOOLS=off`, `CONTEXTSTREAM_CHECKOUT_SNAPSHOT=off`, `CONTEXTSTREAM_CHECKOUT_SNAPSHOT_REPOS`, `CONTEXTSTREAM_HOSTED_ENV=off`.

## Written here, on the engine's public items

Three small pieces live in this repository because the engine keeps its own versions in its command-line crate, not in the library crates this project depends on:

- **Reading a result** (`check.rs`): whether a run is a verdict or a "run it locally" answer. It is built on the engine's public helpers (`frames::declined_reason`, `frames::decline_explanation`, `frames::display_text`).
- **Saving the login** (`credentials.rs`). The engine's library crates hold the device login and the type of the saved file; this writes the same file.
- **Running a command locally**, for the opt-in fallback.

## Tests

- Unit tests beside the code: result formatting, output bounding, the run store, argument validation, the credit estimate, the configuration writers, the saved login.
- Integration tests (`tests/`) start the built binary and speak JSON-RPC to it over stdio or HTTP, against a fake API and a fake stream server on localhost. The fakes define generic tools (a `check` whose behaviour is chosen by its command, and an `echo`). They cover listing, catalog tools, a check, the cache, failures, progress, cancellation, turned-away checks, out of credits, signed out, a stream broken mid-run, parallel checks on one snapshot, setup, saved runs, the local fallback, the usage log, the CLI and the HTTP transport.
- A live end-to-end test (`tests/e2e_live.rs`, ignored by default) runs a real check through the stdio interface.
