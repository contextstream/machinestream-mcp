#!/usr/bin/env bash
# Build and check the release binaries, all of them on Linux x86_64.
#
#   scripts/release/build.sh <asset>...   one or more of the five assets named below
#   scripts/release/build.sh all          all five, then dist/SHA256SUMS
#   scripts/release/build.sh sums         write dist/SHA256SUMS for the five assets in dist/
#
# Each binary is written to dist/<asset> and checked before the script says OK. No macOS or
# Windows machine is used. This is the way the engine's own releases are built
# (contextstream/mcp-server, scripts/release/build.sh):
#
#   linux    cargo-zigbuild against glibc 2.28, so a binary starts on RHEL 8, Debian 10 and
#            anything newer. x64 is run here; arm64 is run under qemu-aarch64 when it is
#            installed, and is checked by its headers otherwise.
#   windows  cargo-xwin, which downloads Microsoft's CRT and Windows SDK. Their license has to
#            be accepted by whoever runs the build: set XWIN_ACCEPT_LICENSE=1. The binary is
#            x64, which Windows on ARM runs too, and it is run under Wine when that is installed.
#   macos    clang and ld64.lld against an SDK root made from the macOS libc headers that Zig
#            ships and the framework stubs of the engine (its scripts/release/macos-link-stubs,
#            read from the checkout Cargo already holds). Checked by its headers: the CPU, the
#            minimum macOS version, the code signature on arm64 and the version string.
#
# Needs: rustup with the toolchain this project builds with, clang, lld and llvm (with
# ld64.lld), nasm, curl and python3. The other tools are downloaded, pinned by sha256.
set -euo pipefail

ASSETS=(
  machinestream-mcp-linux-x64
  machinestream-mcp-linux-arm64
  machinestream-mcp-macos-x64
  machinestream-mcp-macos-arm64
  machinestream-mcp-windows-x64.exe
)
SUMS=SHA256SUMS

ZIG_VERSION=0.14.1
ZIG_SHA256=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c
ZIG_DIR=zig-x86_64-linux-$ZIG_VERSION
ZIGBUILD_VERSION=v0.24.0
ZIGBUILD_SHA256=52be2cb85ad3875682564a29bac1832b2ab3db68b1c504765c92d5249cc9b96f
XWIN_VERSION=v0.23.1
XWIN_SHA256=c492c6dfb7e5ac0eee586b796e0fc950ba13077e7f0bbb046f445d71790d5360
# The oldest glibc a Linux binary must start on, so the newest one it may need.
GLIBC_FLOOR=2.28

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
dist=$root/dist

log() { echo "build.sh: $*"; }
die() { echo "build.sh: $*" >&2; exit 1; }

# fetch URL SHA256 FILE: download and check the pinned digest.
fetch() {
  curl -fsSL --retry 4 --retry-delay 3 --max-time 300 -o "$3" "$1"
  echo "$2  $3" | sha256sum -c --quiet -
}

write_sums() {
  for asset in "${ASSETS[@]}"; do
    [ -s "$dist/$asset" ] || die "dist/$asset is missing: build it first"
  done
  (cd "$dist" && sha256sum "${ASSETS[@]}" > "$SUMS")
  log "wrote dist/$SUMS"
}

# What a binary's headers say, for the ones Linux cannot run.
headers() { # headers <file> <kind> <cpu> [<minimum macOS>]
  python3 - "$@" "$version" <<'PY'
import struct, sys

path, kind, cpu = sys.argv[1:4]
version = sys.argv[-1]
data = open(path, "rb").read()


def fail(message):
    sys.exit("build.sh: %s: %s" % (path, message))


if ("machinestream-mcp " + version).encode() not in data and version.encode() not in data:
    fail("does not carry version " + version)
if kind == "elf":
    if data[:4] != b"\x7fELF":
        fail("is not an ELF file")
    machine = struct.unpack_from("<H", data, 18)[0]
    if machine != {"x64": 62, "arm64": 183}[cpu]:
        fail("has ELF machine %d" % machine)
elif kind == "pe":
    offset = struct.unpack_from("<I", data, 0x3C)[0]
    if data[:2] != b"MZ" or data[offset:offset + 4] != b"PE\0\0":
        fail("is not a PE file")
    machine = struct.unpack_from("<H", data, offset + 4)[0]
    if machine != {"x64": 0x8664, "arm64": 0xAA64}[cpu]:
        fail("has PE machine %#x" % machine)
elif kind == "macho":
    wanted = tuple(int(part) for part in sys.argv[4].split("."))
    magic, cputype, _, _, count = struct.unpack_from("<IiiII", data, 0)
    if magic != 0xFEEDFACF:
        fail("is not a 64-bit Mach-O file")
    if cputype != {"x64": 0x01000007, "arm64": 0x0100000C}[cpu]:
        fail("has cputype %#x" % cputype)
    offset, minimum, signed, libraries = 32, None, False, []
    for _ in range(count):
        command, size = struct.unpack_from("<II", data, offset)
        if command == 0x32:  # LC_BUILD_VERSION
            packed = struct.unpack_from("<I", data, offset + 12)[0]
            minimum = (packed >> 16, (packed >> 8) & 0xFF)
        elif command == 0x24:  # LC_VERSION_MIN_MACOSX
            packed = struct.unpack_from("<I", data, offset + 8)[0]
            minimum = (packed >> 16, (packed >> 8) & 0xFF)
        elif command == 0x1D:  # LC_CODE_SIGNATURE
            signed = True
        elif command == 0x0C:  # LC_LOAD_DYLIB
            start = offset + struct.unpack_from("<I", data, offset + 8)[0]
            libraries.append(data[start:data.index(b"\0", start)].decode())
        offset += size
    if minimum != wanted[:2]:
        fail("needs macOS %s, not %s" % (minimum, wanted[:2]))
    if cpu == "arm64" and not signed:
        fail("has no code signature; macOS refuses an unsigned arm64 binary")
    print("build.sh: %s: macOS %d.%d and newer, %s, links %s" % (
        path.rsplit("/", 1)[-1], minimum[0], minimum[1], "signed" if signed else "unsigned",
        ", ".join(sorted(name.rsplit("/", 1)[-1] for name in libraries))))
PY
}

build() {
  local asset=$1 target kind cpu exe=machinestream-mcp
  case "$asset" in
    machinestream-mcp-linux-x64) target=x86_64-unknown-linux-gnu; kind=linux; cpu=x64 ;;
    machinestream-mcp-linux-arm64) target=aarch64-unknown-linux-gnu; kind=linux; cpu=arm64 ;;
    machinestream-mcp-macos-x64) target=x86_64-apple-darwin; kind=macos; cpu=x64; export MACOSX_DEPLOYMENT_TARGET=10.12 ;;
    machinestream-mcp-macos-arm64) target=aarch64-apple-darwin; kind=macos; cpu=arm64; export MACOSX_DEPLOYMENT_TARGET=11.0 ;;
    machinestream-mcp-windows-x64.exe) target=x86_64-pc-windows-msvc; kind=windows; cpu=x64; exe=machinestream-mcp.exe ;;
    *) die "unknown asset $asset (one of: ${ASSETS[*]})" ;;
  esac
  [ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] || die "the release binaries build on Linux x86_64"

  # A target directory and a tool directory of its own, so several assets can build side by side.
  local work=$root/target/release-assets/$asset
  local tools=$work/tools
  export CARGO_TARGET_DIR=$work/target
  rm -rf "$tools" && mkdir -p "$tools" "$dist"
  rustup target add "$target" >/dev/null

  case "$kind" in
    linux)
      fetch "https://ziglang.org/download/$ZIG_VERSION/$ZIG_DIR.tar.xz" "$ZIG_SHA256" "$tools/zig.tar.xz"
      tar -xJf "$tools/zig.tar.xz" -C "$tools"
      fetch "https://github.com/rust-cross/cargo-zigbuild/releases/download/$ZIGBUILD_VERSION/cargo-zigbuild-x86_64-unknown-linux-musl.tar.xz" \
        "$ZIGBUILD_SHA256" "$tools/zigbuild.tar.xz"
      tar -xJf "$tools/zigbuild.tar.xz" -C "$tools"
      # Zig compiles the C dependencies and links, against glibc $GLIBC_FLOOR. A linker or flags the environment
      # sets for native builds would link against this machine's own glibc instead.
      local variable
      variable=CARGO_TARGET_$(echo "${target//-/_}" | tr 'a-z' 'A-Z')
      env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS -u "${variable}_LINKER" -u "${variable}_RUSTFLAGS" \
        PATH="$tools/$ZIG_DIR:$tools/cargo-zigbuild-x86_64-unknown-linux-musl:$PATH" \
        cargo zigbuild --locked --release --bin machinestream-mcp --target "$target.$GLIBC_FLOOR"
      ;;
    windows)
      [ "${XWIN_ACCEPT_LICENSE:-}" = 1 ] || die "the Windows build downloads Microsoft's CRT and SDK: read their license terms, then set XWIN_ACCEPT_LICENSE=1"
      command -v lld-link >/dev/null || die "lld-link is needed for the Windows build (install lld)"
      fetch "https://github.com/messense/cargo-xwin/releases/download/$XWIN_VERSION/cargo-xwin-$XWIN_VERSION.x86_64-unknown-linux-musl.tar.gz" \
        "$XWIN_SHA256" "$tools/xwin.tgz"
      tar -xzf "$tools/xwin.tgz" -C "$tools"
      PATH="$tools:$PATH" XWIN_CACHE_DIR=$work/xwin cargo xwin build --locked --release --bin machinestream-mcp --target "$target"
      ;;
    macos)
      compgen -G '/usr/lib/llvm-*/bin/ld64.lld' >/dev/null || die "ld64.lld is needed for the macOS build (install clang, lld and llvm)"
      local llvm_bin engine stubs zig_libc sdk libsystem triple underscored upper header
      llvm_bin=$(dirname "$(ls /usr/lib/llvm-*/bin/ld64.lld | sort -V | tail -1)")
      export PATH="$llvm_bin:$PATH"
      # The engine's checkout, as Cargo fetched it for the pinned tag: it holds the framework stubs.
      engine=$(cargo metadata --format-version 1 --locked | python3 -c '
import json, os, sys
packages = json.load(sys.stdin)["packages"]
manifest = next(p["manifest_path"] for p in packages if p["name"] == "mcp-client")
print(os.path.dirname(os.path.dirname(os.path.dirname(manifest))))')
      stubs=$engine/scripts/release/macos-link-stubs
      [ -d "$stubs/System/Library/Frameworks" ] || die "the engine checkout at $engine has no macos-link-stubs"
      fetch "https://ziglang.org/download/$ZIG_VERSION/$ZIG_DIR.tar.xz" "$ZIG_SHA256" "$tools/zig.tar.xz"
      tar -xJf "$tools/zig.tar.xz" -C "$tools"
      zig_libc=$tools/$ZIG_DIR/lib/libc
      sdk=$tools/MacOSX.sdk
      mkdir -p "$sdk/usr/include" "$sdk/usr/lib" "$sdk/System/Library/Frameworks"
      [ -d "$zig_libc/include/any-macos-any/sys" ] || die "zig $ZIG_VERSION has no macOS headers"
      cp -R "$zig_libc/include/any-macos-any/." "$sdk/usr/include/"
      [ ! -d "$zig_libc/include/${target%%-*}-macos-none" ] || cp -R "$zig_libc/include/${target%%-*}-macos-none/." "$sdk/usr/include/"
      libsystem=$(find "$zig_libc" -name libSystem.tbd | head -1)
      [ -s "$libsystem" ] || die "zig $ZIG_VERSION has no libSystem.tbd"
      cp "$libsystem" "$sdk/usr/lib/libSystem.tbd"
      ln -s libSystem.tbd "$sdk/usr/lib/libc.tbd"
      ln -s libSystem.tbd "$sdk/usr/lib/libm.tbd"
      cp "$stubs/usr/lib/libiconv.tbd" "$sdk/usr/lib/libiconv.tbd"
      cp -R "$stubs/System/Library/Frameworks/." "$sdk/System/Library/Frameworks/"
      (cd "$stubs/include" && find . -type f -name '*.h') | while read -r header; do
        [ -e "$sdk/usr/include/$header" ] || install -D -m 0644 "$stubs/include/$header" "$sdk/usr/include/$header"
      done
      printf '{"CanonicalName": "macosx14.0", "DisplayName": "macOS 14.0", "Version": "14.0", "MaximumDeploymentTarget": "14.0.99"}\n' \
        > "$sdk/SDKSettings.json"
      triple=${target%%-*}-apple-macos$MACOSX_DEPLOYMENT_TARGET
      triple=${triple/aarch64-/arm64-}
      # One driver for compiling and linking: clang, with ld64.lld.
      printf '#!/bin/sh\nexec clang --target=%s -isysroot %s -fuse-ld=lld -Wno-unused-command-line-argument "$@"\n' \
        "$triple" "$sdk" > "$tools/macos-cc"
      chmod 0755 "$tools/macos-cc"
      underscored=${target//-/_}
      upper=$(echo "$underscored" | tr 'a-z' 'A-Z')
      env -u RUSTFLAGS SDKROOT="$sdk" "CC_$underscored=$tools/macos-cc" "AR_$underscored=llvm-ar" "RANLIB_$underscored=llvm-ranlib" \
        "CARGO_TARGET_${upper}_LINKER=$tools/macos-cc" \
        cargo build --locked --release --bin machinestream-mcp --target "$target"
      ;;
  esac

  local binary=$CARGO_TARGET_DIR/$target/release/$exe shown newest objdump
  [ -s "$binary" ] || die "$binary was not built"
  install -m 0755 "$binary" "$dist/$asset"

  case "$kind" in
    linux)
      headers "$dist/$asset" elf "$cpu"
      if [ "$cpu" = x64 ]; then
        shown=$("$dist/$asset" --version)
        [ "$shown" = "machinestream-mcp $version" ] || die "$asset printed [$shown]"
        log "ran it: $shown"
      elif command -v qemu-aarch64 >/dev/null && [ -d /usr/aarch64-linux-gnu/lib ]; then
        shown=$(qemu-aarch64 -L /usr/aarch64-linux-gnu "$dist/$asset" --version 2>&1 | tail -1)
        [ "$shown" = "machinestream-mcp $version" ] || die "qemu-aarch64 printed [$shown]"
        log "ran it under qemu-aarch64: $shown"
      else
        log "$asset was not run (no qemu-aarch64 here); its headers were checked"
      fi
      objdump=$(command -v llvm-objdump || ls /usr/lib/llvm-*/bin/llvm-objdump | sort -V | tail -1)
      newest=$("$objdump" -T "$dist/$asset" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
      [ -n "$newest" ] && [ "$(printf '%s\n%s\n' "$newest" "$GLIBC_FLOOR" | sort -V | tail -1)" = "$GLIBC_FLOOR" ] \
        || die "$asset needs glibc $newest, above the $GLIBC_FLOOR floor"
      log "$asset needs glibc $newest at most (floor $GLIBC_FLOOR)"
      ;;
    windows)
      headers "$dist/$asset" pe "$cpu"
      local wine=""
      for candidate in /usr/lib/wine/wine64 wine64 wine; do
        command -v "$candidate" >/dev/null && { wine=$candidate; break; }
      done
      if [ -n "$wine" ]; then
        shown=$(WINEDEBUG=-all WINEPREFIX=$work/wine "$wine" "$dist/$asset" --version 2>&1 | tr -d '\r' | tail -1) || true
        [ "$shown" = "machinestream-mcp $version" ] || die "$wine printed [$shown]"
        log "ran it under Wine: $shown"
      else
        log "$asset was not run here; its headers were checked"
      fi
      ;;
    macos)
      headers "$dist/$asset" macho "$cpu" "$MACOSX_DEPLOYMENT_TARGET"
      ;;
  esac
  rm -rf "$tools"
  log "$asset $(sha256sum "$dist/$asset" | cut -d' ' -f1) OK"
}

[ $# -gt 0 ] || die "name an asset, or all, or sums (assets: ${ASSETS[*]})"
case "$1" in
  all)
    for asset in "${ASSETS[@]}"; do (build "$asset"); done
    write_sums
    ;;
  sums) write_sums ;;
  *) for asset in "$@"; do (build "$asset"); done ;;
esac
