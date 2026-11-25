#!/usr/bin/env bash
# Pack the Claude Desktop bundle, dist/machinestream-mcp.mcpb, from the manifest, the npm
# launcher and the icon.
#
#   scripts/release/bundle.sh
#
# The bundle holds no binary. Its launcher downloads the one for the machine from the GitHub
# release of its own version and checks it against that release's checksums, so pack it for
# a version that is released. Needs Node.js 20 or newer.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/bundle" dist
cp mcpb/manifest.json package.json LICENSE README.md "$work/bundle/"
cp assets/machinestream-icon.png "$work/bundle/icon.png"
cp -r npm "$work/bundle/npm"
rm -rf "$work/bundle/npm/test"
npx --yes @anthropic-ai/mcpb validate "$work/bundle/manifest.json"
npx --yes @anthropic-ai/mcpb pack "$work/bundle" dist/machinestream-mcp.mcpb
