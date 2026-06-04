// Top-level-agent write-deny guard (G-SHARED-WRITE), wired as a PreToolUse hook
// passed directly to query() — NOT a plugin `hooks` declaration (the SDK does not
// honor plugin-manifest hooks) and NOT canUseTool (skipped under bypassPermissions).
// Reuses the packaged Python guard (engine.hooks.shared_write_guard via the shell
// wrapper) so the deny rules live in one place. Fail-open on any error so a guard
// problem never wedges the session.
import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

import type { HookCallback, HookInput } from "@anthropic-ai/claude-agent-sdk";

// Resolve the packaged guard script. Packaged layout nests resources under
// `<LINGXI_RESOURCE_DIR>/.claude/`; fall back to a flat layout for dev dirs.
function resolveGuardScript(): string | undefined {
  const dir = process.env.LINGXI_RESOURCE_DIR?.trim();
  if (!dir) {
    return undefined;
  }
  const candidates = [
    path.join(dir, ".claude", "hooks", "deny-shared-knowledge-write.sh"),
    path.join(dir, "hooks", "deny-shared-knowledge-write.sh"),
  ];
  return candidates.find((p) => fs.existsSync(p));
}

async function runGuard(script: string, payload: string): Promise<string> {
  return new Promise<string>((resolve) => {
    try {
      const child = spawn("bash", [script], { stdio: ["pipe", "pipe", "ignore"] });
      let stdout = "";
      child.stdout.on("data", (d) => {
        stdout += d.toString();
      });
      child.on("error", () => resolve(""));
      child.on("close", () => resolve(stdout.trim()));
      child.stdin.on("error", () => {});
      child.stdin.write(payload);
      child.stdin.end();
    } catch {
      resolve("");
    }
  });
}

// HookCallback for PreToolUse. Spawns the guard with the tool call; if the guard
// emits a deny payload (the SDK PreToolUse output contract), return it; else allow.
export function makeSharedWriteDenyHook(): HookCallback {
  return (async (input: HookInput) => {
    const script = resolveGuardScript();
    if (!script) {
      return { continue: true };
    }
    const anyInput = input as { tool_name?: unknown; tool_input?: unknown };
    const payload = JSON.stringify({
      tool_name: anyInput.tool_name,
      tool_input: anyInput.tool_input,
    });
    const out = await runGuard(script, payload);
    if (!out) {
      return { continue: true };
    }
    try {
      return JSON.parse(out) as ReturnType<HookCallback> extends Promise<infer R> ? R : never;
    } catch {
      return { continue: true };
    }
  }) as HookCallback;
}
