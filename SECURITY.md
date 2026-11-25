# Security policy

## Reporting a vulnerability

Please use GitHub private vulnerability reporting for this repository. Do not open a public issue that holds exploit details, credentials, private paths or anyone's data. If private reporting is not available to you, write to `security@contextstream.io` with the affected version, the impact and a minimal reproduction.

We will acknowledge the report, confirm its scope, coordinate a fix and its disclosure, and credit you if you want that. Please do not test against accounts or data you do not own.

## What this server handles

- **Your API key.** Read from `MACHINESTREAM_API_KEY` (or `CONTEXTSTREAM_API_KEY`), from the saved login, or from a request's `Authorization` header over HTTP. It is sent to the API only. It is never printed or logged: logs and messages carry a 12-character fingerprint (a digest) instead. The stream a check runs on receives a token for that one call, not the key.
- **The saved login** (`~/.contextstream/credentials.json`) is written with owner-only permissions, through a temporary file, and never through a symlink.
- **Your checkout.** A check sends a snapshot of it. What is sent, what is withheld and the switches that turn it off are in the README under "What leaves your machine"; `machinestream-mcp doctor --explain-upload` shows it for the current folder without sending anything.
- **Saved output.** Each run's output is kept in `~/.cache/machinestream/runs` with owner-only permissions. Over HTTP each account's runs are kept in a folder of their own.
- **Commands.** The server sends commands to the service. It runs a command on your machine only when you set `MACHINESTREAM_LOCAL_FALLBACK=1`, and never over HTTP.

## The HTTP transport

`serve --http` accepts only loopback `Host` names unless told otherwise, refuses folders outside its `--root` allowlist (symlinks resolved), and does not authenticate callers beyond the MachineStream key they bring. Read "Who can reach it" in [docs/marketplaces.md](docs/marketplaces.md) before exposing it.

## Supply chain

The snapshot, stream and sign-in code is the engine of the open-source `contextstream/mcp-server`, pinned by tag in `Cargo.toml` and locked in `Cargo.lock`. `install.sh` and `install.ps1` download the binary for your machine from this repository's GitHub release, keep it only if it matches the release's `SHA256SUMS`, and install it for your user without administrator rights. The npm launcher does the same for its own version.
