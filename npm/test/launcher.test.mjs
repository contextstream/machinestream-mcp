import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { chmod, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  PACKAGE_VERSION,
  artifactFor,
  cacheDir,
  parseChecksums,
  releaseBase,
  resolveBinary,
} from "../launcher.mjs";

const entry = fileURLToPath(new URL("../bin/machinestream-mcp.mjs", import.meta.url));

test("every published platform has an artifact", () => {
  assert.equal(artifactFor("linux", "x64"), "machinestream-mcp-linux-x64");
  assert.equal(artifactFor("linux", "arm64"), "machinestream-mcp-linux-arm64");
  assert.equal(artifactFor("darwin", "x64"), "machinestream-mcp-macos-x64");
  assert.equal(artifactFor("darwin", "arm64"), "machinestream-mcp-macos-arm64");
  assert.equal(artifactFor("win32", "x64"), "machinestream-mcp-windows-x64.exe");
  assert.equal(artifactFor("win32", "arm64"), "machinestream-mcp-windows-x64.exe");
  assert.throws(() => artifactFor("sunos", "sparc"), /no binary for sunos-sparc/);
});

test("the release address is this version's, and must be https", () => {
  assert.equal(
    releaseBase("1.2.3", {}).href,
    "https://github.com/contextstream/machinestream-mcp/releases/download/v1.2.3/",
  );
  assert.equal(
    releaseBase("1.2.3", { MACHINESTREAM_MCP_RELEASE_BASE_URL: "https://mirror.example.test/files" }).href,
    "https://mirror.example.test/files/",
  );
  assert.throws(
    () => releaseBase("1.2.3", { MACHINESTREAM_MCP_RELEASE_BASE_URL: "http://mirror.example.test/" }),
    /must use https/,
  );
  assert.throws(
    () => releaseBase("1.2.3", { MACHINESTREAM_MCP_RELEASE_BASE_URL: "https://user:pass@mirror.example.test/" }),
    /must not hold credentials/,
  );
});

test("checksums are read in the sha256sum format", () => {
  const sums = parseChecksums(
    `${"a".repeat(64)}  machinestream-mcp-linux-x64\n${"B".repeat(64)} *dist/machinestream-mcp-windows-x64.exe\nnot a line\n`,
  );
  assert.equal(sums.get("machinestream-mcp-linux-x64"), "a".repeat(64));
  assert.equal(sums.get("machinestream-mcp-windows-x64.exe"), "b".repeat(64));
  assert.equal(sums.size, 2);
});

test("the cache follows MACHINESTREAM_CACHE_DIR", () => {
  assert.equal(cacheDir({ MACHINESTREAM_CACHE_DIR: "/tmp/ms-cache" }, "linux"), "/tmp/ms-cache/bin");
  assert.match(cacheDir({}, "linux"), /\.cache[\\/]machinestream[\\/]bin$/);
});

async function fakeBinary(folder, name, text) {
  const file = path.join(folder, name);
  await writeFile(file, `#!/bin/sh\necho "${text} $*"\nexit 7\n`);
  await chmod(file, 0o755);
  return file;
}

test("MACHINESTREAM_MCP_BINARY runs a local binary with the arguments and exit status", { skip: process.platform === "win32" }, async () => {
  const folder = await mkdtemp(path.join(tmpdir(), "ms-launcher-"));
  const binary = await fakeBinary(folder, "local-build", "local");
  const result = spawnSync(process.execPath, [entry, "status", "--json"], {
    env: { ...process.env, MACHINESTREAM_MCP_BINARY: binary },
    encoding: "utf8",
  });
  assert.equal(result.stdout, "local status --json\n");
  assert.equal(result.status, 7);

  const missing = spawnSync(process.execPath, [entry], {
    env: { ...process.env, MACHINESTREAM_MCP_BINARY: path.join(folder, "absent") },
    encoding: "utf8",
  });
  assert.equal(missing.status, 1);
  assert.equal(missing.stdout, "", "nothing but protocol ever goes to stdout");
  assert.match(missing.stderr, /names no file/);
});

test("a binary is downloaded once, checked against its checksum, and cached", { skip: process.platform === "win32" }, async () => {
  const folder = await mkdtemp(path.join(tmpdir(), "ms-launcher-"));
  const artifact = artifactFor();
  const source = await readFile(await fakeBinary(folder, "published", "downloaded"));
  const digest = createHash("sha256").update(source).digest("hex");
  let sums = `${digest}  ${artifact}\n`;
  let requests = 0;
  const server = createServer((request, response) => {
    requests += 1;
    if (request.url === "/SHA256SUMS") {
      response.end(sums);
    } else if (request.url === `/${artifact}`) {
      response.end(source);
    } else {
      response.statusCode = 404;
      response.end();
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}/`;
  try {
    const env = { MACHINESTREAM_CACHE_DIR: path.join(folder, "cache"), MACHINESTREAM_MCP_RELEASE_BASE_URL: base };
    const first = await resolveBinary(env);
    assert.equal(first, path.join(folder, "cache", "bin", PACKAGE_VERSION, artifact));
    assert.deepEqual(await readFile(first), source);
    assert.equal(requests, 2);
    assert.equal(await resolveBinary(env), first);
    assert.equal(requests, 2, "the second start uses the cached binary");

    // A binary that does not match its checksum is never kept.
    sums = `${"0".repeat(64)}  ${artifact}\n`;
    const other = { ...env, MACHINESTREAM_CACHE_DIR: path.join(folder, "cache-2") };
    await assert.rejects(resolveBinary(other), /did not match its checksum/);
    sums = "";
    await assert.rejects(resolveBinary(other), /does not list/);
  } finally {
    server.close();
  }
});
