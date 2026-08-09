#!/usr/bin/env node

/**
 * Symlink the locally built release binary into ~/.local/bin (or %USERPROFILE%\bin
 * on Windows) for local debugging, without publishing to or installing from npm.
 */

const { existsSync, mkdirSync, symlinkSync, unlinkSync } = require('fs');
const { join } = require('path');
const { platform } = require('os');

const projectRoot = join(__dirname, '..');
const os = platform();
const ext = os === 'win32' ? '.exe' : '';

const sourceBinary = join(projectRoot, 'target', 'release', `agent-rdp${ext}`);

if (!existsSync(sourceBinary)) {
  console.error(`Error: Binary not found at ${sourceBinary}`);
  console.error('Run "bun run build:rust" first.');
  process.exit(1);
}

const destDir = os === 'win32'
  ? join(require('os').homedir(), 'bin')
  : join(require('os').homedir(), '.local', 'bin');
const destBinary = join(destDir, `agent-rdp${ext}`);

if (!existsSync(destDir)) {
  mkdirSync(destDir, { recursive: true });
}

if (existsSync(destBinary)) {
  unlinkSync(destBinary);
}

symlinkSync(sourceBinary, destBinary);

console.log(`Linked ${destBinary} -> ${sourceBinary}`);
if (!(process.env.PATH || '').split(os === 'win32' ? ';' : ':').includes(destDir)) {
  console.log(`Note: ${destDir} is not on your PATH. Add it to use "agent-rdp" directly.`);
}
