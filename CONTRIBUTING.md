# Contributing

Thanks for helping. This project is deliberately small: an MCP server whose only job is remote checks. Changes that keep it that way are the easy ones to accept.

## Before you start

Open an issue before a change to the tool surface (names, arguments, result fields). Agents and marketplaces depend on those staying stable.

Changes to how a checkout is snapshotted, how a run's stream is driven or how sign-in works belong in the engine, [`contextstream/mcp-server`](https://github.com/contextstream/mcp-server). This repository pins that engine and must not fork it.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
npm test
```

`cargo test` starts the built binary against a fake API and a fake stream server on localhost. It needs git and bash, and no network or account.

## Rules for what goes in

- No credentials, account names, private host names or internal paths, in code, tests, docs or commit messages. Tests use generic fixtures.
- Nothing about a command or an address is hard-coded: tool definitions, command patterns and stream addresses come from the API at run time.
- Describe the service only as its public documentation at [machinestream.io](https://machinestream.io) does.
- Plain, precise English.
- A tool description says when to use the tool and when not to. A result leads with the answer and says the next step.
- New behaviour comes with a test.
