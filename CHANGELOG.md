# Changelog

## 0.1.0

The first version.

- MCP tools: `check`, `check_parallel`, `status`, `setup` and `run_output`, each with a title, annotations, and structured content with an output schema where the result has a fixed shape.
- Progress notifications that carry the newest output lines, and cancellation.
- Tools the account's catalog defines are listed as the service describes them, read in the background and cached; changes are announced.
- Saved runs as MCP resources (`machinestream://runs/<id>`) and a `verify-changes` prompt.
- A billing estimate on every check result, and an optional usage log for per-use billing.
- An opt-in local fallback (`MACHINESTREAM_LOCAL_FALLBACK=1`).
- Install scripts for macOS, Linux and Windows (`install.sh`, `install.ps1`) that check the download against the release's checksums.
- `machinestream-mcp setup`: connects the MCP clients on the machine, signs in, shows the price and the balance, and turns remote checks on only after a yes.
- A command line: `login`, `logout`, `status`, `run`, `doctor` (with `--explain-upload`), `install` for seven clients, and `serve --http`.
- Streamable HTTP transport with a key per request, a folder allowlist and a health answer.
- An npm launcher that downloads the platform binary and checks it against the release's checksums.
- Manifests for the MCP registry, Claude Desktop bundles, Smithery, Glama and Docker.
