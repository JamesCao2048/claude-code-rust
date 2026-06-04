import fs from "node:fs";
import path from "node:path";

import type { AvailableCommand } from "../types.js";

// The SDK surfaces three different kinds of entries through `supportedCommands()`:
// genuine slash commands (scenario personas + claude builtins), INTERNAL agents
// (e.g. `lingxi-ascendc:cannbot-ascendc-developer`), and SKILLS loaded from the
// local plugin (e.g. `lingxi-ascendc:ascendc-debug`). The product picker should
// only show genuine commands. We filter agents + skills out here rather than in
// Rust because the SDK exposes agent names (`supportedAgents()`) and the plugin
// manifest (`.claude-plugin/plugin.json -> skills`) both live on this side; Rust
// has no skills list. Filtering by "is an agent or skill" — not by hardcoded
// scenario names — means adding a new scenario command needs no code change here.

// Normalize a command / agent / skill identifier to a bare comparable name:
// strip a leading slash, drop any `<plugin>:` namespace prefix, drop a trailing
// `.md`, and lowercase. `supportedCommands()` yields `lingxi-ascendc:aog-worker`
// while `supportedAgents()` yields the bare `aog-worker`; this makes both align.
export function normalizeInternalName(value: string): string {
  let name = value.trim();
  if (name.startsWith("/")) {
    name = name.slice(1);
  }
  const colonIndex = name.lastIndexOf(":");
  if (colonIndex >= 0) {
    name = name.slice(colonIndex + 1);
  }
  if (name.toLowerCase().endsWith(".md")) {
    name = name.slice(0, -3);
  }
  return name.toLowerCase();
}

// Read the loaded plugin's skill names from `$LINGXI_RESOURCE_DIR/.claude-plugin/
// plugin.json`. Each `skills` entry is a path like `./skills/ascendc-debug`; the
// basename is the skill name that the SDK surfaces (namespaced) as a command.
// Best-effort: any failure (unset env, missing/malformed manifest) yields an
// empty set so the picker simply falls back to showing everything.
export function loadPluginSkillNames(): Set<string> {
  const skillNames = new Set<string>();
  const resourceDir = process.env.LINGXI_RESOURCE_DIR;
  if (!resourceDir) {
    return skillNames;
  }
  try {
    // Packaged layout nests the plugin under `<resource_dir>/.claude/`; prefer that,
    // fall back to a flat layout for dev resource dirs.
    const nested = path.join(resourceDir, ".claude", ".claude-plugin", "plugin.json");
    const flat = path.join(resourceDir, ".claude-plugin", "plugin.json");
    const manifestPath = fs.existsSync(nested) ? nested : flat;
    const raw = fs.readFileSync(manifestPath, "utf8");
    const manifest = JSON.parse(raw) as { skills?: unknown };
    if (Array.isArray(manifest.skills)) {
      for (const entry of manifest.skills) {
        if (typeof entry !== "string") {
          continue;
        }
        const base = path.basename(entry.trim());
        if (base) {
          skillNames.add(normalizeInternalName(base));
        }
      }
    }
  } catch {
    // Best-effort: leave the set empty on any read/parse failure.
  }
  return skillNames;
}

// Drop any command whose normalized name matches a known agent or skill, keeping
// genuine scenario commands + claude builtins.
export function filterOutAgentsAndSkills(
  commands: AvailableCommand[],
  agentNames: Iterable<string>,
  skillNames: Iterable<string>,
): AvailableCommand[] {
  const internal = new Set<string>();
  for (const name of agentNames) {
    const normalized = normalizeInternalName(name);
    if (normalized) {
      internal.add(normalized);
    }
  }
  for (const name of skillNames) {
    const normalized = normalizeInternalName(name);
    if (normalized) {
      internal.add(normalized);
    }
  }
  if (internal.size === 0) {
    return commands;
  }
  return commands.filter((command) => !internal.has(normalizeInternalName(command.name)));
}
