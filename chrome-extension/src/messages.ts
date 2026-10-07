/** Messages from the native companion. It only ever sends text, status and errors. */
export type ListenState = "stopped" | "starting" | "listening" | "stopping" | "error";

export type NativeMessage =
  | { type: "started"; device: string; sampleRate: number; channels: number }
  | { type: "stopped" }
  | {
      type: "status";
      running: boolean;
      state: ListenState;
      deepgramConnected: boolean;
      device: string | null;
    }
  | { type: "transcript_partial"; text: string }
  | { type: "transcript_final"; text: string }
  | { type: "device_changed"; device: string }
  | { type: "screen_captured"; path: string | null; copied: boolean; width: number; height: number }
  | { type: "error"; code: string; message: string };

/** Commands the extension sends to the native companion. */
export type NativeCommand =
  | { type: "start" }
  | { type: "stop" }
  | { type: "status" }
  | { type: "capture_screen" };

export type CompanionConnection = "connecting" | "connected" | "disconnected";

/** Final transcript text spoken without a long pause, stamped with when it started. */
export interface TranscriptBlock {
  /** Milliseconds since the session started (first Start Listening after Clear). */
  at: number;
  text: string;
}

export interface Snapshot {
  connection: CompanionConnection;
  state: ListenState;
  device: string | null;
  blocks: TranscriptBlock[];
  partial: string;
  /** Set when the partial follows a pause and will start a new block at this time. */
  partialBlockAt: number | null;
  error: { code: string; message: string } | null;
  /** Tab the transcript is being typed into, if one is locked. */
  target: { title: string; problem: string | null } | null;
  /** Result of the last Capture Screen click. */
  capture: { busy: boolean; failed: boolean; message: string } | null;
}

/** Popup -> service worker. */
export type PopupCommand =
  | { cmd: "start" }
  | { cmd: "stop" }
  | { cmd: "status" }
  | { cmd: "reconnect" }
  | { cmd: "clear" }
  | { cmd: "lock-target" }
  | { cmd: "unlock-target" }
  | { cmd: "capture-screen" };

/** Service worker -> popup. */
export type PopupUpdate = { kind: "snapshot"; snapshot: Snapshot };

/**
 * Service worker -> content script in the locked frame. Mirrored in content.ts, which can't import.
 * sac-commit inserts catch-up text verbatim (spacing already decided by the background).
 * sac-retract deletes `count` characters before the caret (Clear while locked).
 */
export type InserterMessage =
  | { type: "sac-sync"; text: string; final: boolean }
  | { type: "sac-commit"; text: string }
  | { type: "sac-retract"; count: number }
  | { type: "sac-reset" };

export interface InserterProbe {
  editable: boolean;
  focusedAt: number;
}

export const POPUP_PORT_NAME = "popup";
