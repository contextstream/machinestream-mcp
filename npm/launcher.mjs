// The npm launcher: finds (or downloads once) the machinestream-mcp binary for this
// platform and runs it with the caller's arguments and streams.
//
// The binary comes from this version's GitHub release and is checked against the release's
// SHA256SUMS before it is kept. MACHINESTREAM_MCP_BINARY names a binary to run instead
// (a local build, or one installed another way).

import { createHash, randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { chmod, mkdir, readFile, rename, stat, unlink, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const packagePath = fileURLToPath(new URL("../package.json", import.meta.url));
export const PACKAGE_VERSION = JSON.parse(await readFile(packagePath, "utf8")).version;

export const RELEASE_ROOT = "https://github.com/contextstream/machinestream-mcp/releases/download/";
export const CHECKSUMS_FILE = "SHA256SUMS";

const MAX_DOWNLOAD_BYTES = 200 * 1024 * 1024;
const FETCH_TIMEOUT_MS = 60_000;

const ARTIFACTS = Object.freeze({
  "linux-x64": "machinestream-mcp-linux-x64",
  "linux-arm64": "machinestream-mcp-linux-arm64",
  "darwin-x64": "machinestream-mcp-macos-x64",
  "darwin-arm64": "machinestream-mcp-macos-arm64",
  "win32-x64": "machinestream-mcp-windows-x64.exe",
  // Windows on ARM runs the x64 binary.
  "win32-arm64": "machinestream-mcp-windows-x64.exe",
});

/** The release file for a platform, or an error that names the platform. */
export function artifactFor(platform = process.platform, arch = process.arch) {
  const artifact = ARTIFACTS[`${platform}-${arch}`];
  if (!artifact) {
    throw new Error(
      `machinestream-mcp ${PACKAGE_VERSION} has no binary for ${platform}-${arch}. ` +
        "Build it from source (cargo build --release) and set MACHINESTREAM_MCP_BINARY to its path.",
    );
  }
  return artifact;
}

/** Where this version's release files are. Only https, or http to this machine for tests. */
export function releaseBase(version = PACKAGE_VERSION, env = process.env) {
  const override = env.MACHINESTREAM_MCP_RELEASE_BASE_URL;
  const base = new URL(override ? (override.endsWith("/") ? override : `${override}/`) : `v${version}/`, RELEASE_ROOT);
  const local = base.protocol === "http:" && ["127.0.0.1", "localhost", "[::1]"].includes(base.hostname);
  if (base.protocol !== "https:" && !local) {
    throw new Error("the release address must use https");
  }
  if (base.username || base.password) {
    throw new Error("the release address must not hold credentials");
  }
  return base;
}

export function cacheDir(env = process.env, platform = process.platform) {
  if (env.MACHINESTREAM_CACHE_DIR) {
    return path.join(path.resolve(env.MACHINESTREAM_CACHE_DIR), "bin");
  }
  if (platform === "win32" && env.LOCALAPPDATA) {
    return path.join(env.LOCALAPPDATA, "machinestream", "bin");
  }
  return path.join(homedir(), ".cache", "machinestream", "bin");
}

/** `SHA256SUMS` as a map from file name to lowercase hex digest. */
export function parseChecksums(text) {
  const sums = new Map();
  for (const line of text.split(/\r?\n/)) {
    const match = /^([0-9a-fA-F]{64})\s+\*?(\S+)$/.exec(line.trim());
    if (match) {
      sums.set(path.posix.basename(match[2]), match[1].toLowerCase());
    }
  }
  return sums;
}

async function download(url, maxBytes = MAX_DOWNLOAD_BYTES) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
  try {
    const response = await fetch(url, {
      signal: controller.signal,
      headers: { "user-agent": `machinestream-mcp-launcher/${PACKAGE_VERSION}` },
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status} for ${url}`);
    }
    const chunks = [];
    let total = 0;
    for await (const chunk of response.body) {
      total += chunk.length;
      if (total > maxBytes) {
        throw new Error(`${url} is larger than ${maxBytes} bytes`);
      }
      chunks.push(Buffer.from(chunk));
    }
    return Buffer.concat(chunks, total);
  } finally {
    clearTimeout(timer);
  }
}

async function isFile(file) {
  try {
    return (await stat(file)).isFile();
  } catch {
    return false;
  }
}

/** The path of the binary to run, downloading and checking it when it is not cached. */
export async function resolveBinary(env = process.env, platform = process.platform, arch = process.arch) {
  const override = env.MACHINESTREAM_MCP_BINARY;
  if (override) {
    if (!(await isFile(override))) {
      throw new Error(`MACHINESTREAM_MCP_BINARY names no file: ${override}`);
    }
    return path.resolve(override);
  }
  const artifact = artifactFor(platform, arch);
  const folder = path.join(cacheDir(env, platform), PACKAGE_VERSION);
  const target = path.join(folder, artifact);
  if (await isFile(target)) {
    return target;
  }
  const base = releaseBase(PACKAGE_VERSION, env);
  const [sumsText, binary] = await Promise.all([
    download(new URL(CHECKSUMS_FILE, base), 1024 * 1024).then((buffer) => buffer.toString("utf8")),
    download(new URL(artifact, base)),
  ]);
  const expected = parseChecksums(sumsText).get(artifact);
  if (!expected) {
    throw new Error(`${CHECKSUMS_FILE} of version ${PACKAGE_VERSION} does not list ${artifact}`);
  }
  const actual = createHash("sha256").update(binary).digest("hex");
  if (actual !== expected) {
    throw new Error(`${artifact} did not match its checksum and was not kept`);
  }
  await mkdir(folder, { recursive: true });
  // Written beside the target and renamed, so a half-written file is never run.
  const temporary = path.join(folder, `.${artifact}.${randomUUID()}.tmp`);
  try {
    await writeFile(temporary, binary, { mode: 0o755 });
    await chmod(temporary, 0o755);
    await rename(temporary, target);
  } catch (error) {
    await unlink(temporary).catch(() => {});
    throw error;
  }
  return target;
}

/** Run the binary with `args` and this process's streams. Resolves with its exit code. */
export async function run(args = process.argv.slice(2), env = process.env) {
  const binary = await resolveBinary(env);
  return new Promise((resolve, reject) => {
    const child = spawn(binary, args, {
      stdio: "inherit",
      env: { ...env, MACHINESTREAM_MCP_LAUNCHER: "npx" },
      windowsHide: true,
    });
    for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
      process.on(signal, () => child.kill(signal));
    }
    child.on("error", reject);
    child.on("exit", (code, signal) => resolve(signal ? 128 : (code ?? 1)));
  });
}
