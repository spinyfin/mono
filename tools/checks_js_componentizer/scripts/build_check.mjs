#!/usr/bin/env node

import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";

const args = process.argv.slice(2);
const parsed = parseArgs(args);
const repoRoot = parsed.get("repo-root");
const entry = parsed.get("entry");
const out = parsed.get("out");

if (!repoRoot || !entry || !out) {
  fail("usage: build_check.mjs --repo-root <path> --entry <path> --out <path>");
}

const toolchainRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), "..");
const witPath = path.join(toolchainRoot, "wit");
const entryPath = path.resolve(entry);
const outPath = path.resolve(out);

const tempDir = await fs.mkdtemp(path.join(os.tmpdir(), "checkleft-js-build-"));
try {
  const wrapperPath = path.join(tempDir, "wrapper.mjs");
  const bundlePath = path.join(tempDir, "bundle.mjs");
  await fs.writeFile(
    wrapperPath,
    makeWrapperSource(entryPath),
    "utf8",
  );

  run(
    "corepack",
    [
      "pnpm",
      "exec",
      "esbuild",
      wrapperPath,
      "--bundle",
      "--platform=neutral",
      "--format=esm",
      `--outfile=${bundlePath}`,
      "--log-level=warning",
    ],
    toolchainRoot,
  );
  run(
    "corepack",
    [
      "pnpm",
      "exec",
      "jco",
      "componentize",
      bundlePath,
      "--wit",
      witPath,
      "--world-name",
      "check-runtime",
      "--disable",
      "all",
      "--out",
      outPath,
    ],
    toolchainRoot,
  );
} finally {
  await fs.rm(tempDir, { recursive: true, force: true });
}

function parseArgs(rawArgs) {
  const out = new Map();
  for (let i = 0; i < rawArgs.length; i += 1) {
    const key = rawArgs[i];
    const value = rawArgs[i + 1];
    if (!key.startsWith("--")) {
      continue;
    }
    out.set(key.slice(2), value);
    i += 1;
  }
  return out;
}

function makeWrapperSource(entryPath) {
  return `
import * as userModule from ${JSON.stringify(entryPath)};

const impl = userModule.run ?? userModule.check ?? userModule.default;

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function normalizeInput(inputJson) {
  const parsed = JSON.parse(typeof inputJson === "string" && inputJson.length > 0 ? inputJson : "{}");
  const root = isObject(parsed) ? parsed : {};
  const changesetSource = isObject(root.changeset) ? root.changeset : {};
  const changedFiles = Array.isArray(changesetSource.changed_files)
    ? changesetSource.changed_files
    : [];
  const config = isObject(root.config) ? root.config : {};
  const capabilities = isObject(root.capabilities) ? root.capabilities : {};
  const normalized = {
    ...root,
    changeset: {
      ...changesetSource,
      changed_files: changedFiles,
    },
    config,
    capabilities,
  };
  Object.defineProperty(normalized, "toString", {
    value: () => (typeof inputJson === "string" ? inputJson : "{}"),
    enumerable: false,
  });
  return normalized;
}

export function run(inputJson) {
  if (typeof impl !== "function") {
    throw new Error("JS check entry must export run(input) (or check/default).");
  }
  const result = impl(normalizeInput(inputJson));
  if (typeof result === "string") {
    return result;
  }
  return JSON.stringify(result ?? { findings: [] });
}
`;
}

function run(program, commandArgs, cwd) {
  const rendered = [program, ...commandArgs].join(" ");
  const output = spawnSync(program, commandArgs, {
    cwd,
    stdio: "pipe",
    encoding: "utf8",
  });
  if (output.status === 0) {
    return;
  }

  const stderr = (output.stderr ?? "").trim();
  const stdout = (output.stdout ?? "").trim();
  fail(`command failed: ${rendered}\nstdout: ${stdout}\nstderr: ${stderr}`);
}

function fail(message) {
  console.error(message);
  process.exit(1);
}
