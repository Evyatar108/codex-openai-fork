#!/usr/bin/env node
// Unified entry point for the Codex CLI.

import { spawn } from "node:child_process";
import { existsSync } from "fs";
import { createRequire } from "node:module";
import path from "path";
import { fileURLToPath } from "url";

// __dirname equivalent in ESM
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const require = createRequire(import.meta.url);

const PLATFORM_PACKAGE_BY_TARGET = {
  "x86_64-unknown-linux-musl": "@gim-home/codex-linux-x64",
  "aarch64-unknown-linux-musl": "@gim-home/codex-linux-arm64",
  "x86_64-apple-darwin": "@gim-home/codex-darwin-x64",
  "aarch64-apple-darwin": "@gim-home/codex-darwin-arm64",
  "x86_64-pc-windows-msvc": "@gim-home/codex-win32-x64",
  "aarch64-pc-windows-msvc": "@gim-home/codex-win32-arm64",
};

const { platform, arch } = process;

if (platform !== "win32" || arch !== "x64") {
  throw new Error(
    "@gim-home/codex currently supports Windows x64 only. Linux/macOS support is planned.",
  );
}

const targetTriple = "x86_64-pc-windows-msvc";

const platformPackage = PLATFORM_PACKAGE_BY_TARGET[targetTriple];
if (!platformPackage) {
  throw new Error(`Unsupported target triple: ${targetTriple}`);
}

const codexBinaryName = "codex.exe";
const localVendorRoot = path.join(__dirname, "..", "vendor");
const localBinaryPath = path.join(
  localVendorRoot,
  targetTriple,
  "codex",
  codexBinaryName,
);

let vendorRoot;
try {
  const packageJsonPath = require.resolve(`${platformPackage}/package.json`);
  vendorRoot = path.join(path.dirname(packageJsonPath), "vendor");
} catch {
  if (existsSync(localBinaryPath)) {
    vendorRoot = localVendorRoot;
  } else {
    throw new Error(
      `Missing optional dependency ${platformPackage}. Reinstall Codex: npm install -g @gim-home/codex@latest`,
    );
  }
}

if (!vendorRoot) {
  throw new Error(
    `Missing optional dependency ${platformPackage}. Reinstall Codex: npm install -g @gim-home/codex@latest`,
  );
}

const archRoot = path.join(vendorRoot, targetTriple);
const binaryPath = path.join(archRoot, "codex", codexBinaryName);

function getUpdatedPath(newDirs) {
  const pathSep = process.platform === "win32" ? ";" : ":";
  const existingPath = process.env.PATH || "";
  const updatedPath = [
    ...newDirs,
    ...existingPath.split(pathSep).filter(Boolean),
  ].join(pathSep);
  return updatedPath;
}

const additionalDirs = [];
const pathDir = path.join(archRoot, "path");
if (existsSync(pathDir)) {
  additionalDirs.push(pathDir);
}
const updatedPath = getUpdatedPath(additionalDirs);

const env = { ...process.env, PATH: updatedPath, CODEX_MANAGED_BY_NPM: "1" };

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  env,
});

child.on("error", (err) => {
  // eslint-disable-next-line no-console
  console.error(err);
  process.exit(1);
});

const forwardSignal = (signal) => {
  if (child.killed) {
    return;
  }
  try {
    child.kill(signal);
  } catch {
    /* ignore */
  }
};

["SIGINT", "SIGTERM", "SIGHUP"].forEach((sig) => {
  process.on(sig, () => forwardSignal(sig));
});

const childResult = await new Promise((resolve) => {
  child.on("exit", (code, signal) => {
    if (signal) {
      resolve({ type: "signal", signal });
    } else {
      resolve({ type: "code", exitCode: code ?? 1 });
    }
  });
});

if (childResult.type === "signal") {
  process.kill(process.pid, childResult.signal);
} else {
  process.exit(childResult.exitCode);
}
