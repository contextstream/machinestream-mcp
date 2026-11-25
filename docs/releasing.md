# Releasing

Nothing here runs by itself.

1. Set the same version in `Cargo.toml`, `package.json`, `server.json` (twice) and `mcpb/manifest.json`. Add the version to `CHANGELOG.md`.
2. Run the checks: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `npm test`, and the live test (see the README).
3. Build the five binaries and their checksums, on Linux x86_64, from a clean checkout of the commit to release:

   ```sh
   XWIN_ACCEPT_LICENSE=1 scripts/release/build.sh all
   ```

   It writes `dist/` with `machinestream-mcp-linux-x64`, `-linux-arm64`, `-macos-x64`, `-macos-arm64`, `-windows-x64.exe` and `SHA256SUMS`, and checks each binary before it says OK. The head of the script says what it needs, how each binary is built and how it is checked.
4. Tag `vX.Y.Z`, push the tag, and publish the release with those files:

   ```sh
   gh release create vX.Y.Z dist/* --verify-tag --title vX.Y.Z --notes "See CHANGELOG.md for what changed."
   ```

   The install scripts download from `releases/latest/download/`, so a published release is what `curl -fsSL https://machinestream.io/install.sh | sh` installs from then on. Install it that way once and run `machinestream-mcp --version`. The site serves copies of `install.sh` and `install.ps1` from this repository: update them there when they change here.
5. Optional, when the npm package is wanted: publish it (`npm publish`), then the registry entry (`mcp-publisher publish`, which reads `server.json`), and check `npx -y @contextstream/machinestream-mcp@X.Y.Z --version`. The launcher downloads from `releases/download/vX.Y.Z/`, so the release must be public first.

To move to a newer engine, change the tag of the four `mcp-*` git dependencies in `Cargo.toml`, run `cargo update -p mcp-client -p mcp-tools -p mcp-session -p mcp-types`, and repeat step 2.
