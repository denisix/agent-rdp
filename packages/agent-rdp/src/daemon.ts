/**
 * Daemon process management for agent-rdp.
 */

import * as fs from 'node:fs';
import * as path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { IpcClient, getSessionDir, getSocketPath } from './client.js';
import { RdpError } from './types.js';

const MAX_STARTUP_WAIT_MS = 10000;
const STARTUP_POLL_INTERVAL_MS = 100;

// Create require function for resolving platform package paths in ESM
const require = createRequire(import.meta.url);

/**
 * Locate the OCR models bundled in this package.
 *
 * Models are architecture-independent, so they ship here once rather than
 * being duplicated into every platform package.
 */
function findModelsDir(): string {
  // dist/daemon.js -> package root
  return path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'models');
}

/**
 * Get the platform package name for the current OS/arch.
 */
function getPlatformPackage(): string {
  const platform = process.platform; // 'darwin', 'linux', 'win32'
  const arch = process.arch; // 'arm64', 'x64'
  return `@denisixnpm/agent-rdp-${platform}-${arch}`;
}

/**
 * Find the agent-rdp binary.
 */
function findBinary(): string {
  const platformPackage = getPlatformPackage();
  const ext = process.platform === 'win32' ? '.exe' : '';

  try {
    // Resolve the platform package's package.json, then find the binary
    const packageJsonPath = require.resolve(`${platformPackage}/package.json`);
    const packageDir = path.dirname(packageJsonPath);
    const binaryPath = path.join(packageDir, 'bin', `agent-rdp${ext}`);

    if (!fs.existsSync(binaryPath)) {
      throw new RdpError(
        'internal_error',
        `Binary not found at ${binaryPath}. Platform package ${platformPackage} may not be installed correctly.`
      );
    }

    // npm only guarantees the executable bit for files declared in a package's
    // "bin" field, and dependency install scripts are opt-in as of npm v12, so
    // the platform package's postinstall chmod may never run. Restore it here.
    // Non-fatal: read-only stores (pnpm, Nix, container layers) will throw.
    if (process.platform !== 'win32') {
      try {
        if (!(fs.statSync(binaryPath).mode & 0o111)) {
          fs.chmodSync(binaryPath, 0o755);
        }
      } catch {
        // Fall through - spawning will surface a clearer error if it really can't run.
      }
    }

    return binaryPath;
  } catch (err) {
    if (err instanceof RdpError) {
      throw err;
    }
    throw new RdpError(
      'not_supported',
      `Platform package ${platformPackage} is not installed. ` +
        `Make sure you have the correct optional dependency installed for your platform.`
    );
  }
}

/**
 * Manages the daemon lifecycle for a session.
 */
export class DaemonManager {
  private sessionDir: string;
  private pidFile: string;

  constructor(
    private session: string,
    private streamPort: number = 0,
  ) {
    this.sessionDir = getSessionDir(session);
    this.pidFile = path.join(this.sessionDir, 'pid');
  }

  /**
   * Check if the daemon is running.
   */
  isRunning(): boolean {
    if (!fs.existsSync(this.pidFile)) {
      return false;
    }

    try {
      const pid = parseInt(fs.readFileSync(this.pidFile, 'utf8').trim(), 10);
      // Check if process exists
      process.kill(pid, 0);
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Ensure the daemon is running, spawning it if necessary.
   * Returns an IpcClient connected to the daemon.
   */
  async ensureRunning(): Promise<IpcClient> {
    if (!this.isRunning()) {
      await this.spawn();
    }

    const client = new IpcClient(this.session);
    await client.connect();
    return client;
  }

  /**
   * Spawn the daemon process.
   */
  private async spawn(): Promise<void> {
    const binary = findBinary();

    // Ensure session directory exists
    fs.mkdirSync(this.sessionDir, { recursive: true });

    // Build daemon arguments
    const args = ['--session', this.session];
    if (this.streamPort > 0) {
      args.push('--stream-port', this.streamPort.toString());
    }
    args.push('session', 'daemon');

    // Spawn daemon in background
    const child = spawn(binary, args, {
      detached: true,
      stdio: 'ignore',
      env: {
        ...process.env,
        AGENT_RDP_MODELS_DIR: process.env.AGENT_RDP_MODELS_DIR ?? findModelsDir(),
      },
    });

    child.unref();

    // Wait for daemon to be ready (socket file exists or TCP port responds)
    const socketPath = getSocketPath(this.session);
    const startTime = Date.now();

    while (Date.now() - startTime < MAX_STARTUP_WAIT_MS) {
      if (typeof socketPath === 'number') {
        // Windows: try TCP connection
        try {
          const client = new IpcClient(this.session);
          await client.connect();
          await client.close();
          return;
        } catch {
          // Not ready yet
        }
      } else {
        // Unix: check if socket file exists
        if (fs.existsSync(socketPath)) {
          // Give it a moment to be ready
          await sleep(50);
          return;
        }
      }

      await sleep(STARTUP_POLL_INTERVAL_MS);
    }

    throw new RdpError('daemon_not_running', 'Daemon failed to start within timeout');
  }

  /**
   * Get the socket path for this session.
   */
  getSocketPath(): string | number {
    return getSocketPath(this.session);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
