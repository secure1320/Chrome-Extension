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
  | { type: "error"; code: string; message: string };

/** Commands the extension sends to the native companion. */
export type NativeCommand = { type: "start" } | { type: "stop" } | { type: "status" };

export type CompanionConnection = "connecting" | "connected" | "disconnected";

export interface Snapshot {
  connection: CompanionConnection;
  state: ListenState;
  device: string | null;
  finals: string[];
  partial: string;
  error: { code: string; message: string } | null;
  /** Tab the transcript is being typed into, if one is locked. */
  target: { title: string; problem: string | null } | null;
}

/** Popup -> service worker. */
export type PopupCommand =
  | { cmd: "start" }
  | { cmd: "stop" }
  | { cmd: "status" }
  | { cmd: "reconnect" }
  | { cmd: "clear" }
  | { cmd: "lock-target" }
  | { cmd: "unlock-target" };

/** Service worker -> popup. */
export type PopupUpdate = { kind: "snapshot"; snapshot: Snapshot };

/** Service worker -> content script in the locked frame. Mirrored in content.ts, which can't import. */
export type InserterMessage = { type: "sac-sync"; text: string; final: boolean } | { type: "sac-reset" };

export interface InserterProbe {
  editable: boolean;
  focusedAt: number;
}

export const POPUP_PORT_NAME = "popup";
