#!/bin/sh
# MachineStream MCP installer for macOS and Linux.
#
#   curl -fsSL https://machinestream.io/install.sh | sh
#
# Downloads the machinestream-mcp binary for this machine from the project's GitHub
# releases, checks it against the release's SHA-256 checksums, installs it without sudo
# and runs `machinestream-mcp setup`. Safe to run again: a rerun upgrades.
#
#   MACHINESTREAM_VERSION      a version to install instead of the latest (0.1.0 or v0.1.0)
#   MACHINESTREAM_INSTALL_DIR  where to install (default: ~/.local/bin)
#   MACHINESTREAM_NO_SETUP=1   install only, do not run setup
#   MACHINESTREAM_DRY_RUN=1    say what would be done, and do nothing

set -eu

REPO="contextstream/machinestream-mcp"
NAME="machinestream-mcp"
SUMS="SHA256SUMS"

say() { printf '%s\n' "$*"; }
row() { printf '  %-10s %s\n' "$1" "$2"; }
fail() {
  printf '\nmachinestream-mcp was not installed: %s\n' "$*" >&2
  exit 1
}

# The release asset for an operating system and a CPU, as `uname` names them.
asset_for() {
  case "$1" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    MINGW* | MSYS* | CYGWIN* | Windows_NT)
      fail "this script is for macOS and Linux. On Windows run in PowerShell: irm https://machinestream.io/install.ps1 | iex" ;;
    *) fail "there is no binary for $1 yet. Build from source: https://github.com/$REPO" ;;
  esac
  case "$2" in
    x86_64 | amd64) cpu=x64 ;;
    aarch64 | arm64) cpu=arm64 ;;
    *) fail "there is no binary for $1 on $2 yet. Build from source: https://github.com/$REPO" ;;
  esac
  printf '%s-%s-%s' "$NAME" "$os" "$cpu"
}

# Where a version's files are: the latest release, or a named one.
release_base() {
  if [ -n "${MACHINESTREAM_RELEASE_BASE_URL:-}" ]; then
    printf '%s' "${MACHINESTREAM_RELEASE_BASE_URL%/}"
  elif [ -n "${MACHINESTREAM_VERSION:-}" ]; then
    printf 'https://github.com/%s/releases/download/v%s' "$REPO" "${MACHINESTREAM_VERSION#v}"
  else
    printf 'https://github.com/%s/releases/latest/download' "$REPO"
  fi
}

fetch() { # fetch <url> <file>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto "$PROTO" --retry 2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    fail "curl or wget is needed to download it"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | awk '{print $NF}'
  else
    fail "sha256sum, shasum or openssl is needed to check the download"
  fi
}

main() {
  # Only https, except for a mirror on this machine (used by the tests).
  PROTO='=https'
  case "${MACHINESTREAM_RELEASE_BASE_URL:-}" in
    http://127.0.0.1:* | http://localhost:*) PROTO='=https,http' ;;
  esac
  asset=$(asset_for "${MACHINESTREAM_OS:-$(uname -s)}" "${MACHINESTREAM_ARCH:-$(uname -m)}")
  base=$(release_base)
  dir=${MACHINESTREAM_INSTALL_DIR:-"$HOME/.local/bin"}
  target="$dir/$NAME"
  version=${MACHINESTREAM_VERSION:-latest}

  say "MachineStream MCP installer"
  say ""
  row "Platform" "${asset#"$NAME"-}"
  row "Version" "${version#v}"

  if [ "${MACHINESTREAM_DRY_RUN:-}" = "1" ]; then
    row "Download" "$base/$asset"
    row "Checksums" "$base/$SUMS"
    row "Install" "$target"
    say ""
    say "Dry run: nothing was downloaded or changed."
    return 0
  fi

  tmp=$(mktemp -d 2>/dev/null || mktemp -d -t machinestream)
  staged=""
  trap 'rm -rf "$tmp"; [ -z "$staged" ] || rm -f "$staged"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  fetch "$base/$asset" "$tmp/$asset" || fail "could not download $base/$asset"
  fetch "$base/$SUMS" "$tmp/$SUMS" || fail "could not download $base/$SUMS"
  row "Download" "$asset"

  expected=$(awk -v file="$asset" '$2 == file || $2 == "*" file { print $1; exit }' "$tmp/$SUMS")
  [ -n "$expected" ] || fail "$SUMS does not list $asset"
  actual=$(sha256_of "$tmp/$asset")
  [ "$actual" = "$expected" ] || fail "the download did not match its checksum (expected $expected, got $actual)"
  row "Verified" "sha256 $(printf '%s' "$actual" | cut -c1-16)"

  # Staged beside the target and renamed, so a half-written binary is never left behind.
  mkdir -p "$dir" || fail "could not create $dir"
  staged="$dir/.$NAME.$$.tmp"
  cp "$tmp/$asset" "$staged" || fail "could not write to $dir"
  chmod 755 "$staged"
  mv -f "$staged" "$target" || fail "could not replace $target"
  staged=""
  installed=$("$target" --version 2>/dev/null || true)
  [ -n "$installed" ] || fail "$target does not run on this machine"
  row "Installed" "$target ($installed)"

  on_path=0
  case ":$PATH:" in
    *":$dir:"*) on_path=1 ;;
  esac
  if [ "$on_path" = 0 ]; then
    say ""
    say "  $dir is not on your PATH. Add this line to your shell profile, then open a new shell:"
    say "    export PATH=\"$dir:\$PATH\""
  fi

  say ""
  if [ "${MACHINESTREAM_NO_SETUP:-}" = "1" ]; then
    say "Next: $target setup"
  elif [ -t 0 ]; then
    "$target" setup || say "Setup did not finish. Run it again any time: $target setup"
  elif (: </dev/tty) 2>/dev/null; then
    # The script came through a pipe: setup asks on the terminal itself.
    "$target" setup </dev/tty || say "Setup did not finish. Run it again any time: $target setup"
  else
    say "There is no terminal to ask on, so setup was not run."
    say "Next: $target setup"
  fi
}

main "$@"
