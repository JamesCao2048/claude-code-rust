// Stub bridge for headless integration tests.
//
// Reads STUB_SCRIPT (path) line by line; each line is one of:
//   EMIT <json-payload>   → write {"request_id":null,"event":<payload>} to stdout
//   SLEEP <ms>            → await for that many milliseconds
//   IGNORE_SHUTDOWN       → set a flag so subsequent shutdown commands are dropped
//   EXIT_NOW <code>       → process.exit(code)
//
// In parallel, stdin is read line-by-line. Every command is parsed as JSON;
// on a `shutdown` command we exit 0 unless IGNORE_SHUTDOWN was previously set.

import { readFile, appendFile } from "node:fs/promises";
import { createInterface } from "node:readline";

const scriptPath = process.env.STUB_SCRIPT;
if (!scriptPath) {
  process.stderr.write("stub_bridge: STUB_SCRIPT env var not set\n");
  process.exit(2);
}

const debugLog = process.env.STUB_BRIDGE_DEBUG_LOG;
const dbg = async (msg) => {
  if (debugLog) {
    await appendFile(debugLog, `[${Date.now()}] ${msg}\n`).catch(() => {});
  }
};

let ignoreShutdown = false;

// stdin watcher — exits process on shutdown unless IGNORE_SHUTDOWN was set.
const rl = createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  await dbg(`stdin <- ${line.slice(0,160)}`);
  let parsed;
  try {
    parsed = JSON.parse(line);
  } catch {
    return; // ignore garbage
  }
  if (parsed && parsed.command === "shutdown") {
    await dbg(`shutdown received, ignore=${ignoreShutdown}`);
    if (!ignoreShutdown) {
      process.exit(0);
    }
  }
});
rl.on("close", async () => {
  await dbg("stdin closed");
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  const text = await readFile(scriptPath, "utf8");
  await dbg(`script loaded: ${text.length} bytes, ${text.split(/\r?\n/).length} lines`);
  const lines = text.split(/\r?\n/);
  for (const raw of lines) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;

    if (line.startsWith("EMIT ")) {
      // The wire EventEnvelope flattens BridgeEvent into the same object as
      // request_id (`#[serde(flatten)]`), so we prepend `request_id` and emit
      // the payload as one flat JSON object — no nested {"event":{...}} wrapping.
      const payload = JSON.parse(line.slice("EMIT ".length));
      const envelope = { request_id: null, ...payload };
      const out = JSON.stringify(envelope) + "\n";
      process.stdout.write(out);
      await dbg(`EMIT -> ${out.slice(0, 120)}`);
    } else if (line.startsWith("SLEEP ")) {
      const ms = parseInt(line.slice("SLEEP ".length), 10);
      await sleep(ms);
    } else if (line === "IGNORE_SHUTDOWN") {
      ignoreShutdown = true;
    } else if (line.startsWith("EXIT_NOW")) {
      const parts = line.split(/\s+/);
      const code = parts.length > 1 ? parseInt(parts[1], 10) : 0;
      process.exit(code);
    } else {
      process.stderr.write(`stub_bridge: unknown directive: ${line}\n`);
      process.exit(2);
    }
  }
  // Script exhausted without EXIT_NOW: keep stdin alive so the driver can
  // either send shutdown (handled above) or hit a watchdog. If
  // IGNORE_SHUTDOWN was set, we'll be reaped via SIGKILL.
  await new Promise(() => {});
}

main().catch((e) => {
  process.stderr.write(`stub_bridge: ${e.stack || e}\n`);
  process.exit(2);
});
