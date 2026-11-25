# Marketplaces and hosted use

This page is for people who list or host `machinestream-mcp` for others: MCP marketplaces, container platforms, cloud agents.

## Two ways to offer it

### (a) Bring your own account. Works today.

Each user has their own ContextStream account. They give the server their API key (`MACHINESTREAM_API_KEY`, or the `Authorization: Bearer` header over HTTP), or sign in through the `setup` tool. Their checks run in their own account's environment and are paid with their own Context Credits. The marketplace bills nothing; it only distributes the server.

This is how the install scripts, the Claude Desktop bundle and the stdio Docker image work. The npm launcher and the MCP registry entry (`server.json`) in this repository work the same way; they are not published yet.

### (b) Marketplace-paid, billed per credit. Works today with one account.

The marketplace holds one MachineStream account, runs the server with that account's key, and charges its own users for what they use. Two hooks make that possible:

1. **The billing block.** Every `check` result carries, in `structuredContent.billing`:

   ```json
   { "credits_estimated": 9, "billable_seconds": 4.2, "free_reason": null, "estimate": true, "credits_per_10s": 20 }
   ```

   `check_parallel` carries one per command under `checks[]`, and their sum in `summary.credits_estimated`. `free_reason` is `cached`, `not_run_remotely`, `ran_locally` or `account_not_charged` when the call costs nothing. A proxy in front of the server can read the block from each tool result and charge its user.

2. **The usage log.** With `MACHINESTREAM_USAGE_LOG=/path/usage.jsonl` the server appends one JSON line per command:

   ```json
   {"time":"2026-01-01T12:00:00.000Z","tool":"check","run_id":"20260101-120000-a1b2","billable_seconds":4.2,"credits_estimated":9,"cached":false,"ran":true,"key_fingerprint":"0123456789ab"}
   ```

   `key_fingerprint` is a digest of the key the call used, never the key. The file is append-only and each line is written in one piece, so an adapter can tail it. Over HTTP with several keys, the fingerprint tells the accounts apart.

**What an adapter does.** For each of its users it keeps a balance. It forwards the user's MCP requests to the server, reads `billing.credits_estimated` from each `check` or `check_parallel` result (or follows the usage log and joins on `run_id`), and debits the user. It should treat the figure as what it says: an estimate, computed from the published price and the command time the service reports, rounded up per check. The account's own ledger at ContextStream is the authority, and the adapter should reconcile against it.

**What this suits.** All checks made with one account run in that account's environment and are paid from its credits, whoever the end user is. That fits a marketplace whose users belong together (one team, one organisation). When each end user should have an environment of their own, use model (a), or [contact us](https://contextstream.io) to talk about your case.

## Serving over HTTP

```sh
machinestream-mcp serve --http 0.0.0.0:8808 --root /workspace --allowed-host mcp.example.com
```

- **Endpoint.** MCP Streamable HTTP at `/mcp`. `GET /healthz` answers `{"status":"ok", ...}` for load balancers.
- **Accounts.** A request's `Authorization: Bearer <API key>` header chooses the account. Without the header, the process's own `MACHINESTREAM_API_KEY` is used, if set. One process can serve several accounts; each account's saved runs are kept apart and are never shown to another.
- **Folders.** A check needs a git checkout on the server's disk. `--root` (repeatable) lists the folders sessions may work in; the first is the default. A request may name a folder inside one of them with the `X-MachineStream-Root` header (absolute, or relative to the first root). Anything outside the allowlist is refused, symlinks included.
- **Host check.** Only loopback `Host` names are accepted unless you pass `--allowed-host <name>` for the name clients use (repeatable), or `--any-host` when a proxy in front already checks it.
- **No local execution.** `MACHINESTREAM_LOCAL_FALLBACK` is ignored over HTTP: a remote caller never gets a command run on the server's machine.
- **Sign-in.** The browser sign-in of `setup` is for a person at their own machine. Over HTTP, `setup` asks for the `Authorization` header instead.

### Who can reach it

The server does not authenticate callers beyond the MachineStream key they bring. Anyone who can reach the port can ask for a snapshot of the allowed folders to be checked under their own account, and, when the process has its own key, can spend that account's credits. Put it behind your platform's authentication, bind it to a private interface, and treat the `--root` folders as readable by every caller you let through.

## Docker

The image runs the stdio server by default:

```sh
docker build -t machinestream-mcp .
docker run -i --rm -e MACHINESTREAM_API_KEY -v "$PWD":/workspace machinestream-mcp
```

and the HTTP server with arguments:

```sh
docker run --rm -p 8808:8808 -v "$PWD":/workspace machinestream-mcp \
  serve --http 0.0.0.0:8808 --root /workspace --allowed-host mcp.example.com
```

The image holds git (a snapshot needs it) and no language toolchains. MachineStream matches the versions of the tools a command uses to your project (see [how versions are matched](https://machinestream.io/docs/support#matching)), and the server reads the local versions to help it. In an image without those tools, pin the versions in the repository (for example with `rust-toolchain.toml` or `.nvmrc`), or build an image that has the tools.

## Listing files in this repository

| File | For |
| --- | --- |
| `server.json` | The official MCP registry (`io.github.contextstream/machinestream-mcp`, an npm package over stdio), not published yet |
| `package.json`, `npm/` | The npm launcher (`npx -y @contextstream/machinestream-mcp`), not published yet |
| `mcpb/manifest.json` | The Claude Desktop bundle, with the project folder and an optional API key as user settings |
| `Dockerfile` | Container platforms |
| `smithery.yaml` | Smithery (stdio start command and its settings) |
| `glama.json` | Glama (the maintainers allowed to manage the listing) |
