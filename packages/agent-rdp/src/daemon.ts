/**
 * Daemon process management for agent-rdp.
 */

import * as fs from 'node:fs';
import * as path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';
import { IpcClient, getSessionDir, getSocketPath } from './client.js';
import { RdpError } from './types.js';

const MAX_STARTUP_WAIT_MS = 10000;
const STARTUP_POLL_INTERVAL_MS = 100;
const PING_TIMEOUT_MS = 10000;
const SHUTDOWN_TIMEOUT_MS = 10000;
const EXIT_WAIT_MS = 5000;

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
 * Version of the binary this package would spawn (`agent-rdp X.Y.Z` -> `X.Y.Z`),
 * or null if it cannot be determined.
 *
 * Read from the binary rather than package.json: the binary is what the daemon
 * is compared against, and the two can drift when a platform package is
 * updated independently.
 */
function binaryVersion(binary: string): string | null {
  try {
    const out = spawnSync(binary, ['--version'], { encoding: 'utf8', timeout: 5000 });
    const match = (out.stdout ?? '').trim().match(/(\d+\.\d+\.\d+\S*)\s*$/);
    return match ? match[1] : null;
  } catch {
    return null;
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
    return this.runningPid() !== null;
  }

  /**
   * The daemon's pid if its pid file is valid and the process exists.
   */
  private runningPid(): number | null {
    if (!fs.existsSync(this.pidFile)) {
      return null;
    }

    try {
      const pid = parseInt(fs.readFileSync(this.pidFile, 'utf8').trim(), 10);
      // Check if process exists
      process.kill(pid, 0);
      return pid;
    } catch {
      return null;
    }
  }

  /**
   * Ensure the daemon is running, spawning it if necessary.
   * Returns an IpcClient connected to the daemon.
   *
   * A daemon that is running but was started by a different agent-rdp
   * version is replaced. The socket and pid paths depend only on the session
   * name, so after an upgrade a stale daemon would otherwise keep serving the
   * old code - including the automation scripts it embeds - indefinitely.
   */
  async ensureRunning(): Promise<IpcClient> {
    const pid = this.runningPid();
    if (pid !== null) {
      const client = new IpcClient(this.session);
      await client.connect();

      const staleVersion = await this.staleDaemonVersion(client);
      if (staleVersion === null) {
        return client;
      }
      await this.replaceStaleDaemon(client, pid, staleVersion);
    }

    await this.spawn();

    const client = new IpcClient(this.session);
    await client.connect();
    return client;
  }

  /**
   * The running daemon's version if it differs from the binary this package
   * would spawn; null when they match or the comparison is not possible.
   */
  private async staleDaemonVersion(client: IpcClient): Promise<string | null> {
    let expected: string | null;
    try {
      expected = binaryVersion(findBinary());
    } catch {
      expected = null;
    }
    if (expected === null) {
      return null;
    }

    const pong = await client.send({ type: 'ping' }, PING_TIMEOUT_MS);
    const daemonVersion = pong.data?.type === 'pong' ? (pong.data.version ?? '') : '';
    return daemonVersion === expected ? null : daemonVersion;
  }

  /**
   * Stop a version-mismatched daemon: graceful shutdown first, kill if it
   * does not exit. The daemon removes its own socket/pid files on a clean
   * exit; after a kill, the next daemon's bind replaces them.
   */
  private async replaceStaleDaemon(client: IpcClient, pid: number, staleVersion: string): Promise<void> {
    process.stderr.write(
      `agent-rdp: daemon pid ${pid} is version ${staleVersion || '<unversioned>'}, ` +
        `replacing it with the installed binary\n`,
    );
    try {
      await client.send({ type: 'shutdown' }, SHUTDOWN_TIMEOUT_MS);
    } catch {
      // Falls through to the wait/kill below.
    }
    await client.close();

    const start = Date.now();
    while (Date.now() - start < EXIT_WAIT_MS && this.isRunning()) {
      await sleep(100);
    }
    if (this.isRunning()) {
      try {
        process.kill(pid, 'SIGKILL');
      } catch {
        // Already gone.
      }
      await sleep(200);
    }
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

    // Capture the daemon's output. It runs detached, so discarding stderr means
    // a panic leaves no trace and an unexpected exit is undiagnosable.
    let out: number | 'ignore' = 'ignore';
    try {
      out = fs.openSync(path.join(this.sessionDir, 'daemon.log'), 'a');
    } catch {
      // Fall back to discarding rather than failing to start the daemon.
    }

    // Spawn daemon in background
    const child = spawn(binary, args, {
      detached: true,
      stdio: ['ignore', out, out],
      env: {
        ...process.env,
        AGENT_RDP_MODELS_DIR: process.env.AGENT_RDP_MODELS_DIR ?? findModelsDir(),
        // A daemon panic without a backtrace is a one-line mystery in daemon.log.
        RUST_BACKTRACE: process.env.RUST_BACKTRACE ?? '1',
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
