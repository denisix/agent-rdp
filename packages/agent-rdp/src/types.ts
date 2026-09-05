/**
 * TypeScript types for agent-rdp.
 *
 * IPC types are auto-generated from Rust (see generated/).
 * SDK convenience types are defined here.
 */

// --- Re-export generated IPC types ---
// These are auto-generated from agent-rdp-protocol via ts-rs.
// Run `cargo test -p agent-rdp-protocol --lib` to regenerate.

export type {
  // Request types
  Request,
  ConnectRequest,
  ScreenshotRequest,
  MouseRequest,
  KeyboardRequest,
  ScrollRequest,
  ClipboardRequest,
  DriveRequest,
  LocateRequest,
  ClickAtRequest,
  AutomateRequest,

  // Response types
  Response,
  ResponseData,
  ErrorCode,
  ErrorInfo,
  SessionInfo,
  SessionSummary,
  MappedDrive,
  FilePushRequest,
  FilePullRequest,
  FileTransferResult,
  LocateResult,
  ClickAtResult,
  OcrMatch,

  // Supporting types
  DriveMapping,
  ImageFormat,
  MouseButton,
  ScrollDirection,
  ConnectionState,

  // Automation types
  AccessibilityElement,
  AccessibilitySnapshot,
  AutomationStatus,
  AutomationScrollDirection,
  AutomationHandshake,
  ClickResult,
  RunResult,
  ElementBounds,
  ElementValue,
  WindowInfo,
  WindowAction,
  WaitState,

  // File IPC types (daemon <-> PowerShell)
  FileIpcRequest,
  FileIpcResponse,
  FileIpcError,
} from './generated/index.js';

// --- SDK Convenience Types ---
// These are higher-level types for the SDK API, not IPC.

import type { DriveMapping, ErrorCode } from './generated/index.js';

/** Options for connecting to an RDP session. */
export interface ConnectOptions {
  host: string;
  port?: number;
  username: string;
  password: string;
  domain?: string;
  width?: number;
  height?: number;
  drives?: DriveMapping[];
  /** Enable Windows UI Automation. */
  enableWinAutomation?: boolean;
  /**
   * Seconds between keep-alive PDUs; 0 disables them (default: 45).
   *
   * An idle RDP session sends nothing in either direction, so a NAT or
   * firewall on the path drops it after its own idle timeout - and
   * recovering from that costs a reconnect, which relaunches the automation
   * agent by typing Win+R on the remote desktop.
   */
  keepAliveSecs?: number;
}

/** Result of a successful connection. */
export interface ConnectResult {
  host: string;
  width: number;
  height: number;
}

/**
 * A rectangular part of the screen, in screen pixels.
 *
 * Coordinates a region request reports back are always translated into
 * full-screen space, so they can be passed straight to `mouse.click()`.
 */
export interface ScreenRegion {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** Options for taking a screenshot. */
export interface ScreenshotOptions {
  format?: 'png' | 'jpeg';
  /** Capture only this part of the screen instead of the whole desktop. */
  region?: ScreenRegion;
  /**
   * Write the image to this file path instead of returning it as base64.
   * Use this when the caller doesn't need the raw image bytes in memory
   * (e.g. an AI agent that only needs the path/dimensions) — it avoids
   * materializing a large base64 string that would otherwise burn a lot
   * of tokens if echoed back into an LLM context.
   */
  path?: string;
}

/** Result of a screenshot operation saved to disk (see `ScreenshotOptions.path`). */
export interface ScreenshotFileResult {
  path: string;
  width: number;
  height: number;
  format: string;
  /** X offset of the image within the full desktop (0 unless `region` was used). */
  offsetX: number;
  /** Y offset of the image within the full desktop (0 unless `region` was used). */
  offsetY: number;
  /** Milliseconds since the RDP server last sent any data. */
  frameAgeMs: number;
  /**
   * Framebuffer generation counter at capture time. Two screenshots with the
   * same `frameSeq` are guaranteed pixel-identical - a sequence that never
   * advances across an action that must have changed the screen means the
   * frame is stale, not that the desktop is merely idle.
   */
  frameSeq: number;
  /** FNV-1a 64-bit hash (16 hex digits) of the captured pixels. */
  frameHash: string;
}

/** Result of a screenshot operation returned as base64. */
export interface ScreenshotResult {
  base64: string;
  width: number;
  height: number;
  format: string;
  /** X offset of the image within the full desktop (0 unless `region` was used). */
  offsetX: number;
  /** Y offset of the image within the full desktop (0 unless `region` was used). */
  offsetY: number;
  /** Milliseconds since the RDP server last sent any data. */
  frameAgeMs: number;
  /** See `ScreenshotFileResult.frameSeq`. */
  frameSeq: number;
  /** FNV-1a 64-bit hash (16 hex digits) of the captured pixels. */
  frameHash: string;
}

/** A point representing x,y coordinates. */
export interface Point {
  x: number;
  y: number;
}

/** Options for mouse click operations. */
export interface MouseClickOptions {
  x: number;
  y: number;
}

/** Options for mouse drag operations. */
export interface MouseDragOptions {
  from: Point;
  to: Point;
}

/** Options for scroll operations. */
export interface ScrollOptions {
  /** Amount to scroll (default: 3). */
  amount?: number;
  /** X coordinate (optional). */
  x?: number;
  /** Y coordinate (optional). */
  y?: number;
}

/** Options for keyboard type operations. */
export interface KeyboardTypeOptions {
  /** Text to type. */
  text: string;
  /**
   * Pause in milliseconds between batches of characters. Only needed for remote
   * applications that drop input arriving too quickly; omitted sends as fast as
   * the connection allows.
   */
  delayMs?: number;
}

/** Options for keyboard press operations. */
export interface KeyboardPressOptions {
  /** Key combination (e.g., 'ctrl+c') or single key (e.g., 'enter'). */
  keys: string;
}

/** Options for clipboard set operations. */
export interface ClipboardSetOptions {
  /** Text to set. */
  text: string;
}

/** Options for locate (OCR) operations. */
export interface LocateOptions {
  /** Text to search for. Required unless all is true. */
  text?: string;
  /** If true, returns all text on screen. */
  all?: boolean;
  /** Use glob-style pattern matching (* and ?). */
  pattern?: boolean;
  /**
   * Require the whole OCR line to equal the search text, not just contain
   * it. Default substring mode matches "Провести" against a line reading
   * "Провести и закрыть"; `exact` avoids that ambiguity. Takes precedence
   * over `pattern`.
   */
  exact?: boolean;
  /** Case-sensitive matching (default: false). */
  caseSensitive?: boolean;
  /**
   * Search only this part of the screen. A tight region also reads more
   * reliably than a full-screen pass. Match coordinates stay in full-screen
   * space.
   */
  region?: ScreenRegion;
  /**
   * Keep retrying until the text appears, up to this many milliseconds.
   * Blocks server-side instead of polling `locate` in a loop from the
   * outside. Ignored when `all` is true - there is no target text to wait
   * for.
   */
  waitMs?: number;
  /**
   * Click the match instead of just returning its position. Never estimate
   * a coordinate by reading a screenshot - use this, or `automation.click()`.
   */
  click?: 'left' | 'double' | 'right';
  /**
   * Which match to click when several are found (0-based). Required
   * whenever `click` is set and more than one match comes back - clicking
   * the wrong one of several identically-named controls is worse than not
   * clicking at all, so an ambiguous match throws rather than guessing.
   */
  index?: number;
  /**
   * Constrain matches to those within `nearDistance` px of a line containing
   * this anchor text (substring match). Useful when the same text appears in
   * several places (a repeated column header, a label and its tooltip) -
   * anchor to a nearby, more distinctive label instead. If the anchor itself
   * isn't found, the result is zero matches, not an error.
   */
  near?: string;
  /** Max distance in pixels from the `near` anchor (default: 150). */
  nearDistance?: number;
}

/** Result of `locate()` when `click` is set. */
export interface LocateClickResult {
  clicked: true;
  /** Text of the match that was clicked. */
  text: string;
  x: number;
  y: number;
}

/** Options for `clickAt()`. */
export interface ClickAtOptions {
  /** OCR detection window width around the point (default: 400). */
  windowWidth?: number;
  /** OCR detection window height around the point (default: 160). */
  windowHeight?: number;
  /**
   * Refuse the click if another detected text region is within this many
   * pixels of the target (default: 10).
   */
  minGap?: number;
  /** Double-click instead of single click. */
  doubleClick?: boolean;
  /** Right-click instead of left click. */
  rightClick?: boolean;
  /**
   * A second, independently measured point for the same target (e.g. a
   * vision model queried twice). If the two points agree within
   * `maxDivergence`, their midpoint is clicked instead of the first point
   * alone; if they diverge, the click is refused rather than picking one
   * arbitrarily.
   */
  confirm?: Point;
  /**
   * Max pixel distance between the point and `confirm` before it's treated
   * as diverging measurements rather than noise (default: 40).
   */
  maxDivergence?: number;
}

// --- Automation convenience types (aliases for backwards compatibility) ---

/** Bounds for automation elements (alias for ElementBounds). */
export type { ElementBounds as AutomationElementBounds } from './generated/index.js';

/** Automation element (alias for AccessibilityElement). */
export type { AccessibilityElement as AutomationElement } from './generated/index.js';

/** Automation snapshot result. */
export type { AccessibilitySnapshot as AutomationSnapshot } from './generated/index.js';

/** Element value result (alias for ElementValue). */
export type { ElementValue as AutomationElementValue } from './generated/index.js';

/** Window info (alias for WindowInfo). */
export type { WindowInfo as AutomationWindowInfo } from './generated/index.js';

/** Run command result (alias for RunResult). */
export type { RunResult as AutomationRunResult } from './generated/index.js';

/** Incremental run-poll result (alias for RunPollResult). */
export type { RunPollResult as AutomationRunPollResult } from './generated/index.js';

/** Click result (alias for ClickResult). */
export type { ClickResult as AutomationClickResult } from './generated/index.js';

// --- Error Class ---

/** Error class for RDP operations. */
export class RdpError extends Error {
  constructor(
    public code: ErrorCode,
    message: string,
  ) {
    super(message);
    this.name = 'RdpError';
  }
}
