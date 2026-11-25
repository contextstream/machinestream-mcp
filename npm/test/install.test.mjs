// The install scripts: the asset names agree everywhere, and install.sh installs, upgrades
// and refuses a download that does not match its checksum.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { mkdtemp, readFile, readdir, stat } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { artifactFor } from "../launcher.mjs";

const root = fileURLToPath(new URL("../../", import.meta.url));
const installSh = path.join(root, "install.sh");
const unix = process.platform !== "win32";

const EXPECTED = [
  "machinestream-mcp-linux-arm64",
  "machinestream-mcp-linux-x64",
  "machinestream-mcp-macos-arm64",
  "machinestream-mcp-macos-x64",
  "machinestream-mcp-windows-x64.exe",
];

function dryRun(env) {
  const result = spawnSync("sh", [installSh], {
    env: { PATH: process.env.PATH, HOME: "/home/someone", MACHINESTREAM_DRY_RUN: "1", ...env },
    encoding: "utf8",
  });
  return result;
}

test("the release build, the launcher and both install scripts name the same five assets", { skip: !unix }, async () => {
  const build = await readFile(path.join(root, "scripts/release/build.sh"), "utf8");
  const list = /^ASSETS=\(\n([\s\S]*?)\n\)$/m.exec(build)[1];
  const built = list.split("\n").map((line) => line.trim()).sort();
  assert.deepEqual(built, EXPECTED);
  for (const asset of EXPECTED) assert.ok(build.includes(`    ${asset})`), `build.sh builds ${asset}`);

  const launched = [
    ["linux", "x64"], ["linux", "arm64"], ["darwin", "x64"], ["darwin", "arm64"], ["win32", "x64"], ["win32", "arm64"],
  ].map(([platform, arch]) => artifactFor(platform, arch));
  assert.deepEqual([...new Set(launched)].sort(), EXPECTED);

  const fromSh = [
    ["Linux", "x86_64"], ["Linux", "aarch64"], ["Darwin", "x86_64"], ["Darwin", "arm64"],
  ].map(([os, arch]) => {
    const out = dryRun({ MACHINESTREAM_OS: os, MACHINESTREAM_ARCH: arch }).stdout;
    return /Download\s+\S+\/(machinestream-mcp-[a-z0-9-]+)\n/.exec(out)[1];
  });
  const ps1 = await readFile(path.join(root, "install.ps1"), "utf8");
  const fromPs1 = [...ps1.matchAll(/"\$Name-(windows-[a-z0-9]+\.exe)"/g)].map((m) => `machinestream-mcp-${m[1]}`);
  assert.deepEqual([...new Set([...fromSh, ...fromPs1])].sort(), EXPECTED);
  assert.match(build, /^SUMS=SHA256SUMS$/m);
  assert.match(ps1, /\$Sums = 'SHA256SUMS'/);
});

test("install.sh picks the latest release, or the pinned version", { skip: !unix }, () => {
  const latest = dryRun({ MACHINESTREAM_OS: "Linux", MACHINESTREAM_ARCH: "x86_64" });
  assert.equal(latest.status, 0);
  assert.match(latest.stdout, /releases\/latest\/download\/machinestream-mcp-linux-x64\n/);
  assert.match(latest.stdout, /Install\s+\/home\/someone\/\.local\/bin\/machinestream-mcp\n/);
  assert.match(latest.stdout, /Dry run: nothing was downloaded or changed\./);
  for (const version of ["1.2.3", "v1.2.3"]) {
    const pinned = dryRun({ MACHINESTREAM_OS: "Darwin", MACHINESTREAM_ARCH: "arm64", MACHINESTREAM_VERSION: version });
    assert.match(pinned.stdout, /releases\/download\/v1\.2\.3\/machinestream-mcp-macos-arm64\n/);
  }
  const windows = dryRun({ MACHINESTREAM_OS: "MINGW64_NT-10.0", MACHINESTREAM_ARCH: "x86_64" });
  assert.equal(windows.status, 1);
  assert.match(windows.stderr, /irm https:\/\/machinestream\.io\/install\.ps1 \| iex/);
  const unknown = dryRun({ MACHINESTREAM_OS: "Linux", MACHINESTREAM_ARCH: "riscv64" });
  assert.equal(unknown.status, 1);
  assert.match(unknown.stderr, /no binary for Linux on riscv64/);
});

// Not spawnSync: the release server of these tests lives in this process and must stay awake.
function run(shell, env) {
  return new Promise((resolve) => {
    const child = spawn(shell, [installSh], { env, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("close", (status) => resolve({ status, stdout, stderr }));
  });
}

function fakeBinary(version) {
  return Buffer.from(`#!/bin/sh\nif [ "$1" = "--version" ]; then echo "machinestream-mcp ${version}"; else echo "ran: $*"; fi\n`);
}

test("install.sh installs, upgrades on a rerun, and keeps nothing that fails its checksum", { skip: !unix }, async () => {
  const work = await mkdtemp(path.join(tmpdir(), "ms-install-"));
  const asset = artifactFor();
  let binary = fakeBinary("1.0.0");
  let sums = null;
  const server = createServer((request, response) => {
    if (request.url === `/${asset}`) {
      response.end(binary);
    } else if (request.url === "/SHA256SUMS") {
      response.end(sums ?? `${createHash("sha256").update(binary).digest("hex")}  ${asset}\n`);
    } else {
      response.statusCode = 404;
      response.end();
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const dir = path.join(work, "bin dir");
  const env = {
    PATH: process.env.PATH,
    HOME: work,
    MACHINESTREAM_RELEASE_BASE_URL: `http://127.0.0.1:${server.address().port}/`,
    MACHINESTREAM_INSTALL_DIR: dir,
    MACHINESTREAM_NO_SETUP: "1",
  };
  const install = (shell = "sh") => run(shell, env);
  const target = path.join(dir, "machinestream-mcp");
  try {
    const first = await install();
    assert.equal(first.status, 0, first.stderr);
    assert.match(first.stdout, /Verified\s+sha256 [0-9a-f]{16}\n/);
    assert.match(first.stdout, /Installed\s+.*machinestream-mcp \(machinestream-mcp 1\.0\.0\)/);
    assert.match(first.stdout, /is not on your PATH\. Add this line to your shell profile/);
    assert.match(first.stdout, /Next: .*machinestream-mcp setup\n$/);
    assert.deepEqual(await readFile(target), binary);
    assert.equal((await stat(target)).mode & 0o777, 0o755);

    // A rerun upgrades in place (also under bash).
    binary = fakeBinary("1.1.0");
    const second = await install("bash");
    assert.equal(second.status, 0, second.stderr);
    assert.match(second.stdout, /machinestream-mcp 1\.1\.0/);
    assert.deepEqual(await readFile(target), binary);

    // A download that does not match its checksum is refused, and what was installed stays.
    const installed = binary;
    binary = fakeBinary("6.6.6");
    sums = `${"0".repeat(64)}  ${asset}\n`;
    const tampered = await install();
    assert.equal(tampered.status, 1);
    assert.match(tampered.stderr, /did not match its checksum/);
    assert.deepEqual(await readFile(target), installed);
    assert.deepEqual(await readdir(dir), ["machinestream-mcp"], "no half-written file is left");

    sums = "";
    const unlisted = await install();
    assert.equal(unlisted.status, 1);
    assert.match(unlisted.stderr, /SHA256SUMS does not list/);

    // On the PATH already: no PATH advice.
    sums = null;
    const onPath = await run("sh", { ...env, PATH: `${dir}:${process.env.PATH}` });
    assert.equal(onPath.status, 0, onPath.stderr);
    assert.doesNotMatch(onPath.stdout, /not on your PATH/);
  } finally {
    server.close();
  }
});

test("install.sh says so when the release cannot be reached", { skip: !unix }, async () => {
  const work = await mkdtemp(path.join(tmpdir(), "ms-install-"));
  const result = spawnSync("sh", [installSh], {
    env: {
      PATH: process.env.PATH,
      HOME: work,
      MACHINESTREAM_RELEASE_BASE_URL: "http://127.0.0.1:9/",
      MACHINESTREAM_INSTALL_DIR: path.join(work, "bin"),
      MACHINESTREAM_NO_SETUP: "1",
    },
    encoding: "utf8",
  });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /machinestream-mcp was not installed: could not download/);
  await assert.rejects(stat(path.join(work, "bin", "machinestream-mcp")));
});
