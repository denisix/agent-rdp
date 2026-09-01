/**
 * agent-rdp Node.js API
 *
 * Programmatic interface for controlling Windows Remote Desktop sessions.
 *
 * @example
 * ```typescript
 * import { RdpSession } from 'agent-rdp';
 *
 * const rdp = new RdpSession({ session: 'default' });
 *
 * await rdp.connect({
 *   host: '192.168.1.100',
 *   username: 'Administrator',
 *   password: 'secret',
 * });
 *
 * const { path, width, height } = await rdp.screenshot({ path: 'screenshot.png' });
 * await rdp.mouse.click({ x: 100, y: 200 });
 * await rdp.keyboard.type({ text: 'Hello World' });
 * await rdp.disconnect();
 * ```
 */

import * as fs from 'node:fs';
import { IpcClient } from './client.js';
import { DaemonManager } from './daemon.js';
import { AutomationController } from './automation.js';
import {
  ConnectOptions,
  ConnectResult,
  ScreenshotOptions,
  ScreenshotResult,
  ScreenshotFileResult,
  SessionInfo,
  MappedDrive,
  MouseClickOptions,
  MouseDragOptions,
  ScrollOptions,
  KeyboardTypeOptions,
  KeyboardPressOptions,
  ClipboardSetOptions,
  LocateOptions,
  LocateClickResult,
  ClickAtOptions,
  ClickAtResult,
  OcrMatch,
  Request,
  Response,
  RdpError,
} from './types.js';

// Re-export types
export * from './types.js';
export { AutomationController } from './automation.js';

export interface RdpSessionOptions {
  /** Session name (default: 'default') */
  session?: string;
  /** Request timeout in milliseconds (default: 30000) */
  timeout?: number;
  /** WebSocket streaming port (0 = disabled). Connect to ws://localhost:<port> for frames. */
  streamPort?: number;
  /**
   * Address the streaming server binds to (default: '127.0.0.1'). The stream is
   * unauthenticated and grants full control of the session, so only widen this
   * (e.g. '0.0.0.0') on a trusted network.
   */
  streamBind?: string;
  /** Streaming frame rate (default: 10). */
  streamFps?: number;
  /** Streaming JPEG quality, 0-100 (default: 80). */
  streamQuality?: number;
}

/**
 * Mouse controller for RDP sessions.
 */
export class MouseController {
  constructor(private rdp: RdpSession) {}

  /** Move cursor to position. */
  async move(options: MouseClickOptions): Promise<void> {
    await this.rdp._send({ type: 'mouse', action: 'move', x: options.x, y: options.y });
  }

  /** Left click at position. */
  async click(options: MouseClickOptions): Promise<void> {
    await this.rdp._send({ type: 'mouse', action: 'click', x: options.x, y: options.y });
  }

  /** Right click at position. */
  async rightClick(options: MouseClickOptions): Promise<void> {
    await this.rdp._send({ type: 'mouse', action: 'right_click', x: options.x, y: options.y });
  }

  /** Double click at position. */
  async doubleClick(options: MouseClickOptions): Promise<void> {
    await this.rdp._send({ type: 'mouse', action: 'double_click', x: options.x, y: options.y });
  }

  /** Drag from one position to another. */
  async drag(options: MouseDragOptions): Promise<void> {
    await this.rdp._send({
      type: 'mouse',
      action: 'drag',
      from_x: options.from.x,
      from_y: options.from.y,
      to_x: options.to.x,
      to_y: options.to.y,
    });
  }
}

/**
 * Keyboard controller for RDP sessions.
 */
export class KeyboardController {
  constructor(private rdp: RdpSession) {}

  /** Type a text string (Unicode). */
  async type(options: KeyboardTypeOptions): Promise<void> {
    await this.rdp._send({
      type: 'keyboard',
      action: 'type',
      text: options.text,
      delay_ms: options.delayMs,
    });
  }

  /** Press a key combination (e.g., 'ctrl+c', 'alt+tab') or single key (e.g., 'enter'). */
  async press(options: KeyboardPressOptions): Promise<void> {
    await this.rdp._send({ type: 'keyboard', action: 'press', keys: options.keys });
  }

  /** Press and hold a key without releasing it (for shift-click, hold-and-drag, ...). */
  async down(key: string): Promise<void> {
    await this.rdp._send({ type: 'keyboard', action: 'key_down', key });
  }

  /** Release a key previously held with `down`. */
  async up(key: string): Promise<void> {
    await this.rdp._send({ type: 'keyboard', action: 'key_up', key });
  }

  /**
   * Set the clipboard to `text` and paste it with Ctrl+V, as one command.
   *
   * More reliable than `type` for long or non-Latin text: it cannot lose
   * individual keystrokes, and setting the clipboard then pasting in one
   * daemon-side command means focus cannot move in between.
   */
  async paste(text: string): Promise<void> {
    await this.rdp._send({ type: 'keyboard', action: 'paste', text });
  }
}

/**
 * Scroll controller for RDP sessions.
 */
export class ScrollController {
  constructor(private rdp: RdpSession) {}

  /** Scroll up. */
  async up(options: ScrollOptions = {}): Promise<void> {
    await this.rdp._send({ type: 'scroll', direction: 'up', amount: options.amount ?? 3, x: options.x, y: options.y });
  }

  /** Scroll down. */
  async down(options: ScrollOptions = {}): Promise<void> {
    await this.rdp._send({ type: 'scroll', direction: 'down', amount: options.amount ?? 3, x: options.x, y: options.y });
  }

  /** Scroll left. */
  async left(options: ScrollOptions = {}): Promise<void> {
    await this.rdp._send({ type: 'scroll', direction: 'left', amount: options.amount ?? 3, x: options.x, y: options.y });
  }

  /** Scroll right. */
  async right(options: ScrollOptions = {}): Promise<void> {
    await this.rdp._send({ type: 'scroll', direction: 'right', amount: options.amount ?? 3, x: options.x, y: options.y });
  }
}

/**
 * Clipboard controller for RDP sessions.
 */
export class ClipboardController {
  constructor(private rdp: RdpSession) {}

  /** Get clipboard text. */
  async get(): Promise<string> {
    const response = await this.rdp._send({ type: 'clipboard', action: 'get' });
    const data = response.data as { type: 'clipboard'; text: string };
    return data.text;
  }

  /** Set clipboard text. */
  async set(options: ClipboardSetOptions): Promise<void> {
    await this.rdp._send({ type: 'clipboard', action: 'set', text: options.text });
  }
}

/**
 * Drive controller for RDP sessions.
 */
export class DriveController {
  constructor(private rdp: RdpSession) {}

  /** List mapped drives. */
  async list(): Promise<MappedDrive[]> {
    const response = await this.rdp._send({ type: 'drive', action: 'list' });
    const data = response.data as { type: 'drive_list'; drives: MappedDrive[] };
    return data.drives;
  }
}

/**
 * Main RDP session class.
 */
export class RdpSession {
  /** Mouse controller. */
  readonly mouse: MouseController;
  /** Keyboard controller. */
  readonly keyboard: KeyboardController;
  /** Scroll controller. */
  readonly scroll: ScrollController;
  /** Clipboard controller. */
  readonly clipboard: ClipboardController;
  /** Drive controller. */
  readonly drives: DriveController;
  /** Automation controller for Windows UI Automation. */
  readonly automation: AutomationController;

  private session: string;
  private timeout: number;
  private streamPort: number;
  private streamBind: string;
  private streamFps: number;
  private streamQuality: number;
  private daemon: DaemonManager;
  private client: IpcClient | null = null;

  constructor(options: RdpSessionOptions = {}) {
    this.session = options.session ?? 'default';
    this.timeout = options.timeout ?? 30000;
    this.streamPort = options.streamPort ?? 0;
    this.streamBind = options.streamBind ?? '127.0.0.1';
    this.streamFps = options.streamFps ?? 10;
    this.streamQuality = options.streamQuality ?? 80;
    this.daemon = new DaemonManager(this.session, this.streamPort);

    this.mouse = new MouseController(this);
    this.keyboard = new KeyboardController(this);
    this.scroll = new ScrollController(this);
    this.clipboard = new ClipboardController(this);
    this.drives = new DriveController(this);
    this.automation = new AutomationController(this);
  }

  /**
   * Connect to an RDP server.
   *
   * @param options Connection options
   * @param options.host Server hostname or IP
   * @param options.port Server port (default: 3389)
   * @param options.username Username for authentication
   * @param options.password Password for authentication
   * @param options.domain Optional domain
   * @param options.width Desktop width (default: 1280)
   * @param options.height Desktop height (default: 800)
   * @param options.drives Drives to map
   * @param options.enableWinAutomation Enable Windows UI Automation
   */
  async connect(options: ConnectOptions): Promise<ConnectResult> {
    // Ensure daemon is running and connect
    this.client = await this.daemon.ensureRunning();

    const request: Request = {
      type: 'connect',
      host: options.host,
      port: options.port ?? 3389,
      username: options.username,
      password: options.password,
      domain: options.domain,
      width: options.width ?? 1280,
      height: options.height ?? 800,
      drives: options.drives ?? [],
      enable_win_automation: options.enableWinAutomation ?? false,
      stream_port: this.streamPort,
      stream_bind: this.streamBind,
      stream_fps: this.streamFps,
      stream_quality: this.streamQuality,
      // Serve the HTML viewer whenever streaming is on, matching the CLI.
      serve_viewer: this.streamPort > 0,
    };

    const response = await this._send(request);
    const data = response.data as { type: 'connected'; host: string; width: number; height: number };

    return {
      host: data.host,
      width: data.width,
      height: data.height,
    };
  }

  /**
   * Take a screenshot.
   *
   * Pass `path` to write the image to disk and get back just the path and
   * dimensions, instead of a base64 string. Prefer `path` when the caller
   * (e.g. an AI agent) doesn't need the raw bytes in memory — returning
   * base64 to a caller that echoes results into an LLM context can burn a
   * large number of tokens for a single screenshot.
   */
  async screenshot(options: ScreenshotOptions & { path: string }): Promise<ScreenshotFileResult>;
  async screenshot(options?: ScreenshotOptions): Promise<ScreenshotResult>;
  async screenshot(
    options: ScreenshotOptions = {},
  ): Promise<ScreenshotResult | ScreenshotFileResult> {
    const response = await this._send({
      type: 'screenshot',
      format: options.format ?? 'png',
      ...(options.region ? { region: options.region } : {}),
    });

    const data = response.data as {
      type: 'screenshot';
      width: number;
      height: number;
      format: string;
      base64: string;
      offset_x?: number;
      offset_y?: number;
      frame_age_ms: number;
      frame_seq: number;
      frame_hash: string;
    };

    // A full-desktop capture has no offset; report 0 rather than undefined so
    // callers can add it unconditionally.
    const offsetX = data.offset_x ?? 0;
    const offsetY = data.offset_y ?? 0;

    if (options.path) {
      fs.writeFileSync(options.path, Buffer.from(data.base64, 'base64'));
      return {
        path: options.path,
        width: data.width,
        height: data.height,
        format: data.format,
        offsetX,
        offsetY,
        frameAgeMs: data.frame_age_ms,
        frameSeq: data.frame_seq,
        frameHash: data.frame_hash,
      };
    }

    return {
      base64: data.base64,
      width: data.width,
      height: data.height,
      format: data.format,
      offsetX,
      offsetY,
      frameAgeMs: data.frame_age_ms,
      frameSeq: data.frame_seq,
      frameHash: data.frame_hash,
    };
  }

  /**
   * Wait for the given number of milliseconds.
   *
   * A plain client-side sleep - no daemon round-trip. Prefer
   * `locate({ text, waitMs, ... })` when you are actually waiting on
   * something appearing on screen: it blocks server-side and returns as soon
   * as the text shows up, instead of guessing a fixed delay.
   */
  async wait(ms: number): Promise<void> {
    await new Promise((resolve) => setTimeout(resolve, ms));
  }

  /**
   * Get session information.
   */
  async getInfo(): Promise<SessionInfo> {
    const response = await this._send({ type: 'session_info' });
    const data = response.data as unknown as {
      type: 'session_info';
      name: string;
      state: SessionInfo['state'];
      host?: string;
      width?: number;
      height?: number;
      pid: number;
      uptime_secs: number;
    };

    return {
      name: data.name,
      state: data.state,
      host: data.host,
      width: data.width,
      height: data.height,
      pid: data.pid,
      uptime_secs: data.uptime_secs,
    };
  }

  /**
   * Locate text on screen using OCR.
   *
   * @param options Locate options
   * @param options.text Text to search for (required unless all is true)
   * @param options.all If true, returns all text on screen
   * @param options.pattern Use glob-style pattern matching (* and ?)
   * @param options.caseSensitive Case-sensitive matching (default: false)
   * @param options.region Search only part of the screen (results stay in
   *   full-screen coordinates)
   * @returns Array of matching text lines with coordinates
   *
   * Coordinates come back in screen pixels, ready to click. Never estimate a
   * coordinate by looking at a screenshot — use these, or `automation.click()`.
   *
   * @example
   * ```typescript
   * // Find lines containing text
   * const matches = await rdp.locate({ text: 'Non HDR' });
   * if (matches.length > 0) {
   *   await rdp.mouse.click({ x: matches[0].center_x, y: matches[0].center_y });
   * }
   *
   * // Pattern matching
   * const saveButtons = await rdp.locate({ text: 'Save*', pattern: true });
   *
   * // Read one table row - a tight region reads more reliably, and the
   * // coordinates are still full-screen ones
   * const row = await rdp.locate({ all: true, region: { x: 100, y: 380, width: 600, height: 30 } });
   *
   * // Get all text on screen
   * const allLines = await rdp.locate({ all: true });
   *
   * // Click a match without the coordinate passing through your code
   * await rdp.locate({ text: 'Добавить', click: 'left' });
   *
   * // Block until a dialog's text appears, instead of polling in a loop
   * const ok = await rdp.locate({ text: 'OK', waitMs: 10000, click: 'left' });
   * ```
   */
  async locate(options: LocateOptions & { click: 'left' | 'double' | 'right' }): Promise<LocateClickResult>;
  async locate(options: LocateOptions): Promise<OcrMatch[]>;
  async locate(options: LocateOptions): Promise<OcrMatch[] | LocateClickResult> {
    const response = await this._send({
      type: 'locate',
      text: options.text ?? '',
      pattern: options.pattern ?? false,
      exact: options.exact ?? false,
      ignore_case: !(options.caseSensitive ?? false),
      all: options.all ?? false,
      near_distance: options.nearDistance ?? 150,
      ...(options.region ? { region: options.region } : {}),
      ...(options.waitMs !== undefined ? { wait_ms: options.waitMs } : {}),
      ...(options.near !== undefined ? { near: options.near } : {}),
    });

    const data = response.data as { matches: OcrMatch[] };
    const matches = data.matches ?? [];

    if (!options.click) {
      return matches;
    }

    // Clicking is deliberately strict: no match, or several matches without
    // `index`, is an error rather than a guess - the wrong one of several
    // identically-named controls is worse than not clicking at all.
    if (matches.length === 0) {
      throw new RdpError(
        'invalid_request',
        `No text matching '${options.text ?? ''}' found`,
      );
    }

    let target: OcrMatch;
    if (options.index !== undefined) {
      const m = matches[options.index];
      if (!m) {
        throw new RdpError(
          'invalid_request',
          `index ${options.index} is out of range: only ${matches.length} match(es) found`,
        );
      }
      target = m;
    } else if (matches.length === 1) {
      target = matches[0]!;
    } else {
      throw new RdpError(
        'invalid_request',
        `${matches.length} matches found - pass index to choose one, or narrow the search with region`,
      );
    }

    const action =
      options.click === 'double' ? 'double_click' : options.click === 'right' ? 'right_click' : 'click';
    await this._send({ type: 'mouse', action, x: target.center_x, y: target.center_y });

    return { clicked: true, text: target.text, x: target.center_x, y: target.center_y };
  }

  /**
   * Click a known point, refusing if it's ambiguously close to more than one
   * detected text region.
   *
   * The safety net for coordinates computed outside agent-rdp - a vision
   * model reading a screenshot, a manual crop - where `locate({ click })`
   * can't be used. Uses OCR *detection* (bounding boxes) only, so it works
   * even for text OCR recognition can't read.
   *
   * Returns the result including any nearby regions; `clicked: false` with a
   * populated `nearby` array means the click was refused as ambiguous.
   *
   * @example
   * ```typescript
   * const result = await rdp.clickAt(665, 209);
   * if (!result.clicked) {
   *   console.log('Ambiguous:', result.nearby);
   * }
   * ```
   */
  async clickAt(x: number, y: number, options: ClickAtOptions = {}): Promise<ClickAtResult> {
    const response = await this._send({
      type: 'click_at',
      x,
      y,
      window_width: options.windowWidth ?? 400,
      window_height: options.windowHeight ?? 160,
      min_gap: options.minGap ?? 10,
      double_click: options.doubleClick ?? false,
      right_click: options.rightClick ?? false,
      max_divergence: options.maxDivergence ?? 40,
      ...(options.confirm ? { confirm_x: options.confirm.x, confirm_y: options.confirm.y } : {}),
    });

    const data = response.data as unknown as { type: 'click_at_result' } & ClickAtResult;
    return {
      clicked: data.clicked,
      x: data.x,
      y: data.y,
      matched_text: data.matched_text,
      nearby: data.nearby ?? [],
      divergence: data.divergence,
    };
  }

  /**
   * Disconnect from the RDP server.
   */
  async disconnect(): Promise<void> {
    await this._send({ type: 'disconnect' });
    await this.close();
  }

  /**
   * Close the IPC connection without disconnecting the RDP session.
   */
  async close(): Promise<void> {
    if (this.client) {
      await this.client.close();
      this.client = null;
    }
  }

  /**
   * Get the WebSocket streaming URL, if streaming is enabled.
   * Connect to this URL to receive JPEG frames.
   */
  getStreamUrl(): string | null {
    if (this.streamPort === 0) {
      return null;
    }
    return `ws://localhost:${this.streamPort}`;
  }

  /**
   * Internal: Send a request to the daemon.
   * @internal
   */
  async _send(request: Request): Promise<Response> {
    if (!this.client) {
      // Auto-connect to daemon if not connected
      this.client = await this.daemon.ensureRunning();
    }

    const response = await this.client.send(request, this.timeout);

    if (!response.success) {
      throw new RdpError(
        response.error?.code ?? 'internal_error',
        response.error?.message ?? 'Unknown error',
      );
    }

    return response;
  }
}
