#!/usr/bin/env node

/**
 * CLI entry point for agent-rdp.
 * Resolves and executes the platform-specific binary.
 */

import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { chmodSync, existsSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);

const platform = process.platform; // darwin, linux, win32
const arch = process.arch;         // arm64, x64
const ext = platform === 'win32' ? '.exe' : '';
const platformPackage = `@denisix/agent-rdp-${platform}-${arch}`;

let binaryPath;

try {
  const packageJsonPath = require.resolve(`${platformPackage}/package.json`);
  binaryPath = join(dirname(packageJsonPath), 'bin', `agent-rdp${ext}`);
} catch {
  console.error(`Error: Platform package ${platformPackage} is not installed.`);
  console.error(`This platform (${platform}-${arch}) may not be supported.`);
  process.exit(1);
}

if (!existsSync(binaryPath)) {
  console.error(`Error: Binary not found at ${binaryPath}`);
  console.error(`The platform package ${platformPackage} may not be installed correctly.`);
  process.exit(1);
}

// npm only guarantees the executable bit for files declared in a package's
// "bin" field, and dependency install scripts are opt-in as of npm v12, so the
// platform package's postinstall chmod may never run. Restore it here.
// Non-fatal: read-only stores (pnpm, Nix, container layers) will throw.
if (process.platform !== 'win32') {
  try {
    if (!(statSync(binaryPath).mode & 0o111)) {
      chmodSync(binaryPath, 0o755);
    }
  } catch {
    // Fall through - spawnSync will surface a clearer error if it really can't run.
  }
}

// OCR models ship in this package (they're architecture-independent, so they
// aren't duplicated into each platform package). Tell the binary where to find them.
const modelsDir = join(dirname(fileURLToPath(import.meta.url)), '..', 'models');

const result = spawnSync(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  env: { ...process.env, AGENT_RDP_MODELS_DIR: process.env.AGENT_RDP_MODELS_DIR ?? modelsDir },
});

process.exit(result.status ?? 1);
