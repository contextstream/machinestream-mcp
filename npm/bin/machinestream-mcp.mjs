#!/usr/bin/env node
import { run } from "../launcher.mjs";

try {
  process.exitCode = await run();
} catch (error) {
  // stderr only: in stdio mode stdout carries the protocol.
  process.stderr.write(`machinestream-mcp: ${error.message}\n`);
  process.exitCode = 1;
}
